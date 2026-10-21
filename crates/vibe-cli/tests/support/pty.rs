//! The pseudo-terminal harness the TUI tests drive `vibe` through.
//!
//! Unix only, like the binaries that compile it: the terminal opens with
//! `nix::pty::openpty`, which Windows lacks.

#![allow(
    dead_code,
    reason = "every PTY test binary compiles this module and uses a part of it"
)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::{Termios, tcgetattr};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use vibe_core::events::ModelMessage;
use vibe_core::storage::SessionStore;

#[path = "test_root.rs"]
mod test_root;

/// Replays a terminal transcript onto a sparse screen so assertions read what
/// the operator sees. Only cursor positioning and erasure move content; every
/// other escape is display state the text assertions do not depend on.
pub fn visible_text(transcript: &[u8]) -> String {
    let text = String::from_utf8_lossy(transcript);
    let mut screen: BTreeMap<usize, BTreeMap<usize, char>> = BTreeMap::new();
    let (mut row, mut column) = (1usize, 1usize);
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    for byte in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&byte) {
                            final_byte = Some(byte);
                            break;
                        }
                        parameters.push(byte);
                    }
                    let numbers = parameters
                        .trim_start_matches('?')
                        .split(';')
                        .map(|value| value.parse::<usize>().unwrap_or(0))
                        .collect::<Vec<_>>();
                    match final_byte {
                        Some('H' | 'f') => {
                            row = numbers
                                .first()
                                .copied()
                                .filter(|value| *value > 0)
                                .unwrap_or(1);
                            column = numbers
                                .get(1)
                                .copied()
                                .filter(|value| *value > 0)
                                .unwrap_or(1);
                        }
                        Some('J') => {
                            if numbers.first().copied().unwrap_or(0) >= 2 {
                                screen.clear();
                            }
                        }
                        Some('K') => {
                            if let Some(line) = screen.get_mut(&row) {
                                line.retain(|position, _| *position < column);
                            }
                        }
                        _ => {}
                    }
                }
                Some(']') => {
                    for byte in characters.by_ref() {
                        if byte == '\u{7}' || byte == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => {
                row = row.saturating_add(1);
                column = 1;
            }
            '\r' => column = 1,
            character if character.is_control() => {}
            character => {
                screen.entry(row).or_default().insert(column, character);
                column = column.saturating_add(1);
            }
        }
    }
    screen
        .values()
        .map(|line| {
            let mut rendered = String::new();
            let mut expected = 1usize;
            for (position, character) in line {
                for _ in expected..*position {
                    rendered.push(' ');
                }
                rendered.push(*character);
                expected = position.saturating_add(1);
            }
            rendered
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the composer carries this word alone on one of its lines.
///
/// The prompt marker owns the first two cells and a continuation line leaves
/// them blank, while a frame repaints either the whole line or only the cells it
/// changed, so both spellings describe the same screen. What the two forms have
/// in common is the column, which is what separates the composer from anything
/// else on screen that happens to contain the word.
pub fn composer_carries(screen: &str, word: &str) -> bool {
    screen
        .lines()
        .map(str::trim_end)
        .any(|line| line == format!("> {word}") || line == format!("  {word}"))
}

pub struct PtyProcess {
    child: Child,
    master: File,
    /// A slave descriptor kept to read the terminal's attributes. It holds the
    /// reader off its end of file, so it is closed before the reader joins.
    terminal: File,
    /// The attributes the terminal had before the client started.
    initial_termios: Termios,
    receiver: Receiver<Vec<u8>>,
    reader: JoinHandle<Vec<u8>>,
    /// Every byte read so far, which replays to the whole current screen.
    screen: Vec<u8>,
    /// How much of `screen` an earlier wait has matched. One read can carry
    /// the bytes a wait looked for and the frame after them, so the next wait
    /// starts from this mark rather than from the next chunk.
    consumed: usize,
}

/// How long one step of a PTY exchange may take before the test gives up.
///
/// These are deadlines, not sleeps: a step the client answers immediately costs
/// nothing, so the budget is set for the slowest machine that runs this suite
/// rather than for the fastest one. Every wait shares it, so the suite's
/// tolerance for a loaded machine is one decision instead of thirty.
pub const STEP: Duration = Duration::from_secs(20);

impl PtyProcess {
    pub fn spawn(working_directory: &Path, vibe_home: &Path, arguments: &[&str]) -> Self {
        Self::spawn_with_environment(working_directory, vibe_home, arguments, &[])
    }

    /// The same launch with the shared defaults overridden, which is how the
    /// update tests point the gateway at a loopback fixture instead of the
    /// unreachable port every other test uses.
    pub fn spawn_with_environment(
        working_directory: &Path,
        vibe_home: &Path,
        arguments: &[&str],
        environment: &[(&str, String)],
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vibe"));
        command.args(arguments);
        Self::spawn_command_with(working_directory, vibe_home, command, environment)
    }

    pub fn spawn_piped_prompt(working_directory: &Path, vibe_home: &Path, prompt: &str) -> Self {
        let mut command = Command::new("setsid");
        command
            .args([
                "--ctty",
                "--wait",
                "sh",
                "-c",
                "printf '%s\\n' \"$VIBE_TEST_PROMPT\" | \"$VIBE_TEST_BIN\" --trust --api-base http://127.0.0.1:9/v1",
            ])
            .env("VIBE_TEST_PROMPT", prompt)
            .env("VIBE_TEST_BIN", env!("CARGO_BIN_EXE_vibe"));
        Self::spawn_command(working_directory, vibe_home, command)
    }

    fn spawn_command(working_directory: &Path, vibe_home: &Path, command: Command) -> Self {
        Self::spawn_command_with(working_directory, vibe_home, command, &[])
    }

    fn spawn_command_with(
        working_directory: &Path,
        vibe_home: &Path,
        mut command: Command,
        environment: &[(&str, String)],
    ) -> Self {
        let pty = openpty(
            Some(&Winsize {
                ws_row: 30,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .expect("PTY opens");
        let master = File::from(pty.master);
        let mut reader = master.try_clone().expect("PTY reader clones");
        let slave = File::from(pty.slave);
        let terminal = slave.try_clone().expect("PTY probe clones");
        let initial_termios = tcgetattr(&terminal).expect("PTY attributes read");
        let child = command
            .current_dir(working_directory)
            .env("HOME", vibe_home)
            .env("VIBE_HOME", vibe_home.join(".vibe"))
            .env("MISTRAL_API_KEY", "fixture")
            // Update discovery must never leave the machine during tests.
            .env("VIBE_UPDATE_BASE_URL", "http://127.0.0.1:9/v1")
            .env("NO_COLOR", "1")
            .env("TERM", "xterm-256color")
            .envs(environment.iter().map(|(key, value)| (*key, value)))
            .stdin(Stdio::from(slave.try_clone().expect("PTY stdin clones")))
            .stdout(Stdio::from(slave.try_clone().expect("PTY stdout clones")))
            .stderr(Stdio::from(slave))
            .spawn()
            .expect("TUI starts");
        let (sender, receiver) = mpsc::channel();
        // A real terminal answers the cursor-position report crossterm asks for
        // when it re-enters raw mode, which is what happens on the way back from
        // an external editor. Nothing else here plays terminal, so without this
        // reply the client waits out its own timeout and paints a failed UI
        // instead of the edited prompt.
        let mut responder = master.try_clone().expect("PTY responder clones");
        let reader = std::thread::spawn(move || {
            const CURSOR_REPORT_REQUEST: &[u8] = b"\x1b[6n";
            let mut transcript = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if buffer[..count]
                            .windows(CURSOR_REPORT_REQUEST.len())
                            .any(|window| window == CURSOR_REPORT_REQUEST)
                        {
                            let _ = responder.write_all(b"\x1b[1;1R");
                            let _ = responder.flush();
                        }
                        transcript.extend_from_slice(&buffer[..count]);
                        let _ = sender.send(buffer[..count].to_vec());
                    }
                }
            }
            transcript
        });
        Self {
            child,
            master,
            terminal,
            initial_termios,
            receiver,
            reader,
            screen: Vec::new(),
            consumed: 0,
        }
    }

    /// Waits for text the terminal has actually painted. Ratatui writes only
    /// the cells a frame changed, so a rendered phrase is routinely split by
    /// cursor moves and cannot be matched in the raw byte stream, and a cell
    /// that already held the right character is not written at all. A phrase
    /// absent from the screen earlier waits matched therefore also matches on
    /// the whole replayed screen; one already there must be painted again.
    pub fn wait_for_visible(&mut self, pattern: &str, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = self.screen[self.consumed..].to_vec();
        let already_visible = visible_text(&self.screen[..self.consumed]).contains(pattern);
        while !(visible_text(&output).contains(pattern)
            || (!already_visible && visible_text(&self.screen).contains(pattern)))
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.child.kill().expect("timed-out TUI stops");
                let _ = self.child.wait();
                panic!(
                    "PTY output omitted the visible {pattern:?}: {}",
                    String::from_utf8_lossy(&output)
                );
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(chunk) => {
                    self.screen.extend_from_slice(&chunk);
                    output.extend(chunk);
                }
                Err(error) => {
                    self.child.kill().expect("failed TUI stops");
                    let _ = self.child.wait();
                    panic!(
                        "PTY output stopped ({error}) before the visible {pattern:?}: {}",
                        String::from_utf8_lossy(&output)
                    );
                }
            }
        }
        self.consumed = self.screen.len();
        output
    }

    pub fn wait_for(&mut self, pattern: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = self.screen[self.consumed..].to_vec();
        while !output
            .windows(pattern.len())
            .any(|window| window == pattern)
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.child.kill().expect("timed-out TUI stops");
                let _ = self.child.wait();
                panic!(
                    "PTY output omitted {:?}: {}",
                    String::from_utf8_lossy(pattern),
                    String::from_utf8_lossy(&output)
                );
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(chunk) => {
                    self.screen.extend_from_slice(&chunk);
                    output.extend(chunk);
                }
                Err(error) => {
                    self.child.kill().expect("failed TUI stops");
                    let _ = self.child.wait();
                    panic!(
                        "PTY output stopped ({error}) before {:?}: {}",
                        String::from_utf8_lossy(pattern),
                        String::from_utf8_lossy(&output)
                    );
                }
            }
        }
        let start = self.screen.len() - output.len();
        let matched_end = output
            .windows(pattern.len())
            .position(|window| window == pattern)
            .map_or(output.len(), |index| index + pattern.len());
        self.consumed = start + matched_end;
        output
    }

    /// The terminal's attributes before the client started.
    pub fn initial_termios(&self) -> &Termios {
        &self.initial_termios
    }

    /// The terminal's attributes as the client last left them.
    pub fn termios(&self) -> Termios {
        tcgetattr(&self.terminal).expect("PTY attributes read")
    }

    /// The child's exit status, once it has exited.
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("child status")
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("PTY input writes");
        self.master.flush().expect("PTY input flushes");
    }

    pub fn interrupt(&self) {
        let pid = i32::try_from(self.child.id()).expect("child pid fits platform pid");
        kill(Pid::from_raw(pid), Signal::SIGINT).expect("SIGINT reaches TUI");
    }

    /// Reports whether the child stopped itself with `SIGTSTP`. A sandboxed
    /// run leaves the child in an orphaned process group, where POSIX discards
    /// stop signals, so this is observed rather than required.
    pub fn stopped(&self) -> bool {
        let pid = i32::try_from(self.child.id()).expect("child pid fits platform pid");
        matches!(
            waitpid(
                Pid::from_raw(pid),
                Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG)
            ),
            Ok(WaitStatus::Stopped(_, Signal::SIGTSTP))
        )
    }

    pub fn resume(&self) {
        let pid = i32::try_from(self.child.id()).expect("child pid fits platform pid");
        kill(Pid::from_raw(pid), Signal::SIGCONT).expect("SIGCONT reaches TUI");
    }

    pub fn wait(mut self, timeout: Duration) -> (ExitStatus, Vec<u8>) {
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.try_wait() {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("timed-out TUI stops");
                let _ = self.child.wait();
                panic!("TUI did not exit before timeout");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(self.master);
        drop(self.terminal);
        let transcript = self.reader.join().expect("PTY reader joins");
        (status, transcript)
    }

    pub fn kill(mut self) -> Vec<u8> {
        self.child.kill().expect("TUI stops");
        let _ = self.child.wait();
        drop(self.master);
        drop(self.terminal);
        self.reader.join().expect("PTY reader joins")
    }
}

pub fn seed_session(vibe_home: &Path, workspace: &Path, id: &str, marker: &str, timestamp: u64) {
    let store = SessionStore::new(vibe_home.join(".vibe/logs/session"));
    let mut metadata = store
        .create(id, &workspace.to_string_lossy(), None, timestamp)
        .expect("saved session");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::user(marker.to_owned()),
            timestamp + 1,
        )
        .expect("saved user message");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::Assistant {
                message_id: None,
                reasoning_message_id: None,
                content: "saved answer".to_owned(),
                reasoning: None,
                reasoning_payloads: Vec::new(),
                tool_calls: Vec::new(),
                keeps_empty_content: false,
            },
            timestamp + 2,
        )
        .expect("saved assistant message");
}

/// A temporary root holding the workspace a TUI starts in and the home it
/// keeps its state under. The root lives as long as the returned guard, which
/// removes the hardened plugin checkouts a session start leaves in the home.
pub fn tui_workspace() -> (test_root::TestRoot, PathBuf, PathBuf) {
    let temporary = test_root::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    (temporary, workspace, home)
}
