//! Where the `vibe` binary keeps its state when it starts.
//!
//! The reference reads `VIBE_HOME`, then `.vibe` under `Path.home()`, which on
//! Windows is `USERPROFILE` (then `HOMEDRIVE` plus `HOMEPATH`) and never `HOME`
//! (`docs/parity.md`, "Windows observations"). The first thing a launch writes
//! is its log file, `<vibe home>/logs/vibe.log`, so a launch that fails right
//! after (a `--workdir` that does not exist) shows where the home resolved.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod support;
use support::FixtureEnvironment;

struct Layout {
    _root: tempfile::TempDir,
    profile: PathBuf,
    /// What Git Bash exports as `HOME`, which Windows resolution ignores.
    bash_home: PathBuf,
    project: PathBuf,
}

fn layout() -> Layout {
    let root = tempfile::tempdir().expect("fixture root");
    let [profile, bash_home, project] =
        ["profile", "bash-home", "project"].map(|name| root.path().join(name));
    for directory in [&profile, &bash_home, &project] {
        fs::create_dir_all(directory).expect("fixture directory");
    }
    Layout {
        _root: root,
        profile,
        bash_home,
        project,
    }
}

fn launch(project: &Path, environment: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vibe"));
    command
        .env_clear_keeping_base()
        .env("VIBE_UPDATE_BASE_URL", "http://127.0.0.1:9")
        .env("TERM", "dumb")
        .current_dir(project)
        .args(["--workdir", "missing", "--prompt", "hi"])
        .stdin(Stdio::null());
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("vibe launched")
}

fn log_file(vibe_home: &Path) -> PathBuf {
    vibe_home.join("logs").join("vibe.log")
}

#[test]
fn the_profile_directory_holds_the_state() {
    let layout = layout();
    let environment: &[(&str, &Path)] = if cfg!(windows) {
        &[
            ("USERPROFILE", &layout.profile),
            ("HOME", &layout.bash_home),
        ]
    } else {
        &[("HOME", &layout.profile)]
    };
    let output = launch(&layout.project, environment);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        log_file(&layout.profile.join(".vibe")).is_file(),
        "the log must open under the profile: {output:?}"
    );
    assert!(!layout.project.join(".vibe").exists(), "{output:?}");
    assert!(!layout.bash_home.join(".vibe").exists(), "{output:?}");
}

#[test]
fn vibe_home_wins_and_expands_its_tilde() {
    let layout = layout();
    let home_variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let output = launch(
        &layout.project,
        &[
            (home_variable, &layout.profile),
            ("VIBE_HOME", Path::new("~/state")),
        ],
    );

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        log_file(&layout.profile.join("state")).is_file(),
        "{output:?}"
    );
    assert!(!layout.profile.join(".vibe").exists(), "{output:?}");
}

#[cfg(windows)]
#[test]
fn a_windows_host_without_a_profile_refuses_to_start() {
    let layout = layout();
    let output = launch(&layout.project, &[("HOME", &layout.bash_home)]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("VIBE_HOME"), "stderr: {stderr}");
    assert!(!layout.project.join(".vibe").exists(), "{output:?}");
    assert!(!layout.bash_home.join(".vibe").exists(), "{output:?}");
}
