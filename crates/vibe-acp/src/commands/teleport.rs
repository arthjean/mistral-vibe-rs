//! `/teleport`: hand the session over to Vibe Code Web.

use std::future::Future;

use serde_json::{Value, json};
use vibe_app_server::client::{
    HeadlessService, ProgrammaticTeleportEvent, PublicNotification, TurnDriver,
};

use crate::agent::AcpAgent;
use crate::commands::CommandSink;
use crate::history::all_history_entries;
use crate::protocol::AcpError;
use crate::session::now_millis;

const MAX_SUMMARY_CHARS: usize = 32 * 1024;
const MAX_SUMMARY_ENTRIES: usize = 32;
const MAX_FAILURE_CHARS: usize = 4_096;

pub(crate) async fn execute<D>(
    agent: &AcpAgent<D>,
    sink: &CommandSink<'_, D>,
) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    let call_id = sink.harness.next_local_id("teleport");
    // The cloud operation ID is an idempotency key across processes, so it
    // carries the wall clock and PID on top of the process-unique call ID.
    let operation_id = format!("{call_id}-{}-{}", std::process::id(), now_millis());
    let mut service = sink.harness.service.lock().await;
    let history = match all_history_entries(&mut service, sink.session_id()) {
        Ok(history) if !history.is_empty() => history,
        Ok(_) => return sink.reply("No conversation history to teleport."),
        Err(error) => {
            return sink.reply(&format!(
                "Teleport could not read conversation history: {error}"
            ));
        }
    };
    let summary = summarize(&history);
    sink.tool_update(announce(&call_id))?;

    let opened = service.public_call_async(
        "vibeCode/projects/open",
        json!({
            "sessionId": sink.session_id(),
            "workingDirectory": sink.harness.cwd,
            "purpose": "teleport",
        }),
    );
    let Some(opened) = until_cancelled(sink, opened).await else {
        return cancelled(sink, &call_id);
    };
    let opened = match opened {
        Ok(opened) => opened,
        Err(error) => return failed(sink, &call_id, &error.to_string()),
    };
    let Some(picker_id) = opened.result.get("pickerId").and_then(Value::as_str) else {
        return failed(
            sink,
            &call_id,
            "Vibe Code project discovery omitted its picker ID",
        );
    };
    let Some(project_id) = opened
        .result
        .get("resolvedProjectId")
        .and_then(Value::as_str)
    else {
        return failed(
            sink,
            &call_id,
            "No Vibe Code project is linked to this repository.",
        );
    };

    let started = service.public_call_async(
        "vibeCode/teleport/start",
        json!({
            "sessionId": sink.session_id(),
            "pickerId": picker_id.to_owned(),
            "projectId": project_id.to_owned(),
            "operationId": operation_id,
            "workingDirectory": sink.harness.cwd,
            "prompt": summary,
        }),
    );
    let Some(started) = until_cancelled(sink, started).await else {
        return abort(sink, &mut service, &operation_id, &call_id);
    };
    let started = match started {
        Ok(started) => started,
        Err(error) => return failed(sink, &call_id, &error.to_string()),
    };
    let events = events_from(&started.notifications)?;
    emit(sink, &call_id, &events)?;
    if !events
        .iter()
        .any(|event| matches!(event, ProgrammaticTeleportEvent::PushRequired { .. }))
    {
        return Ok(());
    }

    drop(service);
    let permission = agent.request_permission(
        sink.session_id(),
        json!({
            "toolCallId": call_id,
            "title": "Push local commits before Teleport?",
            "kind": "execute",
            "rawInput": {"operationId": operation_id},
            "_meta": {"tool_name": "teleport", "teleport": {"status": "push_required"}},
        }),
        vec![
            json!({
                "optionId": "teleport_push_and_continue",
                "name": "Push and continue",
                "kind": "allow_once",
            }),
            json!({
                "optionId": "teleport_cancel",
                "name": "Cancel Teleport",
                "kind": "reject_once",
            }),
        ],
    );
    let approved = match until_cancelled(sink, permission).await {
        Some(permission) => selected_option(permission.ok().as_ref())
            .is_some_and(|option| option == "teleport_push_and_continue"),
        None => {
            let mut service = sink.harness.service.lock().await;
            return abort(sink, &mut service, &operation_id, &call_id);
        }
    };

    let mut service = sink.harness.service.lock().await;
    let responded = service.public_call_async(
        "vibeCode/teleport/push/respond",
        json!({
            "sessionId": sink.session_id(),
            "operationId": operation_id,
            "approved": approved,
        }),
    );
    let Some(responded) = until_cancelled(sink, responded).await else {
        return abort(sink, &mut service, &operation_id, &call_id);
    };
    match responded {
        Ok(responded) => emit(sink, &call_id, &events_from(&responded.notifications)?),
        Err(error) => failed(sink, &call_id, &error.to_string()),
    }
}

/// Runs cloud work while `/cancel` stays observable. `None` means the session
/// was cancelled before the work finished.
async fn until_cancelled<D, T>(
    sink: &CommandSink<'_, D>,
    work: impl Future<Output = T>,
) -> Option<T>
where
    D: TurnDriver,
{
    tokio::select! {
        result = work => Some(result),
        () = sink.harness.cancelled() => None,
    }
}

fn selected_option(response: Option<&Value>) -> Option<&str> {
    response
        .filter(|response| {
            response.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected")
        })?
        .pointer("/outcome/optionId")?
        .as_str()
}

/// Opening tool call every later update revises.
fn announce(call_id: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": "Teleporting session to Vibe Code",
        "kind": "other",
        "status": "in_progress",
        "_meta": {"tool_name": "teleport", "teleport": {"status": "starting"}},
    })
}

fn update(
    call_id: &str,
    title: &str,
    status: &str,
    teleport_status: &str,
    raw_output: Value,
) -> Value {
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "title": title,
        "kind": "other",
        "status": status,
        "rawOutput": raw_output,
        "_meta": {
            "tool_name": "teleport",
            "teleport": {"status": teleport_status},
        },
    })
}

fn failed<D>(sink: &CommandSink<'_, D>, call_id: &str, message: &str) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    let message = message.chars().take(MAX_FAILURE_CHARS).collect::<String>();
    sink.tool_update(update(
        call_id,
        "Teleport failed",
        "failed",
        "failed",
        json!({"message": message}),
    ))
}

fn cancelled<D>(sink: &CommandSink<'_, D>, call_id: &str) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    sink.tool_update(update(
        call_id,
        "Teleport cancelled",
        "failed",
        "failed",
        json!({"message": "Teleport was cancelled; the local session remains available."}),
    ))
}

/// Cancels the cloud operation before reporting the cancellation locally.
fn abort<D>(
    sink: &CommandSink<'_, D>,
    service: &mut HeadlessService<D>,
    operation_id: &str,
    call_id: &str,
) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    let _ = service.public_call(
        "vibeCode/teleport/cancel",
        json!({"sessionId": sink.session_id(), "operationId": operation_id}),
    );
    cancelled(sink, call_id)
}

fn emit<D>(
    sink: &CommandSink<'_, D>,
    call_id: &str,
    events: &[ProgrammaticTeleportEvent],
) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    for event in events {
        let (title, status, teleport_status, raw_output) = match event {
            ProgrammaticTeleportEvent::SummarizingContext { .. } => (
                "Summarizing session context",
                "in_progress",
                "summarizing_context",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::CheckingGit { .. } => (
                "Checking Git repository",
                "in_progress",
                "preparing_workspace",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::PushRequired {
                unpushed_count,
                branch_not_pushed,
                ..
            } => (
                "Git push approval required",
                "in_progress",
                "push_required",
                json!({
                    "unpushedCount": unpushed_count,
                    "branchNotPushed": branch_not_pushed,
                }),
            ),
            ProgrammaticTeleportEvent::Pushing { .. } => (
                "Pushing Git changes",
                "in_progress",
                "syncing_remote",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::StartingWorkflow { .. } => (
                "Starting Vibe Code workflow",
                "in_progress",
                "starting_workflow",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::Complete { url, .. } => (
                "Session teleported to Vibe Code",
                "completed",
                "completed",
                json!({"url": url}),
            ),
            ProgrammaticTeleportEvent::Failed { error, .. } => (
                "Teleport failed",
                "failed",
                "failed",
                json!({"message": error.message, "code": error.code}),
            ),
        };
        sink.tool_update(update(call_id, title, status, teleport_status, raw_output))?;
    }
    Ok(())
}

fn events_from(
    notifications: &[PublicNotification],
) -> Result<Vec<ProgrammaticTeleportEvent>, AcpError> {
    notifications
        .iter()
        .filter(|notification| notification.method == "vibeCode/teleport/event")
        .map(|notification| {
            notification
                .params
                .get("event")
                .cloned()
                .ok_or_else(|| {
                    AcpError::InvalidResponse("Teleport notification omitted its event".to_owned())
                })
                .and_then(|event| serde_json::from_value(event).map_err(AcpError::Json))
        })
        .collect()
}

/// Trailing conversation excerpt handed to the cloud workflow as its prompt.
fn summarize(history: &[Value]) -> String {
    let lines = history
        .iter()
        .rev()
        .filter_map(|entry| {
            let role = entry.get("role").and_then(Value::as_str)?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let text = match entry.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect(),
                _ => String::new(),
            };
            (!text.trim().is_empty()).then(|| format!("{role}: {text}"))
        })
        .take(MAX_SUMMARY_ENTRIES)
        .collect::<Vec<_>>();
    let summary = lines
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n\n")
        .chars()
        .take(MAX_SUMMARY_CHARS)
        .collect::<String>();
    if summary.is_empty() {
        "Continue this session in Vibe Code".to_owned()
    } else {
        summary
    }
}
