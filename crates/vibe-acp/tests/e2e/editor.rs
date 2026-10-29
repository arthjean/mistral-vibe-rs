//! What the ACP scenarios share: an editor speaking to the subject's
//! `vibe-acp`, its handshake, sessions, prompts, and the updates it receives.

use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Command, Outcome, Rpc, World, ensure};

/// How long a prompt gets to end.
pub const PROMPT: Duration = Duration::from_secs(30);

/// Starts the subject's `vibe-acp` with `flags`, without a handshake.
pub fn spawn(world: &World, flags: &[&str]) -> Outcome<Rpc> {
    spawn_command(world.command("vibe-acp")?.args(flags.iter().copied()))
}

/// Starts `command` with piped stdin, without a handshake.
pub fn spawn_command(command: Command) -> Outcome<Rpc> {
    Ok(Rpc::new(command.stdin_piped().spawn()?))
}

/// Sends `initialize` with `capabilities` and returns its result.
pub fn initialize(rpc: &mut Rpc, capabilities: Value) -> Outcome<Value> {
    rpc.call(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": capabilities,
               "clientInfo": {"name": "vibe-e2e", "version": "0"}}),
    )
}

/// An editor with no client capabilities, past `initialize`.
pub fn editor(world: &World, flags: &[&str]) -> Outcome<Rpc> {
    let mut rpc = spawn(world, flags)?;
    initialize(&mut rpc, json!({}))?;
    Ok(rpc)
}

/// Opens a session in the world's work tree and returns its id.
pub fn new_session(rpc: &mut Rpc, world: &World) -> Outcome<String> {
    let result = rpc.call(
        "session/new",
        json!({"cwd": world.work(), "mcpServers": []}),
    )?;
    result["sessionId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("session/new answered {result}").into())
}

/// Sends a text prompt and waits for its whole response.
pub fn prompt(rpc: &mut Rpc, session: &str, text: &str) -> Outcome<Value> {
    let id = rpc.send(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
    )?;
    rpc.response_within(id, PROMPT)
}

/// Sends a text prompt and fails unless it ends with `stop_reason`.
pub fn prompt_ends(rpc: &mut Rpc, session: &str, text: &str, stop_reason: &str) -> Outcome {
    let response = prompt(rpc, session, text)?;
    ensure(response["result"]["stopReason"] == stop_reason, || {
        format!("session/prompt answered {response}, expected stopReason {stop_reason}")
    })
}

/// The `sessionUpdate` kind of every `session/update` received so far, with
/// consecutive repeats collapsed, since chunk boundaries are the stream's.
pub fn update_kinds(rpc: &Rpc) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for message in rpc.notifications() {
        if message["method"] != "session/update" {
            continue;
        }
        let kind = message["params"]["update"]["sessionUpdate"]
            .as_str()
            .unwrap_or("?")
            .to_owned();
        if kinds.last() != Some(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}
