//! `vibe --check-upgrade` driven through a real pseudo-terminal.
//!
//! The file compiles to no test on Windows, for the reasons `tui_pty.rs`
//! gives: it opens its terminal with `nix::pty::openpty` and shadows the shell
//! with a `#!/bin/sh` fixture made executable by mode bits.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "PTY integration failures must terminate with their captured terminal transcript"
)]

use std::io::{Read, Write};
use std::path::Path;

#[path = "support/pty.rs"]
mod pty;
use pty::{PtyProcess, STEP, tui_workspace, visible_text};

#[test]
fn check_upgrade_reports_the_reference_failure_without_starting_a_session() {
    let (_temporary, workspace, home) = tui_workspace();
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
        !home.join(".vibe/logs/session").exists(),
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

#[test]
fn check_upgrade_prompts_with_the_release_the_github_gateway_reports() {
    let (temporary, workspace, home) = tui_workspace();
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
    let (temporary, workspace, home) = tui_workspace();
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
    let (temporary, workspace, home) = tui_workspace();
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
