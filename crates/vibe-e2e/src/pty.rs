//! A world child on a pseudo-terminal (portable-pty), read into a terminal
//! screen model (vt100), so a TUI scenario asserts on what the user sees.
//!
//! The driver plays the terminal: it answers the cursor position report and
//! primary device attributes requests a client sends at startup, records the
//! window titles an OSC 0 or 2 sequence sets, and encodes keys the way xterm
//! sends them without keyboard enhancements, which it never acknowledges. At
//! most [`LIVE_PTYS`] sessions are live at once in one process; a trial that
//! opens another waits for a slot.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::process::ChildRecord;
use crate::world::World;
use crate::{Failure, Outcome, scaled, tail};

/// The most PTY sessions one process keeps live at once.
pub const LIVE_PTYS: usize = 2;

/// How much raw output a failed wait quotes.
const QUOTED: usize = 8 * 1024;

/// How long a wait goes on after the child exited, for its last output.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

/// What the driver answers a primary device attributes request with: a VT220
/// with ANSI color, the class xterm reports.
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";

/// The queries the driver answers, longest first so `ESC [ 0 c` is not read as
/// `ESC [ c` followed by text.
const QUERIES: [&[u8]; 3] = [b"\x1b[6n", b"\x1b[0c", b"\x1b[c"];

struct Slots {
    live: Mutex<(usize, usize)>,
    freed: Condvar,
}

static SLOTS: Slots = Slots {
    live: Mutex::new((0, 0)),
    freed: Condvar::new(),
};

impl Slots {
    fn lock(&self) -> MutexGuard<'_, (usize, usize)> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One of the [`LIVE_PTYS`] slots, given back when dropped.
struct Slot;

impl Slot {
    fn take() -> Self {
        let mut live = SLOTS.lock();
        while live.0 >= LIVE_PTYS {
            live = SLOTS
                .freed
                .wait(live)
                .unwrap_or_else(PoisonError::into_inner);
        }
        live.0 += 1;
        live.1 = live.1.max(live.0);
        Self
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        SLOTS.lock().0 -= 1;
        SLOTS.freed.notify_one();
    }
}

/// The most PTY sessions this process has had live at once.
#[cfg(all(test, unix))]
pub(crate) fn peak_live() -> usize {
    SLOTS.lock().1
}

/// A key or input a scenario sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// Characters typed one after another.
    Text(String),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    /// A control chord such as Ctrl+C (`Key::Ctrl('c')`).
    Ctrl(char),
    /// Pasted text, inside bracketed paste markers while the client has the
    /// mode on, as a terminal sends it.
    Paste(String),
}

impl Key {
    /// The bytes xterm sends for the key, in normal or application cursor
    /// mode, with bracketed paste on or off.
    #[must_use]
    pub fn bytes(&self, application_cursor: bool, bracketed_paste: bool) -> Vec<u8> {
        let arrow = |letter: u8| {
            if application_cursor {
                vec![0x1b, b'O', letter]
            } else {
                vec![0x1b, b'[', letter]
            }
        };
        match self {
            Self::Text(text) => text.as_bytes().to_vec(),
            Self::Enter => b"\r".to_vec(),
            Self::Esc => b"\x1b".to_vec(),
            Self::Tab => b"\t".to_vec(),
            Self::BackTab => b"\x1b[Z".to_vec(),
            Self::Backspace => b"\x7f".to_vec(),
            Self::Up => arrow(b'A'),
            Self::Down => arrow(b'B'),
            Self::Right => arrow(b'C'),
            Self::Left => arrow(b'D'),
            Self::Ctrl(letter) => {
                let byte = u8::try_from(letter.to_ascii_lowercase()).unwrap_or(b'@');
                vec![byte & 0x1f]
            }
            Self::Paste(text) if !bracketed_paste => text.as_bytes().to_vec(),
            Self::Paste(text) => {
                let mut bytes = b"\x1b[200~".to_vec();
                bytes.extend_from_slice(text.as_bytes());
                bytes.extend_from_slice(b"\x1b[201~");
                bytes
            }
        }
    }
}

/// A snapshot of the terminal screen.
#[derive(Clone)]
pub struct Screen(vt100::Screen);

impl std::fmt::Debug for Screen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text())
    }
}

impl Screen {
    /// The visible text, one line per row, trailing blanks trimmed.
    #[must_use]
    pub fn text(&self) -> String {
        self.rows().join("\n")
    }

    /// The visible rows, trailing blanks trimmed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        let (_, cols) = self.0.size();
        self.0
            .rows(0, cols.get())
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    /// Whether `needle` appears on one row.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.rows().iter().any(|row| row.contains(needle))
    }

    /// The row and column (from 0) where `needle` first appears.
    #[must_use]
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        self.rows().iter().enumerate().find_map(|(row, text)| {
            let byte = text.find(needle)?;
            let column = text[..byte].chars().count();
            Some((u16::try_from(row).ok()?, u16::try_from(column).ok()?))
        })
    }

    /// The cell at `row` and `col` (from 0), with its attributes.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&vt100::Cell> {
        self.0.cell(row, col)
    }

    /// The cursor's row and column, from 0.
    #[must_use]
    pub fn cursor(&self) -> (u16, u16) {
        self.0.cursor_position()
    }

    /// Columns and rows.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.0.size();
        (cols.get(), rows.get())
    }

    /// Whether the client switched to the alternate screen.
    #[must_use]
    pub fn alternate(&self) -> bool {
        self.0.alternate_screen()
    }

    /// Whether the client enabled bracketed paste.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.0.bracketed_paste()
    }

    /// Whether the client switched the arrows to application cursor mode.
    #[must_use]
    pub fn application_cursor(&self) -> bool {
        self.0.application_cursor()
    }
}

/// Records the titles OSC 0 and 2 set.
#[derive(Default)]
struct Titles(Vec<String>);

impl vt100::Callbacks for Titles {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.0.push(String::from_utf8_lossy(title).into_owned());
    }
}

struct Term {
    parser: vt100::Parser<Titles>,
    raw: Arc<Mutex<Vec<u8>>>,
    /// The last bytes of the previous read, so a query split across two reads
    /// is still found.
    carry: Vec<u8>,
    closed: bool,
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

struct Shared {
    term: Mutex<Term>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn size(cols: u16, rows: u16) -> Outcome<(NonZeroU16, NonZeroU16)> {
    match (NonZeroU16::new(cols), NonZeroU16::new(rows)) {
        (Some(cols), Some(rows)) => Ok((cols, rows)),
        _ => Err(Failure(format!(
            "a terminal of {cols}x{rows} cells has no cell"
        ))),
    }
}

/// How the child on the terminal ended.
#[derive(Debug, Clone)]
pub struct TuiExit {
    pub code: u32,
    pub signal: Option<String>,
    pub success: bool,
}

impl std::fmt::Display for TuiExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.signal {
            Some(signal) => write!(formatter, "terminated by {signal}"),
            None => write!(formatter, "exit code {}", self.code),
        }
    }
}

/// A world child running on a pseudo-terminal.
pub struct Tui {
    label: String,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Writer,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    exit: Option<(TuiExit, Instant)>,
    _slot: Slot,
}

impl Tui {
    pub(crate) fn spawn(
        world: &World,
        program: &Path,
        label: String,
        args: &[OsString],
        env: Vec<(String, String)>,
        cwd: &Path,
        (cols, rows): (u16, u16),
    ) -> Outcome<Self> {
        let (width, height) = size(cols, rows)?;
        let slot = Slot::take();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| Failure(format!("cannot open a PTY: {error}")))?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.env_clear();
        for (name, value) in &env {
            command.env(name, value);
        }
        command.cwd(cwd);
        let child = pair.slave.spawn_command(command).map_err(|error| {
            Failure(format!(
                "cannot start {label} ({}) on a PTY: {error}",
                program.display()
            ))
        })?;
        // The child holds the only slave descriptor left, so the reader sees
        // the end of the stream once the child and its descendants let go.
        drop(pair.slave);
        let master = pair.master;
        let reader = master
            .try_clone_reader()
            .map_err(|error| Failure(format!("cannot read the PTY: {error}")))?;
        let writer: Writer =
            Arc::new(Mutex::new(master.take_writer().map_err(|error| {
                Failure(format!("cannot write the PTY: {error}"))
            })?));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Shared {
            term: Mutex::new(Term {
                parser: vt100::Parser::new_with_callbacks(height, width, 0, Titles::default()),
                raw: Arc::clone(&raw),
                carry: Vec::new(),
                closed: false,
            }),
            changed: Condvar::new(),
        });
        world.register(ChildRecord {
            label: label.clone(),
            pid: child.process_id().unwrap_or_default(),
            env,
            stdout: raw,
            stderr: Arc::default(),
        });
        let reading = Arc::clone(&shared);
        let answering = Arc::clone(&writer);
        let reader = thread::Builder::new()
            .name("e2e-pty-reader".to_owned())
            .spawn(move || read(reader, &reading, &answering))?;
        Ok(Self {
            label,
            child,
            master,
            writer,
            shared,
            reader: Some(reader),
            exit: None,
            _slot: slot,
        })
    }

    /// The current screen.
    #[must_use]
    pub fn screen(&self) -> Screen {
        Screen(self.shared.lock().parser.screen().clone())
    }

    /// Every window title the client set, in order.
    #[must_use]
    pub fn titles(&self) -> Vec<String> {
        self.shared.lock().parser.callbacks().0.clone()
    }

    /// Every byte the client wrote so far.
    #[must_use]
    pub fn raw(&self) -> Vec<u8> {
        let raw = Arc::clone(&self.shared.lock().raw);
        raw.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// How many bytes the client wrote so far.
    #[must_use]
    pub fn bytes_written(&self) -> usize {
        let raw = Arc::clone(&self.shared.lock().raw);
        raw.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Sends keys in order. An Esc is followed by a pause, so the next key is
    /// not read as an Alt chord.
    pub fn send(&mut self, keys: &[Key]) -> Outcome {
        for key in keys {
            let screen = self.screen();
            let bytes = key.bytes(screen.application_cursor(), screen.bracketed_paste());
            self.write_raw(&bytes)?;
            if *key == Key::Esc {
                thread::sleep(Duration::from_millis(60));
            }
        }
        Ok(())
    }

    /// Types `text`.
    pub fn type_text(&mut self, text: &str) -> Outcome {
        self.send(&[Key::Text(text.to_owned())])
    }

    /// Types `text`, then Enter.
    pub fn submit(&mut self, text: &str) -> Outcome {
        self.send(&[Key::Text(text.to_owned()), Key::Enter])
    }

    /// Writes bytes to the terminal as typed.
    pub fn write_raw(&mut self, bytes: &[u8]) -> Outcome {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resizes the terminal and the screen model; the client receives
    /// SIGWINCH.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Outcome {
        let (width, height) = size(cols, rows)?;
        let mut term = self.shared.lock();
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| Failure(format!("cannot resize the PTY: {error}")))?;
        term.parser.set_size(height, width);
        Ok(())
    }

    /// The exit, once the child has exited.
    fn exited(&mut self) -> Option<(TuiExit, Instant)> {
        if self.exit.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            let exit = TuiExit {
                code: status.exit_code(),
                signal: status.signal().map(str::to_owned),
                success: status.success(),
            };
            self.exit = Some((exit, Instant::now()));
        }
        self.exit.clone()
    }

    fn quoted(&self) -> String {
        format!(
            "final screen:\n{}\nlast {QUOTED} bytes written to the terminal:\n{:?}",
            self.screen().text(),
            tail(&self.raw(), QUOTED)
        )
    }

    /// Waits until the screen satisfies `predicate`, and returns it. The trial
    /// fails with the final screen and the last raw bytes when `deadline`
    /// (scaled) expires, or soon after the child exits.
    pub fn wait_screen(
        &mut self,
        what: &str,
        predicate: impl Fn(&Screen) -> bool,
        deadline: Duration,
    ) -> Outcome<Screen> {
        let deadline = scaled(deadline);
        let until = Instant::now() + deadline;
        loop {
            let screen = self.screen();
            if predicate(&screen) {
                return Ok(screen);
            }
            if let Some((exit, at)) = self.exited() {
                let closed = self.shared.lock().closed;
                if closed || at.elapsed() >= EXIT_DRAIN {
                    let screen = self.screen();
                    if predicate(&screen) {
                        return Ok(screen);
                    }
                    return Err(Failure(format!(
                        "{} exited ({exit}) before the screen showed {what}; {}",
                        self.label,
                        self.quoted()
                    )));
                }
            }
            if Instant::now() >= until {
                let failure = Failure(format!(
                    "{} did not show {what} within {deadline:?}; {}",
                    self.label,
                    self.quoted()
                ));
                self.kill();
                return Err(failure);
            }
            let term = self.shared.lock();
            let _ = self
                .shared
                .changed
                .wait_timeout(term, Duration::from_millis(50));
        }
    }

    /// Waits until `text` appears on one row.
    pub fn wait_text(&mut self, text: &str, deadline: Duration) -> Outcome<Screen> {
        self.wait_screen(
            &format!("{text:?}"),
            |screen| screen.contains(text),
            deadline,
        )
    }

    /// Waits for the child to exit within `deadline` (scaled), then for the
    /// rest of its output.
    pub fn wait_exit(&mut self, deadline: Duration) -> Outcome<TuiExit> {
        let deadline = scaled(deadline);
        let until = Instant::now() + deadline;
        loop {
            if let Some((exit, at)) = self.exited() {
                while !self.shared.lock().closed && at.elapsed() < EXIT_DRAIN {
                    thread::sleep(Duration::from_millis(10));
                }
                return Ok(exit);
            }
            if Instant::now() >= until {
                let failure = Failure(format!(
                    "{} did not exit within {deadline:?}; {}",
                    self.label,
                    self.quoted()
                ));
                self.kill();
                return Err(failure);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ends the child's process group now.
    pub fn kill(&mut self) {
        if self.exited().is_some() {
            return;
        }
        #[cfg(unix)]
        if let Some(group) = self
            .child
            .process_id()
            .and_then(|id| i32::try_from(id).ok())
        {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            let group = Pid::from_raw(group);
            let _ = killpg(group, Signal::SIGTERM);
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until && self.exited().is_none() {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = killpg(group, Signal::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.exited();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        // Killing first matters: dropping the writer sends an end of file
        // to a child still reading the terminal.
        self.kill();
        if let Some(reader) = self.reader.take() {
            // A grandchild holding the terminal keeps the reader blocked, so
            // it is left running rather than joined.
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

/// Reads the terminal until it closes, feeding the screen model and
/// answering queries at the point of the stream where they arrive.
fn read(mut reader: Box<dyn Read + Send>, shared: &Shared, writer: &Writer) {
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let chunk = &buffer[..count];
        let mut term = shared.lock();
        term.raw
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(chunk);
        let carried = term.carry.len();
        let mut window = std::mem::take(&mut term.carry);
        window.extend_from_slice(chunk);
        let mut processed = 0;
        let mut position = 0;
        while position < window.len() {
            let Some(query) = QUERIES
                .iter()
                .find(|query| window[position..].starts_with(query))
            else {
                position += 1;
                continue;
            };
            let end = position + query.len();
            if end <= carried {
                // Answered when the previous read ended with it.
                position += 1;
                continue;
            }
            // A query that began in the carried bytes ends in this chunk.
            let chunk_end = end.saturating_sub(carried);
            if chunk_end > processed {
                term.parser.process(&chunk[processed..chunk_end]);
                processed = chunk_end;
            }
            let answer = if *query == b"\x1b[6n" {
                let (row, col) = term.parser.screen().cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
            } else {
                DEVICE_ATTRIBUTES.to_vec()
            };
            if let Ok(mut writer) = writer.lock() {
                let _ = writer.write_all(&answer);
                let _ = writer.flush();
            }
            position = end;
        }
        term.parser.process(&chunk[processed..]);
        let keep = window.len().min(3);
        term.carry = window[window.len() - keep..].to_vec();
        drop(term);
        shared.changed.notify_all();
    }
    shared.lock().closed = true;
    shared.changed.notify_all();
}

#[cfg(test)]
mod pty_tests;
