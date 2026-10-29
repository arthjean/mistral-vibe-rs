//! The `vibe` end-to-end battery of `tasks/prd-e2e-test-battery.md`.
//!
//! Every trial drives the shipped `vibe` binary in a hermetic world, inside
//! the network jail, against the scripted platform. `VIBE_E2E_SUBJECT=reference`
//! runs the same trials against the pinned Python reference.

mod cli;
mod engine;
mod faults;
mod hooks;
mod hygiene;
mod mcp;
mod permissions;
mod programmatic;
mod reference;
mod self_test;
mod sessions;
mod skills;
mod surface;
mod tools;

use std::process::ExitCode;

use vibe_e2e::Suite;

fn main() -> ExitCode {
    Suite::new("vibe-cli")
        .binary("vibe", env!("CARGO_BIN_EXE_vibe"))
        .artifacts(env!("CARGO_TARGET_TMPDIR"))
        .reference(reference::pinned)
        .scenarios(self_test::scenarios())
        .scenarios(programmatic::scenarios())
        .scenarios(sessions::scenarios())
        .scenarios(faults::scenarios())
        .scenarios(engine::scenarios())
        .scenarios(surface::scenarios())
        .scenarios(tools::scenarios())
        .scenarios(permissions::scenarios())
        .scenarios(hooks::scenarios())
        .scenarios(mcp::scenarios())
        .scenarios(skills::scenarios())
        .scenarios(hygiene::scenarios())
        .run()
}
