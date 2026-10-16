//! Where the `vibe-acp` binary keeps its state when an editor starts it.
//!
//! It resolves the same home as `vibe` (`vibe_core::platform::vibe_home`): on
//! Windows `USERPROFILE` decides and `HOME` is never read. The log file opens
//! before the first request, so an editor that closes stdin at once still
//! shows where the home resolved.

#![allow(
    clippy::expect_used,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// What Windows needs in any child environment, carried over from the parent.
const WINDOWS_BASE: [&str; 7] = [
    "SystemRoot",
    "SystemDrive",
    "windir",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
];

fn launch(project: &Path, environment: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vibe-acp"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(project)
        .stdin(Stdio::null());
    if cfg!(windows) {
        for name in WINDOWS_BASE {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("vibe-acp launched")
}

#[test]
fn the_editor_session_keeps_its_state_in_the_profile_directory() {
    let root = tempfile::tempdir().expect("fixture root");
    let [profile, bash_home, project] =
        ["profile", "bash-home", "project"].map(|name| root.path().join(name));
    for directory in [&profile, &bash_home, &project] {
        fs::create_dir_all(directory).expect("fixture directory");
    }
    let environment: &[(&str, &Path)] = if cfg!(windows) {
        &[("USERPROFILE", &profile), ("HOME", &bash_home)]
    } else {
        &[("HOME", &profile)]
    };
    let output = launch(&project, environment);

    assert!(
        profile
            .join(".vibe")
            .join("logs")
            .join("vibe.log")
            .is_file(),
        "{output:?}"
    );
    assert!(!project.join(".vibe").exists(), "{output:?}");
    assert!(!bash_home.join(".vibe").exists(), "{output:?}");
}

#[cfg(windows)]
#[test]
fn a_windows_host_without_a_profile_refuses_to_start() {
    let root = tempfile::tempdir().expect("fixture root");
    let bash_home = root.path().join("bash-home");
    let project = root.path().join("project");
    fs::create_dir_all(&bash_home).expect("fixture directory");
    fs::create_dir_all(&project).expect("fixture directory");
    let output = launch(&project, &[("HOME", &bash_home)]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("VIBE_HOME"), "stderr: {stderr}");
    assert!(!project.join(".vibe").exists(), "{output:?}");
    assert!(!bash_home.join(".vibe").exists(), "{output:?}");
}
