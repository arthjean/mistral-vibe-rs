//! The TUI driven through a real pseudo-terminal.
//!
//! The file compiles to no test on Windows: it opens its terminal with
//! `nix::pty::openpty` and installs `#!/bin/sh` editor and shell fixtures made
//! executable by mode bits, none of which exist there. The Windows console path
//! is a ConPTY, owned by US-017 of `tasks/prd-windows-native-support.md`.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "PTY integration failures must terminate with their captured terminal transcript"
)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use nix::sys::termios::LocalFlags;
use vibe_core::storage::SessionStore;

#[path = "support/pty.rs"]
mod pty;
use pty::{PtyProcess, STEP, composer_carries, seed_session, tui_workspace, visible_text};

#[test]
fn interactive_tui_edits_input_and_restores_the_terminal_after_exit() {
    let (_temporary, workspace, home) = tui_workspace();
    let mut tui = PtyProcess::spawn(&workspace, &home, &["--trust"]);
    let baseline_termios = tui.initial_termios().clone();
    tui.wait_for(b"\x1b[?1049h", STEP);
    tui.write(b"/help\r");
    // The help is a Markdown message in the transcript, not a modal, so the
    // proof is a line of the document itself being painted. The command section
    // is last and this line sits four commands from its end, so a 30-row
    // terminal shows it.
    tui.wait_for_visible("Uninstall the Lean 4 agent", STEP);
    std::thread::sleep(Duration::from_millis(150));
    tui.write(b"!pwd\r");
    // A `!` command shows as a call whose output follows it, in the same frame
    // or a later one.
    let workspace_path = workspace.to_string_lossy().into_owned();
    if !visible_text(&tui.wait_for_visible("Ran pwd", STEP)).contains(&workspace_path) {
        tui.wait_for_visible(&workspace_path, STEP);
    }
    tui.write(b"/exitt\x7f\r");
    let deadline = Instant::now() + STEP;
    // The shell output streams into the transcript before the shell is
    // reaped, so a submission racing the final poll is rejected as busy and
    // restored into the composer as a draft; a later Enter resubmits it.
    let mut resubmit_at = Instant::now() + Duration::from_millis(500);
    let status = loop {
        if let Some(status) = tui.try_wait() {
            break status;
        }
        if Instant::now() >= resubmit_at {
            tui.write(b"\r");
            resubmit_at = Instant::now() + Duration::from_millis(500);
        }
        if Instant::now() >= deadline {
            let transcript = tui.kill();
            panic!(
                "interactive TUI did not exit within one step: {}",
                String::from_utf8_lossy(&transcript)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let restored_termios = tui.termios();
    let (_, transcript) = tui.wait(STEP);

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
    let (temporary, workspace, home) = tui_workspace();
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
            "http://127.0.0.1:9/v1",
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

    // `Ctrl+O` folds and unfolds tool output without a word, as reference
    // `action_toggle_tool` does; the chord below still being served is what
    // proves the two presses were consumed rather than wedging the loop.
    process.write(b"\x0f");
    process.write(b"\x0f");

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
    process.wait_for_visible("Command interrupted", STEP);

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
    let (_temporary, workspace, home) = tui_workspace();
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9/v1"],
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

/// US-026: a bracketed paste reaches the composer whole off Windows, where the
/// key-burst reader of US-019 stays disabled. The external editor receives the
/// prompt as it stands, so the copy it writes is the composer's exact text, and
/// a line break that had submitted would have left it holding the last line.
#[test]
fn a_bracketed_paste_keeps_its_lines_and_submits_nothing() {
    let (temporary, workspace, home) = tui_workspace();
    let captured = temporary.path().join("captured.md");
    let editor = temporary.path().join("editor.sh");
    std::fs::write(
        &editor,
        format!("#!/bin/sh\ncp \"$1\" '{}'\n", captured.display()),
    )
    .expect("editor script");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("editor is executable");
    let mut process = PtyProcess::spawn_with_environment(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9/v1"],
        &[("EDITOR", editor.to_string_lossy().into_owned())],
    );
    process.wait_for_visible("Type /help for more information", STEP);

    process.write(b"\x1b[200~first\nsecond\nthird\x1b[201~");
    process.wait_for_visible("third", STEP);
    process.write(b"\x07");
    let deadline = Instant::now() + STEP;
    let composer = loop {
        if let Ok(text) = std::fs::read_to_string(&captured) {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "the editor never received the prompt"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(composer, "first\nsecond\nthird");
    // The editor writes the file before it exits; keys sent before the TUI
    // takes the terminal back meet the cooked line discipline and are lost.
    process.wait_for(b"\x1b[?1049h", STEP);
    process.wait_for_visible("Type /help for more information", STEP);

    process.write(b"\x03");
    process.write(b"\x04");
    process.wait_for_visible("Press Ctrl+D again to quit", STEP);
    process.write(b"\x04");
    let (status, _) = process.wait(STEP);
    assert!(status.success(), "the TUI did not quit cleanly: {status}");
}

#[test]
fn sigint_after_mount_restores_terminal() {
    let (_temporary, workspace, home) = tui_workspace();
    let mut process = PtyProcess::spawn(&workspace, &home, &["--trust"]);
    process.wait_for(b"accept-edits", STEP);
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
    let (_temporary, workspace, home) = tui_workspace();
    std::fs::write(workspace.join("AGENTS.md"), "fixture\n").expect("trust-sensitive fixture");
    let mut process = PtyProcess::spawn(&workspace, &home, &[]);
    process.wait_for(b"Trust", STEP);
    assert!(!home.join(".vibe/logs/session").exists());
    process.write(b"\x03");
    let (status, transcript) = process.wait(STEP);

    assert!(status.success(), "trust cancellation exited with {status}");
    assert!(!home.join(".vibe/logs/session").exists());
    // Reference `trusted_folders_manager` creates the file empty at import,
    // so an aborted prompt leaves it holding no decision.
    assert_eq!(
        std::fs::read_to_string(home.join(".vibe/trusted_folders.toml")).expect("trust file"),
        "trusted = []\nuntrusted = []\n"
    );
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
    assert!(!home.path().join(".vibe/logs/session").exists());
    process.write(b"\x03");
    let (status, transcript) = process.wait(STEP);

    assert!(
        status.success(),
        "location cancellation exited with {status}"
    );
    assert!(!home.path().join(".vibe/logs/session").exists());
    assert!(
        transcript
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "location dialog did not restore the terminal"
    );
}

#[test]
fn positional_prompt_mounts_the_tui_before_dispatch() {
    let (_temporary, workspace, home) = tui_workspace();
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &[
            "--trust",
            "--api-base",
            "http://127.0.0.1:9/v1",
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
    let (_temporary, workspace, home) = tui_workspace();
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
    let (_temporary, workspace, home) = tui_workspace();
    seed_session(&home, &workspace, "saved-session", "saved marker", 1);
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--resume", "--api-base", "http://127.0.0.1:9/v1"],
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
    let (_temporary, workspace, home) = tui_workspace();
    let store = SessionStore::new(home.join(".vibe/logs/session"));
    seed_session(&home, &workspace, "saved-session", "saved marker", 1);
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--resume", "--api-base", "http://127.0.0.1:9/v1"],
    );
    process.wait_for(b"Resume", STEP);

    process.write(b"d");
    process.wait_for(b"d again to delete", STEP);
    assert!(store.load("saved-session").is_ok());

    process.write(b"d");
    process.wait_for(b"accept-edits", STEP);
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
                "http://127.0.0.1:9/v1",
            ],
            "direct resume marker",
        ),
        (
            vec![
                "--trust",
                "--continue",
                "--legacy-harness",
                "--api-base",
                "http://127.0.0.1:9/v1",
            ],
            "continue marker",
        ),
        // The Unified Harness continues only the sessions it saved itself
        // (reference `_sweep_unified_sessions`), so a legacy transcript is
        // passed over and the fresh session stays open.
        (
            vec![
                "--trust",
                "--continue",
                "--api-base",
                "http://127.0.0.1:9/v1",
            ],
            "No previous sessions found.",
        ),
    ] {
        let (_temporary, workspace, home) = tui_workspace();
        let id = if arguments.contains(&"--resume") {
            "direct-session"
        } else {
            "continue-session"
        };
        seed_session(&home, &workspace, id, marker, 10);
        let mut process = PtyProcess::spawn(&workspace, &home, &arguments);
        process.wait_for_visible(marker, STEP);
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
    let (_temporary, workspace, home) = tui_workspace();
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
    let (_temporary, workspace, home) = tui_workspace();
    std::fs::create_dir_all(workspace.join(".vibe")).expect("project config directory");
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
    let output = process.wait_for_visible("must-not-run", STEP);
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
    let (_temporary, workspace, home) = tui_workspace();
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9/v1"],
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
    let (_temporary, workspace, home) = tui_workspace();
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9/v1"],
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
    let (_temporary, workspace, home) = tui_workspace();
    let mut process = PtyProcess::spawn(
        &workspace,
        &home,
        &["--trust", "--api-base", "http://127.0.0.1:9/v1"],
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
fn setup_shows_the_trust_dialog_then_the_onboarding_screens_and_a_cancel_exits_cleanly() {
    let (_temporary, workspace, home) = tui_workspace();
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
