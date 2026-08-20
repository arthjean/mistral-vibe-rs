#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "PTY integration failures must terminate with their captured terminal transcript"
)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::{LocalFlags, tcgetattr};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use vibe_core::events::ModelMessage;
use vibe_core::storage::SessionStore;

/// Replays a terminal transcript onto a sparse screen so assertions read what
/// the operator sees. Only cursor positioning and erasure move content; every
/// other escape is display state the text assertions do not depend on.
fn visible_text(transcript: &[u8]) -> String {
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
fn composer_carries(screen: &str, word: &str) -> bool {
    screen
        .lines()
        .map(str::trim_end)
        .any(|line| line == format!("> {word}") || line == format!("  {word}"))
}

#[test]
#[allow(clippy::unwrap_in_result)]
fn interactive_tui_edits_input_and_restores_the_terminal_after_exit() -> Result<(), String> {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
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
    let mut master = File::from(pty.master);
    let mut reader = master.try_clone().expect("PTY reader clones");
    let slave = File::from(pty.slave);
    let terminal_probe = slave.try_clone().expect("PTY probe clones");
    let baseline_termios = tcgetattr(&terminal_probe).expect("PTY attributes read");
    let mut child = Command::new(env!("CARGO_BIN_EXE_vibe"))
        .current_dir(&workspace)
        .arg("--trust")
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("MISTRAL_API_KEY", "fixture")
        // Update discovery must never leave the machine during tests.
        .env("VIBE_UPDATE_BASE_URL", "http://127.0.0.1:9")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().expect("PTY stdin clones")))
        .stdout(Stdio::from(slave.try_clone().expect("PTY stdout clones")))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("interactive TUI starts");
    let (transcript_sender, transcript_receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut transcript = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    transcript.extend_from_slice(&buffer[..count]);
                    let _ = transcript_sender.send(buffer[..count].to_vec());
                }
            }
        }
        transcript
    });

    let startup_deadline = Instant::now() + STEP;
    let mut startup = Vec::new();
    while !startup
        .windows(b"\x1b[?1049h".len())
        .any(|window| window == b"\x1b[?1049h")
    {
        let remaining = startup_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "interactive TUI did not enter the alternate screen"
        );
        let chunk = transcript_receiver
            .recv_timeout(remaining)
            .expect("TUI startup transcript arrives");
        startup.extend_from_slice(&chunk);
    }
    master.write_all(b"/help\r").expect("help command writes");
    master.flush().expect("edited prompt flushes");
    // The help is a Markdown message in the transcript, not a modal, so the
    // proof is a line of the document itself being painted. The command section
    // is last and this line sits four commands from its end, so a 30-row
    // terminal shows it. The match runs on the replayed screen for the reason
    // `TuiSession::wait_for_visible` documents: a phrase ratatui paints is
    // routinely split by cursor moves in the raw byte stream.
    const HELP_MARKER: &str = "Uninstall the Lean 4 agent";
    let help_deadline = Instant::now() + STEP;
    let mut help_output = Vec::new();
    while !visible_text(&help_output).contains(HELP_MARKER) {
        let remaining = help_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            child.kill().expect("timed-out help TUI stops");
            let _ = child.wait();
            return Err(format!(
                "help document did not render: {}",
                String::from_utf8_lossy(&help_output)
            ));
        }
        let chunk = match transcript_receiver.recv_timeout(remaining) {
            Ok(chunk) => chunk,
            Err(error) => {
                child.kill().expect("timed-out help TUI stops");
                let _ = child.wait();
                return Err(format!(
                    "help document transcript stopped ({error}): {}",
                    String::from_utf8_lossy(&help_output)
                ));
            }
        };
        help_output.extend_from_slice(&chunk);
    }
    std::thread::sleep(Duration::from_millis(150));
    master.write_all(b"!pwd\r").expect("shell command writes");
    master.flush().expect("shell command flushes");
    let shell_deadline = Instant::now() + STEP;
    let mut shell_output = Vec::new();
    while !shell_output
        .windows(b"###".len())
        .any(|window| window == b"###")
    {
        let remaining = shell_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            child.kill().expect("timed-out shell TUI stops");
            let _ = child.wait();
            return Err(format!(
                "shell result did not render: {}",
                String::from_utf8_lossy(&shell_output)
            ));
        }
        let chunk = match transcript_receiver.recv_timeout(remaining) {
            Ok(chunk) => chunk,
            Err(error) => {
                child.kill().expect("failed shell TUI stops");
                let _ = child.wait();
                return Err(format!(
                    "shell transcript stopped ({error}): {}",
                    String::from_utf8_lossy(&shell_output)
                ));
            }
        };
        shell_output.extend_from_slice(&chunk);
    }
    master
        .write_all(b"/exitt\x7f\r")
        .expect("edited exit command writes");
    master.flush().expect("edited prompt flushes");
    let deadline = Instant::now() + STEP;
    // The shell output streams into the transcript before the shell is
    // reaped, so a submission racing the final poll is rejected as busy and
    // restored into the composer as a draft; a later Enter resubmits it.
    let mut resubmit_at = Instant::now() + Duration::from_millis(500);
    let status = loop {
        if let Some(status) = child.try_wait().expect("TUI status is readable") {
            break status;
        }
        if Instant::now() >= resubmit_at {
            master.write_all(b"\r").expect("exit resubmission writes");
            master.flush().expect("exit resubmission flushes");
            resubmit_at = Instant::now() + Duration::from_millis(500);
        }
        let timed_out = Instant::now() >= deadline;
        if timed_out {
            child.kill().expect("timed-out TUI stops");
            let _ = child.wait();
        }
        assert!(
            !timed_out,
            "interactive TUI did not exit within five seconds"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let restored_termios = tcgetattr(&terminal_probe).expect("restored PTY attributes read");
    drop(terminal_probe);
    drop(master);
    let transcript = reader.join().expect("PTY reader joins");

    assert!(
        status.success(),
        "TUI exited with {status}: {}",
        String::from_utf8_lossy(&transcript)
    );
    assert!(
        transcript
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "TUI did not enter the alternate screen"
    );
    assert!(
        transcript
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "TUI did not restore the primary screen"
    );
    assert!(
        transcript
            .windows(b"\x1b[?25h".len())
            .any(|window| window == b"\x1b[?25h"),
        "TUI did not restore cursor visibility"
    );
    for (sequence, capability) in [
        (b"\x1b[?1000l".as_slice(), "mouse capture"),
        (b"\x1b[?2004l".as_slice(), "bracketed paste"),
        (b"\x1b[?1004l".as_slice(), "focus reporting"),
    ] {
        assert!(
            transcript
                .windows(sequence.len())
                .any(|window| window == sequence),
            "TUI did not disable {capability}"
        );
    }
    for flag in [LocalFlags::ICANON, LocalFlags::ECHO] {
        assert_eq!(
            restored_termios.local_flags.contains(flag),
            baseline_termios.local_flags.contains(flag),
            "TUI did not restore terminal flag {flag:?}"
        );
    }
    Ok(())
}

struct PtyProcess {
    child: Child,
    master: File,
    receiver: Receiver<Vec<u8>>,
    reader: JoinHandle<Vec<u8>>,
}

/// How long one step of a PTY exchange may take before the test gives up.
///
/// These are deadlines, not sleeps: a step the client answers immediately costs
/// nothing, so the budget is set for the slowest machine that runs this suite
/// rather than for the fastest one. Every wait shares it, so the suite's
/// tolerance for a loaded machine is one decision instead of thirty.
const STEP: Duration = Duration::from_secs(20);

impl PtyProcess {
    fn spawn(working_directory: &Path, vibe_home: &Path, arguments: &[&str]) -> Self {
        Self::spawn_with_environment(working_directory, vibe_home, arguments, &[])
    }

    /// The same launch with the shared defaults overridden, which is how the
    /// update tests point the gateway at a loopback fixture instead of the
    /// unreachable port every other test uses.
    fn spawn_with_environment(
        working_directory: &Path,
        vibe_home: &Path,
        arguments: &[&str],
        environment: &[(&str, String)],
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vibe"));
        command.args(arguments);
        Self::spawn_command_with(working_directory, vibe_home, command, environment)
    }

    fn spawn_piped_prompt(working_directory: &Path, vibe_home: &Path, prompt: &str) -> Self {
        let mut command = Command::new("setsid");
        command
            .args([
                "--ctty",
                "--wait",
                "sh",
                "-c",
                "printf '%s\\n' \"$VIBE_TEST_PROMPT\" | \"$VIBE_TEST_BIN\" --trust --api-base http://127.0.0.1:9",
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
        let child = command
            .current_dir(working_directory)
            .env("HOME", vibe_home)
            .env("VIBE_HOME", vibe_home.join(".vibe"))
            .env("MISTRAL_API_KEY", "fixture")
            // Update discovery must never leave the machine during tests.
            .env("VIBE_UPDATE_BASE_URL", "http://127.0.0.1:9")
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
            receiver,
            reader,
        }
    }

    /// Waits for text the terminal has actually painted. Ratatui writes only
    /// the cells a frame changed, so a rendered phrase is routinely split by
    /// cursor moves and cannot be matched in the raw byte stream.
    fn wait_for_visible(&mut self, pattern: &str, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        while !visible_text(&output).contains(pattern) {
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
                Ok(chunk) => output.extend(chunk),
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
        output
    }

    fn wait_for(&mut self, pattern: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
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
                Ok(chunk) => output.extend(chunk),
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
        output
    }

    fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("PTY input writes");
        self.master.flush().expect("PTY input flushes");
    }

    fn interrupt(&self) {
        let pid = i32::try_from(self.child.id()).expect("child pid fits platform pid");
        kill(Pid::from_raw(pid), Signal::SIGINT).expect("SIGINT reaches TUI");
    }

    /// Reports whether the child stopped itself with `SIGTSTP`. A sandboxed
    /// run leaves the child in an orphaned process group, where POSIX discards
    /// stop signals, so this is observed rather than required.
    fn stopped(&self) -> bool {
        let pid = i32::try_from(self.child.id()).expect("child pid fits platform pid");
        matches!(
            waitpid(
                Pid::from_raw(pid),
                Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG)
            ),
            Ok(WaitStatus::Stopped(_, Signal::SIGTSTP))
        )
    }

    fn resume(&self) {
        let pid = i32::try_from(self.child.id()).expect("child pid fits platform pid");
        kill(Pid::from_raw(pid), Signal::SIGCONT).expect("SIGCONT reaches TUI");
    }

    fn wait(mut self, timeout: Duration) -> (ExitStatus, Vec<u8>) {
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
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
        let transcript = self.reader.join().expect("PTY reader joins");
        (status, transcript)
    }

    fn kill(mut self) -> Vec<u8> {
        self.child.kill().expect("TUI stops");
        let _ = self.child.wait();
        drop(self.master);
        self.reader.join().expect("PTY reader joins")
    }
}

fn seed_session(vibe_home: &Path, workspace: &Path, id: &str, marker: &str, timestamp: u64) {
    let store = SessionStore::new(vibe_home.join(".vibe/sessions"));
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
                content: "saved answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
            timestamp + 2,
        )
        .expect("saved assistant message");
}

/// Every line of the help's shortcut section, performed in the running client.
///
/// `help_tests::every_advertised_key_routes_to_the_chord_its_line_names` proves
/// each advertised key resolves to the chord its line names. This proves the
/// other half: that the chord does what the line claims, in the binary an
/// operator runs rather than in a context a test assembles. A shortcut line is a
/// statement about this program only while both halves hold.
///
/// One session serves all eight, since a spawn costs more than a keystroke and
/// every step asserts on something the screen did not already carry. Assertions
/// read what appeared, never what left: a frame carries only the cells it
/// changed, so a phrase that scrolled away is still on the replayed screen.
/// `Shift+Enter` is the one advertised key no assertion below presses, because a
/// terminal reports it as a plain `Enter` unless it speaks the Kitty protocol,
/// which leaves `Ctrl+J` to stand for that line here.
///
/// The escape line names two actions and gets two assertions, one for each: the
/// overlay it dismisses, and the running operation it stops. The operation is a
/// manual shell command rather than a model turn, since no model answers in a
/// hermetic harness, and it is the same `escape` branch either way: the client
/// itself offers the key for both while something is running.
#[test]
fn every_advertised_shortcut_performs_its_action_in_the_running_tui() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    // The rewind line needs a message to rewind to.
    seed_session(&home, &workspace, "shortcut-session", "seeded prompt", 10);
    // The external-editor line needs an editor that answers.
    let editor = temporary.path().join("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf 'edited by the external editor' > \"$1\"\n",
    )
    .expect("editor script");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("editor is executable");
    // The stop half of the escape line needs an operation that keeps running
    // until it is stopped. `tail` is on the allowlist the manual shell reads, so
    // the command runs unattended, and `-f` is what makes it wait.
    std::fs::write(workspace.join("log.txt"), "a logged line\n").expect("log file");
    let mut process = PtyProcess::spawn_with_environment(
        &workspace,
        &home,
        &[
            "--trust",
            "--resume",
            "shortcut-session",
            "--api-base",
            "http://127.0.0.1:9",
        ],
        &[("EDITOR", editor.to_string_lossy().into_owned())],
    );
    process.wait_for_visible("seeded prompt", STEP);

    // `Enter` sends the prompt: a picker opens only because the line submitted.
    process.write(b"/theme\r");
    process.wait_for_visible("Select Theme", STEP);

    // `Escape` dismisses the overlay, and the composer taking text again is the
    // proof. The pause is not politeness: an escape byte immediately followed by
    // a letter is how a terminal reports `Alt`, so the two have to arrive as
    // separate reads to stay two presses. The column is what tells the composer
    // apart from the picker's own filter, which would have shown the same word
    // further right had the overlay stayed up.
    process.write(b"\x1b");
    std::thread::sleep(Duration::from_millis(150));
    process.write(b"zeta");
    let dismissed = visible_text(&process.wait_for_visible("zeta", STEP));
    assert!(
        composer_carries(&dismissed, "zeta"),
        "Escape left the overlay holding the keyboard: {dismissed}"
    );

    // `Ctrl+C` clears a non-empty prompt first and only offers to quit once the
    // prompt is empty, so the confirmation arriving on the second press is what
    // proves the first press cleared the composer.
    process.write(b"\x03");
    process.write(b"\x03");
    process.wait_for_visible("Press Ctrl+C again to quit", STEP);

    // `Ctrl+J` starts a new line rather than submitting. The column is the
    // assertion: the composer indents a continuation to the third cell, where a
    // key that had done nothing would leave `beta` appended to `alpha` instead.
    process.write(b"alpha");
    process.write(b"\x0a");
    process.write(b"beta");
    let composed = visible_text(&process.wait_for_visible("beta", STEP));
    assert!(
        composer_carries(&composed, "beta"),
        "Ctrl+J did not open a second composer line: {composed}"
    );
    process.write(b"\x03");

    // `Ctrl+O` folds and unfolds tool output, and the client names the state it
    // moved to. A session starts folded, so the first press unfolds.
    process.write(b"\x0f");
    process.wait_for_visible("Tool output expanded", STEP);
    process.write(b"\x0f");
    // Only the word changes on the flip back, so only the word is repainted.
    process.wait_for_visible("collapsed", STEP);

    // `Shift+Tab` switches to the next agent, which a terminal sends as
    // `BackTab`.
    process.write(b"\x1b[Z");
    process.wait_for_visible("Switched to agent", STEP);

    // `Esc Esc` on an empty prompt rewinds. The two presses travel separately
    // because a terminal sending them in one packet is reporting one
    // escape-prefixed key, not two presses.
    process.write(b"\x1b");
    std::thread::sleep(Duration::from_millis(150));
    process.write(b"\x1b");
    process.wait_for_visible("Edit an earlier message", STEP);
    process.write(b"q");

    // `Ctrl+G` hands the prompt to the external editor and keeps what came back.
    process.write(b"\x07");
    process.wait_for_visible("edited by the external editor", STEP);

    // `Escape` also stops what is running, which is the other half of its line.
    // The command prints its file and then waits, so its own output is what says
    // the operation is under way; the key is pressed only once it is.
    process.write(b"\x03");
    process.write(b"!tail -f log.txt\r");
    process.wait_for_visible("a logged line", STEP);
    process.write(b"\x1b");
    process.wait_for_visible("Command was interrupted", STEP);

    // `Ctrl+D` quits, once the prompt the editor filled is out of its way.
    process.write(b"\x04");
    process.wait_for_visible("Press Ctrl+D again to quit", STEP);
    process.write(b"\x04");
    let (status, _) = process.wait(STEP);
    assert!(status.success(), "Ctrl+D did not quit cleanly: {status}");
}

/// US-233: a submitted command line is echoed into the running transcript,
/// under the `/` prompt reference `SlashCommandMessage` paints it with, before
/// the command's own output. `/copy` is the cheapest command that both echoes
/// and answers without a session behind it.
#[test]
fn a_submitted_command_line_is_echoed_in_the_running_tui() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9"],
    );
    process.wait_for_visible("Type /help for more information", STEP);

    process.write(b"/copy\r");
    let screen = visible_text(&process.wait_for_visible("/ copy", STEP));
    assert!(
        screen
            .lines()
            .map(str::trim_end)
            .any(|line| line == "/ copy"),
        "the echo did not land on its own transcript row: {screen}"
    );
    assert!(
        !screen
            .lines()
            .map(str::trim_end)
            .any(|line| line == "> /copy"),
        "the echo was mounted as an operator message: {screen}"
    );

    process.write(b"\x04");
    process.wait_for_visible("Press Ctrl+D again to quit", STEP);
    process.write(b"\x04");
    let (status, _) = process.wait(STEP);
    assert!(status.success(), "the TUI did not quit cleanly: {status}");
}

#[test]
fn sigint_after_mount_restores_terminal() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(&workspace, &home, &["--trust"]);
    process.wait_for(b"default", STEP);
    process.interrupt();
    let (status, transcript) = process.wait(STEP);

    assert!(status.success(), "SIGINT shutdown exited with {status}");
    for (sequence, capability) in [
        (b"\x1b[?1049l".as_slice(), "primary screen"),
        (b"\x1b[?25h".as_slice(), "visible cursor"),
        (b"\x1b[?2004l".as_slice(), "disabled bracketed paste"),
    ] {
        assert!(
            transcript
                .windows(sequence.len())
                .any(|window| window == sequence),
            "SIGINT did not restore {capability}: {}",
            String::from_utf8_lossy(&transcript)
        );
    }
}

#[test]
fn trust_abort_restores_terminal_without_starting_session_discovery() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(workspace.join("AGENTS.md"), "fixture\n").expect("trust-sensitive fixture");
    let mut process = PtyProcess::spawn(&workspace, &home, &[]);
    process.wait_for(b"Trust", STEP);
    assert!(!home.join(".vibe/sessions").exists());
    process.write(b"\x03");
    let (status, transcript) = process.wait(STEP);

    assert!(status.success(), "trust cancellation exited with {status}");
    assert!(!home.join(".vibe/sessions").exists());
    assert!(!home.join(".vibe/trusted_folders.toml").exists());
    assert!(
        transcript
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h")
    );
    assert!(
        transcript
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l")
    );
}

#[test]
fn sensitive_location_abort_precedes_session_discovery_and_restores_terminal() {
    let home = tempfile::tempdir().expect("sensitive home");
    let mut process = PtyProcess::spawn(home.path(), home.path(), &[]);
    process.wait_for(b"WARNING:", STEP);
    assert!(!home.path().join(".vibe/sessions").exists());
    process.write(b"\x03");
    let (status, transcript) = process.wait(STEP);

    assert!(
        status.success(),
        "location cancellation exited with {status}"
    );
    assert!(!home.path().join(".vibe/sessions").exists());
    assert!(
        transcript
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "location dialog did not restore the terminal"
    );
}

#[test]
fn positional_prompt_mounts_the_tui_before_dispatch() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &[
            "--trust",
            "--api-base",
            "http://127.0.0.1:9",
            "hello from startup",
        ],
    );
    let mounted = process.wait_for(b"\x1b[?1049h", STEP).len();
    let rendered = process.wait_for_visible("hello from startup", STEP).len();
    process.kill();
    assert!(
        mounted <= rendered,
        "prompt appeared before the TUI mounted"
    );
}

#[test]
fn piped_prompt_mounts_before_dispatch_and_keeps_the_tty_interactive() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn_piped_prompt(&workspace, &home, "hello from piped stdin");
    let mounted = process.wait_for(b"\x1b[?1049h", STEP).len();
    let rendered = process
        .wait_for_visible("hello from piped stdin", STEP)
        .len();
    assert!(mounted <= rendered, "piped prompt preceded TUI mount");

    process.write(b"\x04\x04");
    let (status, transcript) = process.wait(STEP);
    assert!(status.success(), "piped TUI exited with {status}");
    assert!(
        transcript
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "piped TUI did not restore the terminal"
    );
}

#[test]
fn bare_resume_opens_the_directory_scoped_picker_before_starting_new() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    SessionStore::new(home.join(".vibe/sessions"))
        .create("saved-session", &workspace.to_string_lossy(), None, 1)
        .expect("saved session");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--resume", "--api-base", "http://127.0.0.1:9"],
    );
    process.wait_for(b"Resume", STEP);
    process.write(b"\x1b");
    process.wait_for(b"\x1b[?1049h", STEP);
    let transcript = process.kill();

    assert!(
        transcript
            .windows(b"saved-se".len())
            .any(|window| window == b"saved-se")
    );
    assert!(
        transcript
            .windows(b"\x1b[?1049h".len())
            .filter(|window| *window == b"\x1b[?1049h")
            .count()
            >= 2,
        "resume picker did not precede the main TUI"
    );
}

#[test]
fn bare_resume_deletes_only_after_confirmation_and_final_delete_starts_new() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let store = SessionStore::new(home.join(".vibe/sessions"));
    store
        .create("saved-session", &workspace.to_string_lossy(), None, 1)
        .expect("saved session");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--resume", "--api-base", "http://127.0.0.1:9"],
    );
    process.wait_for(b"Resume", STEP);

    process.write(b"d");
    process.wait_for(b"Press d aga", STEP);
    assert!(store.load("saved-session").is_ok());

    process.write(b"d");
    process.wait_for(b"default", STEP);
    assert!(store.load("saved-session").is_err());
    let transcript = process.kill();
    assert!(
        transcript
            .windows(b"\x1b[?1049h".len())
            .filter(|window| *window == b"\x1b[?1049h")
            .count()
            >= 2,
        "final deletion did not continue into a new-session TUI"
    );
}

#[test]
fn direct_resume_and_continue_hydrate_the_requested_saved_session() {
    for (arguments, marker) in [
        (
            vec![
                "--trust",
                "--resume",
                "direct-session",
                "--api-base",
                "http://127.0.0.1:9",
            ],
            "direct resume marker",
        ),
        (
            vec!["--trust", "--continue", "--api-base", "http://127.0.0.1:9"],
            "continue marker",
        ),
    ] {
        let temporary = tempfile::tempdir().expect("temporary TUI home");
        let workspace = temporary.path().join("workspace");
        let home = temporary.path().join("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&home).expect("home");
        let id = if arguments.contains(&"--resume") {
            "direct-session"
        } else {
            "continue-session"
        };
        seed_session(&home, &workspace, id, marker, 10);
        let mut process = PtyProcess::spawn(&workspace, &home, &arguments);
        process.wait_for(marker.as_bytes(), STEP);
        let transcript = process.kill();
        assert!(
            transcript
                .windows(b"Resume a saved session".len())
                .all(|window| window != b"Resume a saved session"),
            "direct session intent unexpectedly opened the picker"
        );
    }
}

#[test]
fn missing_direct_resume_fails_without_opening_another_session() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    seed_session(&home, &workspace, "existing-session", "must not resume", 10);
    let process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--resume", "missing-session"],
    );
    let (status, transcript) = process.wait(STEP);
    assert!(!status.success());
    assert!(
        transcript
            .windows(b"missing-session".len())
            .any(|window| window == b"missing-session")
    );
    assert!(
        transcript
            .windows(b"must not resume".len())
            .all(|window| window != b"must not resume")
    );
}

#[test]
fn mcp_discovery_failure_is_visible_after_mount_and_remains_recoverable() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(workspace.join(".vibe")).expect("project config directory");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(
        workspace.join(".vibe/config.toml"),
        r#"
[[mcp_servers]]
name = "broken"
transport = "stdio"
command = "/must-not-run"
"#,
    )
    .expect("project MCP config");
    let mut process = PtyProcess::spawn(&workspace, &home, &["--trust"]);
    let output = process.wait_for(b"must-not-run", STEP);
    assert!(
        output
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "MCP failure appeared before TUI mount"
    );
    process.write(b"/exit\r");
    let (status, _) = process.wait(STEP);
    assert!(status.success(), "recoverable MCP failure blocked exit");
}

#[test]
fn ctrl_z_suspends_the_session_and_resumes_a_restored_terminal() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9"],
    );
    process.wait_for(b"\x1b[?1049h", STEP);
    process.write(b"\x1a");
    // The reference restores the terminal, prints the resume hint, and only
    // then stops, so both observations precede the stop.
    let suspended = process.wait_for(
        b"Mistral Vibe has been suspended. Run fg to bring Mistral Vibe back.",
        STEP,
    );
    assert!(
        suspended
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "suspend did not leave the alternate screen first"
    );
    std::thread::sleep(Duration::from_millis(200));
    let _stopped = process.stopped();
    process.resume();
    std::thread::sleep(Duration::from_millis(200));
    process.write(b"\x04\x04");
    let (status, transcript) = process.wait(STEP);

    assert!(status.success(), "suspended session exited with {status}");
    for (sequence, expected, label) in [
        (b"\x1b[?1049h".as_slice(), 2, "alternate screen entries"),
        (
            b"\x1b[?1049l".as_slice(),
            2,
            "alternate screen restorations",
        ),
    ] {
        let seen = transcript
            .windows(sequence.len())
            .filter(|window| *window == sequence)
            .count();
        assert!(
            seen >= expected,
            "suspend and resume must each produce {label}, saw {seen}: {}",
            String::from_utf8_lossy(&transcript)
        );
    }
}

#[test]
fn confirmed_exit_prints_the_reference_session_summary() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9"],
    );
    process.wait_for(b"\x1b[?1049h", STEP);
    process.write(b"\x04");
    process.wait_for_visible("Press Ctrl+D again to quit", STEP);
    process.write(b"\x04");
    let (status, transcript) = process.wait(STEP);

    assert!(status.success(), "confirmed exit failed with {status}");
    let text = String::from_utf8_lossy(&transcript);
    assert!(
        text.contains("Total tokens used this session: input=0 output=0 (total=0)"),
        "exit omitted the reference usage summary: {text}"
    );
    assert!(
        text.contains("To continue this session, run: vibe --continue"),
        "exit omitted the reference resume command: {text}"
    );
    assert!(
        text.contains("Or: vibe --resume "),
        "exit omitted the short resume identifier: {text}"
    );
}

#[test]
fn focus_events_restore_the_reference_terminal_title() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9"],
    );
    process.wait_for(b"\x1b[?1049h", STEP);
    // Reference `on_app_blur` records focus silently; `on_app_focus` restores
    // the default title, which is the observable half of the contract.
    process.write(b"\x1b[O");
    process.write(b"\x1b[I");
    process.wait_for(b"\x1b]0;Vibe\x07", STEP);
    let transcript = process.kill();
    assert!(
        transcript
            .windows(b"\x1b[?1004h".len())
            .any(|window| window == b"\x1b[?1004h"),
        "focus reporting was never enabled: {}",
        String::from_utf8_lossy(&transcript)
    );
}

#[test]
fn check_upgrade_reports_the_reference_failure_without_starting_a_session() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let process = PtyProcess::spawn(&workspace, &home, &["--check-upgrade"]);
    let (status, transcript) = process.wait(STEP);

    let text = String::from_utf8_lossy(&transcript);
    assert!(
        text.contains("Update check failed: Network error while checking for updates."),
        "check-upgrade omitted the reference failure: {text}"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "the reference exits non-zero after a failed check"
    );
    assert!(
        !home.join(".vibe/sessions").exists(),
        "check-upgrade started session discovery"
    );
}

/// Serves the one endpoint `GitHubUpdateGateway` reads, answering every request
/// with the same releases payload until the test process exits.
///
/// The payload carries a draft and a prerelease ahead of the published release
/// so a run that stopped filtering them would report the wrong version.
fn github_releases_fixture() -> String {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("fixture binds a loopback port");
    let port = listener
        .local_addr()
        .expect("fixture reports its port")
        .port();
    let body = concat!(
        r#"[{"tag_name":"v100.0.0","draft":true,"prerelease":false,"published_at":"2026-08-19T12:00:00Z"},"#,
        r#"{"tag_name":"v101.0.0","draft":false,"prerelease":true,"published_at":"2026-08-19T13:00:00Z"},"#,
        r#"{"tag_name":"v99.0.0","draft":false,"prerelease":false,"published_at":"2026-08-19T11:00:00Z"}]"#,
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => request.extend_from_slice(&buffer[..count]),
                }
            }
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Puts a `sh` that exits with `code` ahead of the real one on `PATH`.
///
/// The upgrade runner spawns every command through `sh -c`, so shadowing the
/// shell proves the whole path from the prompt to the exit code without ever
/// running the published installer or reaching the network.
fn shell_exiting_with(directory: &Path, code: i32) -> String {
    std::fs::create_dir_all(directory).expect("shell fixture directory");
    let shell = directory.join("sh");
    std::fs::write(&shell, format!("#!/bin/sh\nexit {code}\n")).expect("shell fixture writes");
    let mut permissions = std::fs::metadata(&shell)
        .expect("shell fixture metadata")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&shell, permissions).expect("shell fixture becomes executable");
    let inherited = std::env::var("PATH").unwrap_or_default();
    format!("{}:{inherited}", directory.display())
}

fn update_workspace() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    (temporary, workspace, home)
}

#[test]
fn check_upgrade_prompts_with_the_release_the_github_gateway_reports() {
    let (temporary, workspace, home) = update_workspace();
    let mut process = PtyProcess::spawn_with_environment(
        &workspace,
        &home,
        &["--check-upgrade"],
        &[("VIBE_UPDATE_BASE_URL", github_releases_fixture())],
    );
    // One wait, then one assertion set: `wait_for_visible` accumulates only the
    // chunks it read, so a second call cannot see a frame the first consumed.
    let dialog = visible_text(&process.wait_for_visible("Cancel upgrade", STEP));
    assert!(
        dialog.contains("A new Vibe release is available"),
        "the check-upgrade dialog lost its title: {dialog}"
    );
    assert!(
        dialog.contains(&format!("{} \u{2192} 99.0.0", env!("CARGO_PKG_VERSION"))),
        "the dialog did not name the release the gateway reported: {dialog}"
    );

    // Right then Enter is `Cancel upgrade`, which must run no command at all.
    process.write(b"\x1b[C");
    process.write(b"\r");
    let (status, transcript) = process.wait(STEP);
    let text = String::from_utf8_lossy(&transcript);
    assert_eq!(
        status.code(),
        Some(0),
        "cancelling the upgrade is not a failed check: {text}"
    );
    assert!(
        !text.contains("Updating Vibe"),
        "cancelling the upgrade started an upgrade anyway: {text}"
    );
    drop(temporary);
}

#[test]
fn choosing_update_now_reports_the_installed_version_and_exits_zero() {
    let (temporary, workspace, home) = update_workspace();
    let path = shell_exiting_with(&temporary.path().join("succeeding-shell"), 0);
    let mut process = PtyProcess::spawn_with_environment(
        &workspace,
        &home,
        &["--check-upgrade"],
        &[
            ("VIBE_UPDATE_BASE_URL", github_releases_fixture()),
            ("PATH", path),
        ],
    );
    process.wait_for_visible("A new Vibe release is available", STEP);
    process.write(b"\r");
    let (status, transcript) = process.wait(STEP);
    let text = String::from_utf8_lossy(&transcript);
    assert!(
        text.contains("Updating Vibe. Press Ctrl+C to cancel."),
        "the upgrade never announced itself: {text}"
    );
    assert!(
        text.contains(&format!(
            "Vibe was updated from {} to 99.0.0.",
            env!("CARGO_PKG_VERSION")
        )),
        "a succeeding upgrade did not name both versions: {text}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "the reference exits zero after UPDATED: {text}"
    );
    drop(temporary);
}

#[test]
fn an_upgrade_whose_commands_all_fail_names_the_manual_path_and_exits_one() {
    let (temporary, workspace, home) = update_workspace();
    let path = shell_exiting_with(&temporary.path().join("failing-shell"), 1);
    let mut process = PtyProcess::spawn_with_environment(
        &workspace,
        &home,
        &["--check-upgrade"],
        &[
            ("VIBE_UPDATE_BASE_URL", github_releases_fixture()),
            ("PATH", path),
        ],
    );
    process.wait_for_visible("A new Vibe release is available", STEP);
    process.write(b"\r");
    let (status, transcript) = process.wait(STEP);
    let text = String::from_utf8_lossy(&transcript);
    assert!(
        text.contains("Vibe could not update automatically."),
        "a failed upgrade did not report itself: {text}"
    );
    assert!(
        text.contains("rerunning the installer from https://github.com/arthjean/mistral-vibe-rs"),
        "the failed branch did not name this port's manual path: {text}"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "the reference exits non-zero after UPDATE_FAILED: {text}"
    );
    drop(temporary);
}

#[test]
fn setup_shows_the_trust_dialog_then_the_onboarding_screens_and_a_cancel_exits_cleanly() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    // An AGENTS.md is what makes the workspace worth a trust decision.
    std::fs::write(workspace.join("AGENTS.md"), "# workspace instructions\n")
        .expect("workspace instructions");
    let mut process = PtyProcess::spawn(&workspace, &home, &["--setup"]);
    process.wait_for(b"\x1b[?1049h", STEP);
    // With trust undecided, the pre-session dialog now precedes the setup
    // screens instead of being skipped by `--setup`.
    process.wait_for_visible("Trust", STEP);
    process.write(b"\r");
    // The welcome text types progressively; its prefix appearing proves the
    // screen sequence mounted in place of the retired chat-transcript setup.
    process.wait_for_visible("Welcome", STEP);
    process.write(b"\x1b");
    let (status, transcript) = process.wait(STEP);
    assert!(
        status.success(),
        "cancelled onboarding exited with {status}"
    );
    assert!(
        transcript
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "onboarding did not restore the terminal"
    );
    let text = String::from_utf8_lossy(&transcript);
    assert!(
        text.contains("Setup canceled"),
        "the cancellation message did not print after the terminal restored: {text}"
    );
}
