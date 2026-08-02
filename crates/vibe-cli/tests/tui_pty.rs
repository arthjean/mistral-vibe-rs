#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "PTY integration failures must terminate with their captured terminal transcript"
)]

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::{LocalFlags, tcgetattr};
use nix::unistd::Pid;
use vibe_core::events::ModelMessage;
use vibe_core::storage::SessionStore;

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

    let startup_deadline = Instant::now() + Duration::from_secs(2);
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
    let help_deadline = Instant::now() + Duration::from_secs(2);
    let mut help_output = Vec::new();
    while !help_output
        .windows(b"Help".len())
        .any(|window| window == b"Help")
    {
        let remaining = help_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            child.kill().expect("timed-out help TUI stops");
            let _ = child.wait();
            return Err(format!(
                "help overlay did not render: {}",
                String::from_utf8_lossy(&help_output)
            ));
        }
        let chunk = match transcript_receiver.recv_timeout(remaining) {
            Ok(chunk) => chunk,
            Err(error) => {
                child.kill().expect("timed-out help TUI stops");
                let _ = child.wait();
                return Err(format!(
                    "help overlay transcript stopped ({error}): {}",
                    String::from_utf8_lossy(&help_output)
                ));
            }
        };
        help_output.extend_from_slice(&chunk);
    }
    master.write_all(b"\x1b").expect("help escape writes");
    master.flush().expect("help escape flushes");
    std::thread::sleep(Duration::from_millis(150));
    master.write_all(b"!pwd\r").expect("shell command writes");
    master.flush().expect("shell command flushes");
    let shell_deadline = Instant::now() + Duration::from_secs(3);
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
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("TUI status is readable") {
            break status;
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

impl PtyProcess {
    fn spawn(working_directory: &Path, vibe_home: &Path, arguments: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vibe"));
        command.args(arguments);
        Self::spawn_command(working_directory, vibe_home, command)
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

    fn spawn_command(working_directory: &Path, vibe_home: &Path, mut command: Command) -> Self {
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
            .env("NO_COLOR", "1")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().expect("PTY stdin clones")))
            .stdout(Stdio::from(slave.try_clone().expect("PTY stdout clones")))
            .stderr(Stdio::from(slave))
            .spawn()
            .expect("TUI starts");
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut transcript = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
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
            &ModelMessage::User {
                content: marker.to_owned(),
            },
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

#[test]
fn sigint_after_mount_restores_terminal() {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
    let workspace = temporary.path().join("workspace");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let mut process = PtyProcess::spawn(&workspace, &home, &["--trust"]);
    process.wait_for(b"default", Duration::from_secs(3));
    process.interrupt();
    let (status, transcript) = process.wait(Duration::from_secs(3));

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
    process.wait_for(b"Trust", Duration::from_secs(3));
    assert!(!home.join(".vibe/sessions").exists());
    process.write(b"\x03");
    let (status, transcript) = process.wait(Duration::from_secs(3));

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
    process.wait_for(b"WARNING:", Duration::from_secs(3));
    assert!(!home.path().join(".vibe/sessions").exists());
    process.write(b"\x03");
    let (status, transcript) = process.wait(Duration::from_secs(3));

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
    process.wait_for(b"hello from startup", Duration::from_secs(5));
    let transcript = process.kill();
    let mounted = transcript
        .windows(b"\x1b[?1049h".len())
        .position(|window| window == b"\x1b[?1049h")
        .expect("TUI mounted");
    let submitted = transcript
        .windows(b"hello from startup".len())
        .position(|window| window == b"hello from startup")
        .expect("initial prompt rendered");
    assert!(
        mounted < submitted,
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
    let output = process.wait_for(b"hello from piped stdin", Duration::from_secs(5));
    let mounted = output
        .windows(b"\x1b[?1049h".len())
        .position(|window| window == b"\x1b[?1049h")
        .expect("TUI mounted");
    let submitted = output
        .windows(b"hello from piped stdin".len())
        .position(|window| window == b"hello from piped stdin")
        .expect("piped prompt rendered");
    assert!(mounted < submitted, "piped prompt preceded TUI mount");

    process.write(b"/exit\r");
    let (status, transcript) = process.wait(Duration::from_secs(3));
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
    process.wait_for(b"Resume", Duration::from_secs(3));
    process.write(b"\x1b");
    process.wait_for(b"\x1b[?1049h", Duration::from_secs(3));
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
        process.wait_for(marker.as_bytes(), Duration::from_secs(4));
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
    let (status, transcript) = process.wait(Duration::from_secs(3));
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
    let output = process.wait_for(b"must-not-run", Duration::from_secs(5));
    assert!(
        output
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "MCP failure appeared before TUI mount"
    );
    process.write(b"/exit\r");
    let (status, _) = process.wait(Duration::from_secs(3));
    assert!(status.success(), "recoverable MCP failure blocked exit");
}
