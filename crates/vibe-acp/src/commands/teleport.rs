//! `/teleport`: hand the session to Vibe Code on the web.
//!
//! Reference `AcpCommandController._teleport` and
//! `vibe/acp/commands/teleport.py`: a tool call follows the operation, a push
//! the workflow needs is put to the client as a permission request, and the
//! prompt response carries the outcome under `_meta.teleport`.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use vibe_app_server::client::{
    LiveNotificationListener, ProgrammaticTeleportEvent, PublicNotification, TurnDriver,
};

use super::{app_server_message, end_turn, text_content};
use crate::agent::AcpAgent;
use crate::agent::turn::uuid;
use crate::protocol::AcpError;
use crate::session::AcpHarness;

const PUSH_OPTION_ID: &str = "teleport_push_and_continue";
const CANCEL_OPTION_ID: &str = "teleport_cancel";

/// Reference `teleport_field_meta`.
fn field_meta(
    status: &str,
    url: Option<&str>,
    unpushed_count: Option<u64>,
    branch_not_pushed: Option<bool>,
) -> Value {
    let mut teleport = json!({"status": status});
    if let Some(url) = url {
        teleport["url"] = json!(url);
    }
    if let Some(count) = unpushed_count {
        teleport["unpushedCount"] = json!(count);
    }
    if let Some(branch) = branch_not_pushed {
        teleport["branchNotPushed"] = json!(branch);
    }
    json!({"tool_name": "teleport", "teleport": teleport})
}

fn status_meta(status: &str) -> Value {
    field_meta(status, None, None, None)
}

/// Reference `_progress`.
fn progress(
    call_id: &str,
    title: &str,
    status: &str,
    text: Option<&str>,
    raw_output: Option<&str>,
    meta: Value,
) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "title": title,
        "kind": "other",
        "status": status,
        "_meta": meta,
    });
    if let Some(text) = text {
        update["content"] = json!([text_content(text)]);
    }
    if let Some(raw_output) = raw_output {
        update["rawOutput"] = json!(raw_output);
    }
    update
}

fn failed_update(call_id: &str, message: &str) -> Value {
    progress(
        call_id,
        "Teleport failed",
        "failed",
        Some(message),
        Some(message),
        status_meta("failed"),
    )
}

/// Reference `teleport_push_question`.
fn push_question(unpushed_count: u64, branch_not_pushed: bool) -> String {
    if branch_not_pushed {
        return "This branch is not on the remote yet. Push it and continue?".to_owned();
    }
    let plural = if unpushed_count == 1 { "" } else { "s" };
    format!("{unpushed_count} local commit{plural} are not pushed. Push them and continue?")
}

/// Reference `teleport_event_update`.
fn event_update(call_id: &str, event: &ProgrammaticTeleportEvent) -> Value {
    let step = |title: &str, status: &str| {
        progress(
            call_id,
            title,
            "in_progress",
            Some(title),
            None,
            status_meta(status),
        )
    };
    match event {
        ProgrammaticTeleportEvent::SummarizingContext { .. } => {
            step("Summarizing the context...", "summarizing_context")
        }
        ProgrammaticTeleportEvent::CheckingGit { .. } => {
            step("Getting the workspace ready...", "preparing_workspace")
        }
        ProgrammaticTeleportEvent::PushRequired {
            unpushed_count,
            branch_not_pushed,
            ..
        } => progress(
            call_id,
            "A push is needed",
            "in_progress",
            Some(&push_question(*unpushed_count, *branch_not_pushed)),
            None,
            field_meta(
                "push_required",
                None,
                Some(*unpushed_count),
                Some(*branch_not_pushed),
            ),
        ),
        ProgrammaticTeleportEvent::Pushing { .. } => {
            step("Syncing with the remote...", "syncing_remote")
        }
        ProgrammaticTeleportEvent::StartingWorkflow { .. } => {
            step("Opening the Vibe Code web session...", "starting_workflow")
        }
        ProgrammaticTeleportEvent::Complete { url, .. } => progress(
            call_id,
            "Session moved to Vibe Code on the web",
            "completed",
            Some(&format!(
                "The session continues on Vibe Code on the web: {url}"
            )),
            Some(url),
            field_meta("completed", Some(url), None, None),
        ),
        ProgrammaticTeleportEvent::Failed { error, .. } => failed_update(call_id, &error.message),
    }
}

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    pub(super) async fn teleport(&self, harness: &AcpHarness<D>) -> Result<Value, AcpError> {
        let opened = self
            .call_async(
                harness,
                "vibeCode/projects/open",
                json!({"purpose": "teleport"}),
            )
            .await;
        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                return Ok(self.reply(
                    harness,
                    &app_server_message(&error),
                    Some(status_meta("unavailable")),
                ));
            }
        };
        let field = |key: &str| {
            opened
                .result
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        let (Some(project_id), Some(picker_id)) = (field("resolvedProjectId"), field("pickerId"))
        else {
            return Ok(self.reply(
                harness,
                "This repository is not linked to a Vibe Code project.",
                Some(status_meta("unavailable")),
            ));
        };
        let call_id = uuid();
        self.session_update(
            &harness.session_id,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": "Moving the session to Vibe Code on the web...",
                "kind": "other",
                "status": "in_progress",
                "content": [text_content("Getting the workspace ready...")],
                "_meta": status_meta("starting"),
            }),
        );
        let operation_id = uuid();
        let mut events = match self
            .start_teleport(harness, &picker_id, &project_id, &operation_id)
            .await
        {
            Ok(events) => events,
            Err(error) => return Ok(self.teleport_failed(harness, &call_id, &error)),
        };
        while let Some(event) = events.recv().await {
            self.session_update(&harness.session_id, event_update(&call_id, &event));
            match event {
                ProgrammaticTeleportEvent::PushRequired {
                    unpushed_count,
                    branch_not_pushed,
                    ..
                } => {
                    let permission = self
                        .call_client(
                            "session/request_permission",
                            json!({
                                "sessionId": harness.session_id,
                                "toolCall": {
                                    "toolCallId": call_id,
                                    "title": push_question(unpushed_count, branch_not_pushed),
                                    "kind": "execute",
                                    "status": "pending",
                                    "_meta": field_meta(
                                        "push_required",
                                        None,
                                        Some(unpushed_count),
                                        Some(branch_not_pushed),
                                    ),
                                },
                                "options": [
                                    {"optionId": PUSH_OPTION_ID, "name": "Push, then continue", "kind": "allow_once"},
                                    {"optionId": CANCEL_OPTION_ID, "name": "Cancel", "kind": "reject_once"},
                                ],
                            }),
                        )
                        .await?;
                    let approved = permission
                        .pointer("/outcome/outcome")
                        .and_then(Value::as_str)
                        == Some("selected")
                        && permission
                            .pointer("/outcome/optionId")
                            .and_then(Value::as_str)
                            == Some(PUSH_OPTION_ID);
                    if let Err(error) = self
                        .call_async(
                            harness,
                            "vibeCode/teleport/push/respond",
                            json!({"operationId": operation_id, "approved": approved}),
                        )
                        .await
                    {
                        return Ok(self.teleport_failed(harness, &call_id, &error));
                    }
                }
                ProgrammaticTeleportEvent::Failed { .. } => {
                    return Ok(end_turn(Some(status_meta("failed"))));
                }
                ProgrammaticTeleportEvent::Complete { url, .. } => {
                    return Ok(end_turn(Some(field_meta(
                        "completed",
                        Some(&url),
                        None,
                        None,
                    ))));
                }
                _ => {}
            }
        }
        Err(AcpError::Internal(
            "the teleport ended without reporting an outcome".to_owned(),
        ))
    }

    /// Starts the run and answers the channel its events arrive on: the
    /// server answers the start first, then publishes each event of the run as
    /// it happens.
    async fn start_teleport(
        &self,
        harness: &AcpHarness<D>,
        picker_id: &str,
        project_id: &str,
        operation_id: &str,
    ) -> Result<UnboundedReceiver<ProgrammaticTeleportEvent>, AcpError> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let wanted = operation_id.to_owned();
        let listener: LiveNotificationListener =
            Arc::new(move |notification: PublicNotification| {
                if notification.method != "vibeCode/teleport/event" {
                    return;
                }
                let Some(event) = notification.params.get("event").cloned().and_then(|event| {
                    serde_json::from_value::<ProgrammaticTeleportEvent>(event).ok()
                }) else {
                    return;
                };
                if event.operation_id() == wanted {
                    let _ = sender.send(event);
                }
            });
        let pending = harness.service.lock().await.begin_public_call(
            "vibeCode/teleport/start",
            json!({
                "sessionId": harness.canonical_id(),
                "pickerId": picker_id,
                "operationId": operation_id,
                "prompt": null,
                "projectId": project_id,
            }),
        )?;
        pending.with_listener(listener).complete().await?;
        Ok(receiver)
    }

    fn teleport_failed(&self, harness: &AcpHarness<D>, call_id: &str, error: &AcpError) -> Value {
        self.session_update(
            &harness.session_id,
            failed_update(call_id, &app_server_message(error)),
        );
        end_turn(Some(status_meta("failed")))
    }
}
