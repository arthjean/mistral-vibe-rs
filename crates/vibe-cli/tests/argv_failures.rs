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

/// One launch, with the environment reduced to what startup reads and no
/// provider credential in reach, so a refusal is never a credential failure.
fn launch(argv: &[&str]) -> Output {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("home directory");
    Command::new(env!("CARGO_BIN_EXE_vibe"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("TERM", "dumb")
        .current_dir(canonical(root.path()))
        .args(argv)
        .stdin(Stdio::null())
        .output()
        .expect("vibe launched")
}

fn canonical(path: &Path) -> std::path::PathBuf {
    fs::canonicalize(path).expect("the fixture path resolves")
}
