//! The programmatic turn the release smoke job runs, driven against this
//! build's `vibe`.
//!
//! `.github/workflows/release.yml` runs `scripts/ci/smoke-programmatic.py`
//! only after a tag publishes a release. Running the same script here keeps it
//! working between releases, on every platform the test suite runs on.

#![allow(
    clippy::expect_used,
    reason = "an integration failure must terminate with the output the run produced"
)]

use std::path::Path;
use std::process::Command;

#[test]
fn the_release_smoke_turn_answers_through_the_stub_backend() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .arg(repository.join("scripts/ci/smoke-programmatic.py"))
        .arg(env!("CARGO_BIN_EXE_vibe"))
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the smoke script");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
