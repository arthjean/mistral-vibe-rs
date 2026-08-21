//! What a launch says when `--worktree` is asked for and git is not there.
//!
//! The contract in `vibe_core::worktree` classifies a failed spawn of `git`
//! into its own error, and the point of that classification is the sentence the
//! operator reads. Only a real launch proves the sentence survives the crossing
//! into `StartupError`, so this test drives the `vibe` binary itself with a
//! `PATH` that holds nothing, rather than converting a hand-built error.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::path::Path;
use std::process::Command;

/// The launch fails before any provider work, so no credential is needed and
/// the git-absent sentence is the whole output that matters.
#[test]
fn a_launch_that_cannot_find_git_says_so_instead_of_leaking_the_spawn_error() {
    let Some(root) = scripted_repository() else {
        eprintln!("skipping: git is unavailable to build the fixture repository");
        return;
    };
    let workspace = root.path();
    let empty_path = workspace.join("empty-path");
    let home = workspace.join("home");
    fs::create_dir_all(&empty_path).expect("empty PATH directory");
    fs::create_dir_all(&home).expect("home directory");

    let output = Command::new(env!("CARGO_BIN_EXE_vibe"))
        .env_clear()
        .env("PATH", &empty_path)
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("TERM", "dumb")
        .args(["--workdir"])
        .arg(workspace.join("repository"))
        .args(["--worktree", "probe", "--prompt", "hi"])
        .output()
        .expect("vibe launched");

    let mut reported = String::from_utf8_lossy(&output.stdout).into_owned();
    reported.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(
        !output.status.success(),
        "a launch without git must fail, reported: {reported}"
    );
    assert!(
        reported.contains("require git on PATH"),
        "the failure must name git, reported: {reported}"
    );
    assert!(
        !reported.contains("worktree `git` failed"),
        "the raw spawn error must not stand in for the typed one, reported: {reported}"
    );
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
