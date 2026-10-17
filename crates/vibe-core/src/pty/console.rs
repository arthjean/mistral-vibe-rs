//! What a pseudoconsole says to the terminal hosting it, kept apart from what
//! its child writes.
//!
//! ConPTY is a terminal of its own that renders the child into VT and talks to
//! a hosting terminal emulator. `portable-pty` creates it with
//! `PSEUDOCONSOLE_INHERIT_CURSOR` and `PSEUDOCONSOLE_WIN32_INPUT_MODE`, so the
//! console opens by asking that terminal for the cursor position (`ESC [ 6 n`)
//! and holds the child's output until it is answered, then asks for win32 input
//! and focus reports (`ESC [ ? 9001 h`, `ESC [ ? 1004 h`), resets the colors,
//! titles the window after the program and shows the cursor, and switches both
//! modes off again as it closes. No terminal emulator sits behind a session, so
//! the reader answers the request and drops that exchange: what reaches the
//! session is what the child wrote, as a POSIX terminal hands it over.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

#[cfg(test)]
mod console_tests;

/// The startup request ConPTY holds the child's output behind, and the answer
/// a fresh console expects: the top-left corner.
const CURSOR_POSITION_REQUEST: &[u8] = b"\x1b[6n";
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// The modes the console asks its host for and releases, wherever they appear.
const HOST_MODES: [&[u8]; 4] = [
    b"\x1b[?9001h",
    b"\x1b[?9001l",
    b"\x1b[?1004h",
    b"\x1b[?1004l",
];

/// What the console paints before the child's first byte: a color reset and a
/// visible cursor. The window title is the third part of that prologue.
const PROLOGUE: [&[u8]; 2] = [b"\x1b[m", b"\x1b[?25h"];

/// The longest escape sequence held back while it is incomplete; anything
/// longer is the child's own and passes through as written.
const MAX_SEQUENCE: usize = 4096;

/// The writing half of a pseudoconsole, shared with the reader that answers
/// the console on the terminal's behalf.
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// The two halves of a pseudoconsole as a session reads and writes them.
///
/// Off Windows both come back unchanged, since a POSIX terminal neither asks
/// nor paints anything of its own. On Windows the reader answers the cursor
/// request through the writer and drops the console's exchange with its host.
///
/// The console's input stays open until both halves are gone, so dropping the
/// writer handed back only stops the session writing. Closing the input pipe
/// delivers no end of file to a ConPTY child: the console host reads it as its
/// own end, and on Windows Server 2022 a pseudoconsole whose input closed
/// before it did never returns from `ClosePseudoConsole`.
#[must_use]
pub fn pseudoconsole_streams(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
    if !cfg!(windows) {
        return (reader, writer);
    }
    let shared: SharedWriter = Arc::new(Mutex::new(writer));
    (
        Box::new(ConsoleReader::new(reader, Arc::clone(&shared))),
        Box::new(ConsoleWriter(shared)),
    )
}

/// Reads the child's output out of what the console writes.
struct ConsoleReader {
    inner: Box<dyn Read + Send>,
    writer: SharedWriter,
    /// An escape sequence begun but not yet complete, held across reads.
    pending: Vec<u8>,
    /// What was kept and not yet handed to a caller.
    ready: VecDeque<u8>,
    /// Whether the child has written anything yet, which ends the prologue.
    started: bool,
}

impl ConsoleReader {
    fn new(inner: Box<dyn Read + Send>, writer: SharedWriter) -> Self {
        Self {
            inner,
            writer,
            pending: Vec::new(),
            ready: VecDeque::new(),
            started: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                } else {
                    self.started = true;
                    self.ready.push_back(byte);
                }
                continue;
            }
            self.pending.push(byte);
            if sequence_complete(&self.pending) {
                let sequence = std::mem::take(&mut self.pending);
                self.settle(&sequence);
            } else if self.pending.len() > MAX_SEQUENCE {
                self.started = true;
                self.ready.extend(self.pending.drain(..));
            }
        }
    }

    /// Answers, drops or keeps one complete escape sequence.
    fn settle(&mut self, sequence: &[u8]) {
        if sequence == CURSOR_POSITION_REQUEST {
            self.answer();
            return;
        }
        if HOST_MODES.contains(&sequence) {
            return;
        }
        let title = sequence.starts_with(b"\x1b]0;");
        if !self.started && (title || PROLOGUE.contains(&sequence)) {
            return;
        }
        self.started = true;
        self.ready.extend(sequence);
    }

    fn answer(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer
                .write_all(CURSOR_POSITION_REPORT)
                .and_then(|()| writer.flush());
        }
    }
}

/// Whether `sequence`, which starts with `ESC`, is complete: a CSI ends on its
/// final byte, an OSC on `BEL` or `ESC \`, and any other escape after one byte.
fn sequence_complete(sequence: &[u8]) -> bool {
    match sequence.get(1) {
        None => false,
        Some(b'[') => {
            sequence.len() > 2
                && sequence
                    .last()
                    .is_some_and(|byte| (0x40..=0x7e).contains(byte))
        }
        Some(b']') => {
            sequence.ends_with(b"\x07") || sequence.len() > 3 && sequence.ends_with(b"\x1b\\")
        }
        Some(_) => true,
    }
}

impl Read for ConsoleReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut chunk = vec![0_u8; buffer.len().max(1)];
        while self.ready.is_empty() {
            let read = self.inner.read(&mut chunk)?;
            if read == 0 {
                // The console is gone: whatever was held back was the child's.
                self.ready.extend(self.pending.drain(..));
                break;
            }
            self.feed(chunk.get(..read).unwrap_or_default());
        }
        let count = buffer.len().min(self.ready.len());
        for (slot, byte) in buffer.iter_mut().zip(self.ready.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }
}

struct ConsoleWriter(SharedWriter);

impl Write for ConsoleWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("the terminal writer lock is poisoned"))?
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut writer) => writer.flush(),
            Err(_) => Err(std::io::Error::other(
                "the terminal writer lock is poisoned",
            )),
        }
    }
}
