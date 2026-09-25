//! What the binary exits with, and where it writes, when the argv is refused.
//!
//! The two failures a script has to tell apart only exist at the process
//! boundary: argparse exits 2 for an argv it cannot parse and the reference
//! exits 1 for a parse that succeeded and left the run unusable
//! (`vibe/cli/entrypoint.py:27` for the first, `vibe/cli/cli.py:147-150` for
//! the second). Both are read back from the binary here.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A refusal argparse renders itself exits 2 and reports on standard error,
/// under the usage block and the program's own name.
#[test]
fn a_parse_failure_exits_two_under_a_usage_block_on_standard_error() {
    let cases: &[(&[&str], &str)] = &[
        (&["--bogus"], "vibe: error: unrecognized arguments: --bogus"),
        (
            &["--output", "bogus"],
            "vibe: error: argument --output: invalid choice: 'bogus' (choose from text, json, \
             streaming)",
        ),
        (
            &["--agent"],
            "vibe: error: argument --agent: expected one argument",
        ),
        (
            &["--max-turns", "x"],
            "vibe: error: argument --max-turns: invalid int value: 'x'",
        ),
        (
            &["-c", "--resume"],
            "vibe: error: argument --resume: not allowed with argument -c/--continue",
        ),
    ];
    for (argv, expected) in cases {
        let output = launch(argv);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(2), "{argv:?} stderr: {stderr}");
        assert!(stdout.is_empty(), "{argv:?} wrote to stdout: {stdout}");
        assert!(
            stderr.starts_with("usage: vibe "),
            "{argv:?} opened without a usage block: {stderr}"
        );
        assert_eq!(
            stderr.lines().next_back(),
            Some(*expected),
            "{argv:?} stderr: {stderr}"
        );
    }
}

/// A prompt the parser accepted and the run cannot use exits 1, not 2, and is
/// prefixed the way the reference prefixes it rather than with an error-kind
/// name of this port's own.
#[test]
fn a_missing_programmatic_prompt_exits_one_under_the_reference_prefix() {
    let output = launch(&["--prompt", "   "]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(stdout.is_empty(), "the refusal reached stdout: {stdout}");
    assert_eq!(
        stderr.lines().next_back(),
        Some("Error: No prompt provided for programmatic mode"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("invalid arguments"),
        "this port's own error-kind name reached the user: {stderr}"
    );
}

/// The help and the version are answers rather than failures: standard output
/// and exit 0.
#[test]
fn the_help_and_the_version_exit_zero_on_standard_output() {
    for argv in [
        ["-h"].as_slice(),
        ["--help"].as_slice(),
        ["-v"].as_slice(),
        ["--version"].as_slice(),
    ] {
        let output = launch(argv);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(0), "{argv:?} stderr: {stderr}");
        assert!(!stdout.is_empty(), "{argv:?} printed nothing");
        assert!(stderr.is_empty(), "{argv:?} wrote to stderr: {stderr}");
    }
}

/// A budget the reference parses and finds already spent stops the run before
/// its first request, with the limit answer and exit 1: a turn budget of zero
/// or less, or a token or price budget below zero
/// (`vibe/core/middleware.py:48-96`, `vibe/cli/cli.py:214-216`). The provider
/// address is a closed port, so a run that did reach the model would fail on
/// the connection instead.
#[test]
fn a_budget_already_spent_stops_the_run_before_its_first_request() {
    for budget in [
        ["--max-turns", "-5"],
        ["--max-turns", "0"],
        ["--max-tokens", "-1"],
        ["--max-price", "-2.5"],
    ] {
        let mut argv = vec![
            "-p",
            "hello",
            "--trust",
            "--api-base",
            "http://127.0.0.1:9/v1",
        ];
        argv.extend(budget);
        let output = launch_with_key(&argv);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(1), "{budget:?} stderr: {stderr}");
        assert_eq!(
            stderr.lines().next_back(),
            Some("The configured conversation limit was reached"),
            "{budget:?} stderr: {stderr}"
        );
    }
}

/// `--smart-approve` and both harness flags start a programmatic run: the
/// Unified Harness is not here, so each lands on the legacy one.
#[test]
fn the_harness_flags_start_a_programmatic_run() {
    for flags in [
        ["--smart-approve"].as_slice(),
        ["--experimental-harness"].as_slice(),
        ["--legacy-harness"].as_slice(),
        ["--smart-approve", "--legacy-harness"].as_slice(),
    ] {
        let mut argv = vec![
            "-p",
            "hello",
            "--trust",
            "--api-base",
            "http://127.0.0.1:9/v1",
            "--max-turns",
            "0",
        ];
        argv.extend(flags);
        let output = launch_with_key(&argv);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(1), "{flags:?} stderr: {stderr}");
        assert_eq!(
            stderr.lines().next_back(),
            Some("The configured conversation limit was reached"),
            "{flags:?} stderr: {stderr}"
        );
    }
}

/// One launch, with the environment reduced to what startup reads and no
/// provider credential in reach, so a refusal is never a credential failure.
fn launch(argv: &[&str]) -> Output {
    launch_in(argv, None)
}

/// One launch with a placeholder credential, for a run that gets as far as
/// the session.
fn launch_with_key(argv: &[&str]) -> Output {
    launch_in(argv, Some("fixture"))
}

fn launch_in(argv: &[&str], credential: Option<&str>) -> Output {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("home directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_vibe"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("TERM", "dumb")
        // No keyring either: a credential comes from the environment or not at all.
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
    if let Some(credential) = credential {
        command.env("MISTRAL_API_KEY", credential);
    }
    command
        .current_dir(canonical(root.path()))
        .args(argv)
        .stdin(Stdio::null())
        .output()
        .expect("vibe launched")
}

fn canonical(path: &Path) -> std::path::PathBuf {
    fs::canonicalize(path).expect("the fixture path resolves")
}
