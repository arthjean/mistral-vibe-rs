//! Which stream a launch narrates its worktree on.
//!
//! The reference writes both progress lines to standard error and the one
//! preparation failure to standard output, and the split only exists at the
//! process boundary: a unit test on the writer cannot tell the two streams
//! apart. So this drives the `vibe` binary and reads its captured streams.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Preparation narrates the requested name, then the resolved path, both on
/// standard error, leaving standard output for the run's own result.
#[test]
fn preparation_narrates_on_standard_error_in_the_reference_order() {
    let Some(root) = scripted_repository() else {
        eprintln!("skipping: git is unavailable to build the fixture repository");
        return;
    };
    let output = launch(root.path(), &root.path().join("repository"));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    let preparing = stderr
        .find("Preparing worktree \"probe\"...")
        .unwrap_or_else(|| panic!("the requested name is not narrated, stderr: {stderr}"));
    let using = stderr
        .find("Using worktree: ")
        .unwrap_or_else(|| panic!("the resolved path is not narrated, stderr: {stderr}"));
    assert!(
        preparing < using,
        "narration out of order, stderr: {stderr}"
    );
    assert!(
        !stdout.contains("Preparing worktree") && !stdout.contains("Using worktree:"),
        "progress narration leaked to standard output: {stdout}"
    );
}

/// A preparation that fails reports on standard output and exits 1, which is
/// the one place the reference leaves standard error.
#[test]
fn a_preparation_failure_is_reported_on_standard_output_and_exits_one() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = root.path().join("not-a-repository");
    fs::create_dir_all(&workspace).expect("workspace directory");

    let output = launch(root.path(), &workspace);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        stdout.contains("--worktree requires a git repository"),
        "the failure must reach standard output, stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("--worktree requires a git repository"),
        "the failure must not also reach standard error, stderr: {stderr}"
    );
}

/// The flag documents itself the way the reference's does: one placeholder
/// called `NAME`, and help covering where the worktree lives, the branch it
/// checks out, the trust it grants, and the two flags that ignore it.
#[test]
fn the_worktree_flag_names_its_placeholder_and_documents_its_effects() {
    let output = Command::new(env!("CARGO_BIN_EXE_vibe"))
        .arg("--help")
        .output()
        .expect("vibe launched");
    let help = String::from_utf8_lossy(&output.stdout).into_owned();

    assert!(
        help.contains("--worktree <NAME>"),
        "the placeholder must be NAME, help: {help}"
    );
    for expected in [
        "kept under the vibe home",
        "branch called NAME",
        "trusts that directory without asking",
        "Ignored with --setup and --check-upgrade",
    ] {
        assert!(
            help.contains(expected),
            "help does not cover {expected:?}: {help}"
        );
    }
}

/// One `--worktree probe` launch, with the environment reduced to what
/// preparation reads and no provider credential in reach.
fn launch(home_root: &Path, workspace: &Path) -> Output {
    let home = home_root.join("home");
    fs::create_dir_all(&home).expect("home directory");
    Command::new(env!("CARGO_BIN_EXE_vibe"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("TERM", "dumb")
        .arg("--workdir")
        .arg(workspace)
        .args(["--worktree", "probe", "--prompt", "hi"])
        .stdin(Stdio::null())
        .output()
        .expect("vibe launched")
}

/// A single-commit repository, or `None` when git cannot build one here.
fn scripted_repository() -> Option<tempfile::TempDir> {
    let root = tempfile::tempdir().expect("fixture root");
    let repository = root.path().join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
    fs::write(repository.join("README.md"), "fixture\n").expect("tracked file");
    let steps: [&[&str]; 5] = [
        &["init", "--initial-branch", "main"],
        &["config", "user.email", "parity@example.invalid"],
        &["config", "user.name", "Parity"],
        &["add", "README.md"],
        &["commit", "--message", "fixture"],
    ];
    for step in steps {
        if !run_git(&repository, step) {
            return None;
        }
    }
    Some(root)
}

fn run_git(directory: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .is_ok_and(|output| output.status.success())
}
