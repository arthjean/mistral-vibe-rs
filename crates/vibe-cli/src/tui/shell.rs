//! A command the operator runs with `!`: reference `ShellResource.run`, which
//! hands it to `session/shellCommand` on either backend. The session runs it
//! behind a `shell` entry, settles that entry, and hands the result to the
//! model itself; the terminal draws the entry as the session publishes it.

use std::time::{Duration, Instant};

use serde_json::json;
use vibe_app_server::client::{ClientError, PublicDispatch, PublicHistoryEntry};

use super::InteractiveRuntime;
use super::hydration::history_entry;
use super::state::TuiState;

const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) struct ActiveShell {
    operation_id: String,
    /// The transcript entry the command's published entry is drawn as, once
    /// the session published it.
    transcript_id: Option<String>,
    last_poll: Instant,
    /// The `session/shellCommand` the command runs under, which answers once
    /// the command settled and its result joined the conversation.
    request: tokio::task::JoinHandle<Result<PublicDispatch, ClientError>>,
}

/// Starts the command `input` names after its `!`, and answers whether the
/// input was consumed.
pub(super) fn start_shell(
    input: &str,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> bool {
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before running shell commands");
        return false;
    };
    let command = input
        .trim_start()
        .strip_prefix('!')
        .map(str::trim)
        .unwrap_or_default();
    if command.is_empty() {
        state.push_diagnostic("No command provided after '!'");
        return true;
    }
    let operation_id = vibe_core::session_id::uuid_v4();
    let params = json!({
        "sessionId": runtime.session_id,
        "command": command,
        "operationId": operation_id,
        "action": "run",
    });
    let request = match runtime
        .service
        .begin_public_call("session/shellCommand", params)
    {
        Ok(pending) => tokio::spawn(pending.complete()),
        Err(error) => {
            state.push_diagnostic(format!("Command failed: {error}"));
            return true;
        }
    };
    state.waiting = true;
    runtime.shell = Some(ActiveShell {
        operation_id,
        transcript_id: None,
        last_poll: Instant::now(),
        request,
    });
    true
}

/// Draws what the session published for the running command, and lets the
/// command go once its request answered.
pub(super) async fn finish_shell(runtime: Option<&mut InteractiveRuntime>, state: &mut TuiState) {
    let Some(runtime) = runtime else {
        return;
    };
    let Some(mut shell) = runtime.shell.take() else {
        return;
    };
    if shell.last_poll.elapsed() < SHELL_POLL_INTERVAL {
        runtime.shell = Some(shell);
        return;
    }
    // Read before the entry: a request that answered has settled it.
    let answered = shell.request.is_finished();
    if let Some(entry) = published_entry(runtime, &shell.operation_id) {
        draw_entry(state, &mut shell.transcript_id, entry);
    }
    if !answered {
        retain_shell(&mut runtime.shell, state, shell);
        return;
    }
    state.waiting = false;
    match shell.request.await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => state.push_diagnostic(format!("Command failed: {error}")),
        Err(error) => state.push_diagnostic(format!("Command failed: {error}")),
    }
}

/// Stops the running command. Its run answers once the interrupt settled it.
pub(super) async fn interrupt_shell(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(shell) = runtime.shell.take() else {
        return;
    };
    let result = runtime
        .service
        .public_call_async(
            "session/shellCommand",
            json!({
                "sessionId": runtime.session_id,
                "operationId": shell.operation_id,
                "action": "interrupt",
            }),
        )
        .await;
    state.prompt_queue.pause();
    if let Err(error) = result {
        state.push_diagnostic(format!("Shell interruption failed: {error}"));
    }
    retain_shell(&mut runtime.shell, state, shell);
}

/// The entry the session projects for the command `operation_id`, which
/// belongs to no turn.
fn published_entry(
    runtime: &mut InteractiveRuntime,
    operation_id: &str,
) -> Option<PublicHistoryEntry> {
    runtime
        .service
        .session(&runtime.session_id)
        .ok()?
        .snapshot?
        .history
        .into_iter()
        .rev()
        .find(|entry| {
            let metadata = entry.metadata();
            metadata.id == operation_id && metadata.turn_id.is_none()
        })
}

/// Draws the command's published entry: appended the first time it is
/// seen, replaced after that until it settled.
pub(super) fn draw_entry(
    state: &mut TuiState,
    transcript_id: &mut Option<String>,
    entry: PublicHistoryEntry,
) {
    let entry = history_entry(entry);
    match transcript_id {
        Some(id) => {
            let _ = state.replace_local(id, entry);
        }
        None => *transcript_id = Some(state.append_local(entry)),
    }
}

fn retain_shell(
    shell_slot: &mut Option<ActiveShell>,
    state: &mut TuiState,
    mut shell: ActiveShell,
) {
    shell.last_poll = Instant::now();
    *shell_slot = Some(shell);
    state.waiting = true;
}

#[cfg(test)]
pub(super) mod shell_tests;
