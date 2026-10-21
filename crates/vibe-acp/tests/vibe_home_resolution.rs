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

use std::path::Path;
use std::process::{Command, Output, Stdio};

#[path = "../../vibe-cli/tests/support/mod.rs"]
mod support;
use support::{FixtureEnvironment, home_layout, log_file};

fn launch(project: &Path, environment: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vibe-acp"));
    command
        .env_clear_keeping_base()
        .current_dir(project)
        .stdin(Stdio::null());
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("vibe-acp launched")
}

#[test]
fn the_editor_session_keeps_its_state_in_the_profile_directory() {
    let layout = home_layout();
    let output = launch(&layout.project, &layout.profile_environment());

    assert!(
        log_file(&layout.profile.join(".vibe")).is_file(),
        "{output:?}"
    );
    assert!(!layout.project.join(".vibe").exists(), "{output:?}");
    assert!(!layout.bash_home.join(".vibe").exists(), "{output:?}");
}

#[cfg(windows)]
#[test]
fn a_windows_host_without_a_profile_refuses_to_start() {
    let layout = home_layout();
    let output = launch(&layout.project, &[("HOME", &layout.bash_home)]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("VIBE_HOME"), "stderr: {stderr}");
    assert!(!layout.project.join(".vibe").exists(), "{output:?}");
    assert!(!layout.bash_home.join(".vibe").exists(), "{output:?}");
}
