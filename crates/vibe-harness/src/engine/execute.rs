//! Running one call: the gate that decides it, the callbacks it opens, and
//! the harness tool that answers it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use vibe_core::clock::now_millis;
use vibe_core::pycompat::{self, Dumps};

use super::{CallbackEvent, SessionCore, Sink, TurnEnvironment};
use crate::gate::{
    Approver, AskFuture, CallbackAnswer, GateVerdict, approval_callback, user_input_callback,
};
use crate::state::{Advance, ToolAction, ToolResult};

/// How one call ended for the turn.
pub(super) enum CallOutcome {
    Done(ToolResult),
    CancelTurn,
    FailTurn(String),
}

/// Reference `_gate_tool_call` then the tool: the gate decides, the call
/// runs or is refused, and its result carries the approval record.
pub(super) async fn gated_execute(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
) -> CallOutcome {
    // Reference `_process_actions`: process arguments are checked before any
    // approval is asked, and pacing the agent itself asks none.
    let precheck = crate::processes::precheck(
        &action.tool_name,
        &action.arguments,
        &environment.workspace.cwd,
    );
    if let Some(refused) = precheck {
        return CallOutcome::Done(refused);
    }
    if action.tool_name == "self.sleep" || action.tool_name.starts_with("subagent.") {
        return match execute(core, environment, sink, call_id, action).await {
            Ok(result) => CallOutcome::Done(result),
            Err(message) => CallOutcome::FailTurn(message),
        };
    }
    let approver = EngineApprover {
        core,
        sink,
        action,
        call_id,
    };
    let (mut result, approval) = match environment.gate.check(action, &approver).await {
        GateVerdict::Run { approval } => {
            match execute(core, environment, sink, call_id, action).await {
                Ok(result) => (result, approval),
                Err(message) => return CallOutcome::FailTurn(message),
            }
        }
        GateVerdict::Refuse { approval, message } => {
            (ToolResult::failed("tool_denied", message), approval)
        }
        GateVerdict::CancelTurn => return CallOutcome::CancelTurn,
        GateVerdict::FailTurn { message } => return CallOutcome::FailTurn(message),
    };
    let mut approval = approval;
    // Reference `_with_approval_note`: a note on why the call ran rides the
    // result's metadata, apart from the approval record.
    let note = approval
        .as_object_mut()
        .and_then(|record| record.remove("note"));
    let meta = result.meta_mut().get_or_insert_with(Map::new);
    meta.insert("approval".to_owned(), approval);
    if let Some(note) = note.filter(Value::is_string) {
        meta.insert("approval_note".to_owned(), note);
    }
    CallOutcome::Done(result)
}

/// Opens the callbacks one call asks.
struct EngineApprover<'a> {
    core: &'a Arc<Mutex<SessionCore>>,
    sink: &'a Sink,
    action: &'a ToolAction,
    call_id: &'a str,
}

impl Approver for EngineApprover<'_> {
    fn ask(&self, required_permissions: Vec<Value>, reason: Option<String>) -> AskFuture<'_> {
        Box::pin(async move {
            let receiver = {
                let Ok(mut core) = self.core.lock() else {
                    return CallbackAnswer::Closed("the session is gone".to_owned());
                };
                let callback_id = core
                    .callbacks
                    .fresh_id(&format!("approval-{}", self.call_id));
                let entry = approval_callback(
                    &core.projector.state.session.id,
                    &callback_id,
                    self.action,
                    now_millis(),
                    core.input_entry_of(&self.action.turn_id).as_deref(),
                    reason.as_deref(),
                    required_permissions,
                );
                open_callback(&mut core, self.sink, entry)
            };
            receiver
                .await
                .unwrap_or_else(|_| CallbackAnswer::Closed("Turn interrupted".to_owned()))
        })
    }
}

/// Opens `entry`, publishes it, and answers where its answer arrives.
fn open_callback(
    core: &mut SessionCore,
    sink: &Sink,
    entry: Value,
) -> tokio::sync::oneshot::Receiver<CallbackAnswer> {
    let receiver = core.callbacks.open(entry.clone());
    sink(core.publication(Advance::Provisional, Some(CallbackEvent::Requested(entry))));
    receiver
}

/// Reference `_request_user_input`: the questions go to the person, and
/// their answers are the result. A callback closed without answers fails the
/// turn.
async fn ask_user(
    core: &Arc<Mutex<SessionCore>>,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
) -> Result<ToolResult, String> {
    let receiver = {
        let Ok(mut core) = core.lock() else {
            return Err("the session is gone".to_owned());
        };
        let callback_id = core.callbacks.fresh_id(&format!("user-input-{call_id}"));
        let entry = user_input_callback(
            &core.projector.state.session.id,
            &callback_id,
            action,
            call_id,
            now_millis(),
            core.input_entry_of(&action.turn_id).as_deref(),
        );
        open_callback(&mut core, sink, entry)
    };
    match receiver.await {
        Ok(CallbackAnswer::Output(output)) => {
            let answers = crate::tools::vibe::question_result(&output["result"]);
            Ok(ToolResult::Success {
                content: vec![
                    json!({"type": "text", "text": pycompat::dumps(&answers, Dumps::DEFAULT)}),
                ],
                structured_content: Some(answers),
                meta: None,
            })
        }
        Ok(CallbackAnswer::Closed(message)) => Err(message),
        Err(_) => Err("Callback delivery failed".to_owned()),
    }
}

async fn execute(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
) -> Result<ToolResult, String> {
    let arguments = action.arguments.clone();
    Ok(match action.tool_name.as_str() {
        "ui.ask_user_question" => return ask_user(core, sink, call_id, action).await,
        name @ ("file_system.read_file"
        | "file_system.write_file"
        | "file_system.search_replace") => {
            let name = name.to_owned();
            let workspace = environment.workspace.clone();
            tokio::task::spawn_blocking(move || {
                crate::tools::files::execute(&name, &arguments, &workspace)
            })
            .await
            .unwrap_or_else(|error| ToolResult::failed("tool_failed", error.to_string()))
        }
        "file_system.bash" => crate::tools::shell::execute(&arguments, &environment.shell).await,
        name if name.starts_with("process.") => {
            let (manager, session_id) = match core.lock() {
                Ok(core) => (
                    Arc::clone(&core.processes),
                    core.projector.state.session.id.clone(),
                ),
                Err(_) => return Ok(ToolResult::failed("tool_failed", "the session is gone")),
            };
            let operation = name.trim_start_matches("process.").to_owned();
            let process_id = crate::processes::process_id(&session_id, &action.action_id, call_id);
            let workspace = environment.workspace.cwd.clone();
            let session_dir = environment.session_dir.clone();
            let shell = environment.shell.clone();
            tokio::task::spawn_blocking(move || match operation.as_str() {
                "start" => manager.start(
                    process_id,
                    &arguments,
                    &workspace,
                    session_dir.as_deref(),
                    &shell,
                ),
                "output" => manager.output(&arguments),
                "write" => manager.write(&arguments),
                "list" => manager.list(),
                "stop" => manager.stop(&arguments),
                other => {
                    ToolResult::failed("tool_failed", format!("Unsupported tool: process.{other}"))
                }
            })
            .await
            .unwrap_or_else(|error| ToolResult::failed("tool_failed", error.to_string()))
        }
        name if name.starts_with("subagent.") => {
            let (manager, session_id, message_id, scratchpad) = match core.lock() {
                Ok(core) => (
                    Arc::clone(&core.subagents),
                    core.projector.state.session.id.clone(),
                    core.latest_input_entry_id(),
                    core.vibe.scratchpad.clone(),
                ),
                Err(_) => return Ok(ToolResult::failed("tool_failed", "the session is gone")),
            };
            let operation = name.trim_start_matches("subagent.");
            manager
                .execute(
                    operation,
                    &arguments,
                    &action.action_id,
                    &session_id,
                    environment,
                    message_id,
                    &scratchpad,
                )
                .await
        }
        "self.sleep" => {
            let seconds = arguments["seconds"].as_f64().unwrap_or_default();
            tokio::time::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 60.0))).await;
            ToolResult::Success {
                content: Vec::new(),
                structured_content: Some(json!({"seconds": arguments["seconds"]})),
                meta: None,
            }
        }
        "skill.read" => {
            let name = arguments["name"].as_str().unwrap_or_default();
            match environment.skills.iter().find(|(known, _)| known == name) {
                Some((_, body)) => ToolResult::Success {
                    content: vec![json!({"type": "text", "text": body})],
                    structured_content: Some(json!(body)),
                    meta: None,
                },
                None => {
                    let mut names: Vec<&str> = environment
                        .skills
                        .iter()
                        .map(|(known, _)| known.as_str())
                        .collect();
                    names.sort_unstable();
                    let listing = if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    };
                    ToolResult::failed(
                        "tool_failed",
                        format!(
                            "Skill \"{name}\" is not available to this runtime. Available skills: {listing}"
                        ),
                    )
                }
            }
        }
        "vibe.cron" => match core.lock() {
            Ok(core) => core.loops.run_tool(&arguments),
            Err(_) => ToolResult::failed("tool_failed", "the session is gone"),
        },
        name if name.starts_with("vibe.") => {
            let tool = name.trim_start_matches("vibe.");
            match core.lock() {
                Ok(mut core) => core.vibe.execute(tool, &arguments),
                Err(_) => ToolResult::failed("tool_failed", "the session is gone"),
            }
        }
        other => match &environment.provided {
            Some(provided) if provided.routes(other) => provided.call(other, arguments).await,
            _ => ToolResult::failed("tool_failed", format!("Unsupported tool: {other}")),
        },
    })
}
