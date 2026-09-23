//! `vibe update` and `vibe --check-upgrade` driven at the process boundary.
//!
//! The reference rewrites a first argument of `update` into `--check-upgrade`
//! (`vibe/cli/entrypoint.py:206-208`) and loads the configuration before it
//! looks for a release (`vibe/cli/cli.py:433-439`). Each launch here points the
//! gateway at a local server or at a closed port, so nothing reaches GitHub.

#![allow(
    clippy::expect_used,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn launch(root: &Path, base_url: &str, arguments: &[&str]) -> Output {
    let home = root.join("home");
    let workspace = root.join("workspace");
    fs::create_dir_all(home.join(".vibe")).expect("vibe home");
    fs::create_dir_all(&workspace).expect("workspace");
    Command::new(env!("CARGO_BIN_EXE_vibe"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("VIBE_UPDATE_BASE_URL", base_url)
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .expect("vibe launched")
}

/// Serves one GitHub releases answer naming `tag`, then stops.
fn serve_release(tag: &str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let address = listener.local_addr().expect("local address");
    let body = format!(
        r#"[{{"tag_name":"{tag}","published_at":"2026-09-22T00:00:00Z","prerelease":false,"draft":false}}]"#
    );
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write the response");
    });
    (format!("http://{address}"), handle)
}

#[test]
fn update_is_the_check_upgrade_command() {
    let root = tempfile::tempdir().expect("fixture root");
    let (base_url, server) = serve_release(&format!("v{VERSION}"));
    let output = launch(root.path(), &base_url, &["update"]);
    server.join().expect("the server answered");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert_eq!(
        stdout.trim_end(),
        format!("Vibe is already up to date ({VERSION})."),
        "stdout: {stdout}"
    );
    let cache = fs::read_to_string(root.path().join("home/.vibe/cache.toml"))
        .expect("the forced check refreshes the cache");
    assert!(
        cache.contains(&format!("latest_version = \"{VERSION}\"")),
        "cache: {cache}"
    );
}

#[test]
fn a_failed_check_names_the_gateway_cause_and_exits_one() {
    let root = tempfile::tempdir().expect("fixture root");
    let output = launch(root.path(), "http://127.0.0.1:9", &["--check-upgrade"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert_eq!(
        stdout.trim_end(),
        "✗ Update check failed: Network error while checking for updates.",
    );
}

#[test]
fn a_configuration_no_session_could_load_fails_the_check_first() {
    let root = tempfile::tempdir().expect("fixture root");
    let config = root.path().join("home/.vibe/config.toml");
    fs::create_dir_all(config.parent().expect("config directory")).expect("vibe home");
    fs::write(&config, "theme = [1\n").expect("broken configuration");
    let output = launch(root.path(), "http://127.0.0.1:9", &["update"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("invalid TOML"), "stdout: {stdout}");
    assert!(
        !stdout.contains("Update check failed"),
        "the configuration fails before the gateway is asked, stdout: {stdout}"
    );
}

/// `update` is a command only in first position: after a separator it is the
/// prompt of an ordinary launch.
#[test]
fn update_elsewhere_is_a_prompt() {
    let root = tempfile::tempdir().expect("fixture root");
    let output = launch(
        root.path(),
        "http://127.0.0.1:9",
        &["--fake-response", "echoed", "-p", "update"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Update check failed"),
        "a programmatic prompt of `update` is not the command, stdout: {stdout}"
    );
}
