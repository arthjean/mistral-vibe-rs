#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};

#[test]
#[allow(clippy::unwrap_in_result)]
fn interactive_tui_edits_input_and_restores_the_terminal_after_exit() -> Result<(), String> {
    let temporary = tempfile::tempdir().expect("temporary TUI home");
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_vibe"))
        .current_dir(temporary.path())
        .arg("--trust")
        .env("HOME", temporary.path())
        .env("VIBE_HOME", temporary.path().join(".vibe"))
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
    Ok(())
}
