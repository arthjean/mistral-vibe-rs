//! The `vibe-acp` end-to-end battery of `tasks/prd-e2e-test-battery.md`.
//!
//! Every trial drives the shipped `vibe-acp` binary over stdio as an editor
//! does, in a hermetic world inside the network jail.
//! `VIBE_E2E_SUBJECT=reference` runs the same trials against the pinned
//! Python reference.

#[path = "../../../vibe-cli/tests/e2e/reference.rs"]
mod reference;

use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Child, Ctx, Outcome, Scenario, Suite, ensure};

fn main() -> ExitCode {
    Suite::new("vibe-acp")
        .binary("vibe-acp", env!("CARGO_BIN_EXE_vibe-acp"))
        .artifacts(env!("CARGO_TARGET_TMPDIR"))
        .reference(reference::pinned)
        .scenarios([Scenario::new(
            "acp::initialize_then_new_session",
            initialize_then_new_session,
        )])
        .run()
}

/// Reads messages until the response to `id`.
fn response(acp: &mut Child, id: u64) -> Outcome<Value> {
    loop {
        let line = acp.next_line(Duration::from_secs(30))?;
        let message: Value = serde_json::from_str(&line)?;
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(message);
        }
    }
}

fn initialize_then_new_session(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut acp = world.command("vibe-acp")?.stdin_piped().spawn()?;
    acp.write_line(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}})
        .to_string(),
    )?;
    let initialized = response(&mut acp, 1)?;
    ensure(initialized["result"]["protocolVersion"] == 1, || {
        format!("initialize answered {initialized}")
    })?;
    acp.write_line(
        &json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {"cwd": world.work(), "mcpServers": []}})
        .to_string(),
    )?;
    let created = response(&mut acp, 2)?;
    ensure(created["result"]["sessionId"].is_string(), || {
        format!("session/new answered {created}")
    })
}
