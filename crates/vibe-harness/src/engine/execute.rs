//! Running one call: the gate that decides it, the callbacks it opens, and
//! the harness tool that answers it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use vibe_core::clock::now_millis;
use vibe_core::pycompat::{self, Dumps};
use vibe_core::sync::lock;

use super::{CallbackEvent, SessionCore, Sink, TurnEnvironment};
use crate::gate::{
    Approver, AskFuture, CallbackAnswer, GateVerdict, approval_callback, user_input_callback,
};
use crate::processes::Operation;
use crate::state::{Advance, ToolAction, ToolResult};
use crate::tools::files::FileTool;
use crate::tools::vibe::VibeTool;

/// How one call ended for the turn.
pub(super) enum CallOutcome {
    Done(ToolResult),
    CancelTurn,
    FailTurn(String),
}

/// The harness tool a call names, read once from its dotted name.
#[derive(Debug, Clone, Copy)]
enum HarnessTool<'a> {
    AskUserQuestion,
    File(FileTool),
    Bash,
    Process(Operation),
    Subagent(&'a str),
    Sleep,
    SkillRead,
    Cron,
    Vibe(VibeTool),
    /// A tool the host provides, or one nothing here runs.
    Other(&'a str),
}

impl<'a> HarnessTool<'a> {
    fn parse(name: &'a str) -> Self {
        match name {
            "ui.ask_user_question" => Self::AskUserQuestion,
            "file_system.read_file" => Self::File(FileTool::ReadFile),
            "file_system.write_file" => Self::File(FileTool::WriteFile),
            "file_system.search_replace" => Self::File(FileTool::SearchReplace),
            "file_system.bash" => Self::Bash,
            "self.sleep" => Self::Sleep,
            "skill.read" => Self::SkillRead,
            "vibe.cron" => Self::Cron,
            "vibe.todo" => Self::Vibe(VibeTool::Todo),
            "vibe.unified_harness_scratchpad" => Self::Vibe(VibeTool::Scratchpad),
            _ => match name.split_once('.') {
                Some(("process", operation)) => {
                    Operation::parse(operation).map_or(Self::Other(name), Self::Process)
                }
                Some(("subagent", operation)) => Self::Subagent(operation),
                _ => Self::Other(name),
            },
        }
    }
}

/// Reference `_gate_tool_call` then the tool: the gate decides, the call
/// runs or is refused, and its result carries the approval record.
pub(super) async fn gated_execute(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
    file_lane: &Semaphore,
) -> CallOutcome {
    // Reference `_process_actions`: process arguments are checked before any
    // approval is asked, and pacing the agent itself asks none.
    let tool = HarnessTool::parse(&action.tool_name);
    if let HarnessTool::Process(operation) = tool
        && let Some(refused) =
            crate::processes::precheck(operation, &action.arguments, &environment.workspace.cwd)
    {
        return CallOutcome::Done(refused);
    }
    if matches!(tool, HarnessTool::Sleep | HarnessTool::Subagent(_)) {
        return match execute(core, environment, sink, call_id, tool, action, file_lane).await {
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
            match execute(core, environment, sink, call_id, tool, action, file_lane).await {
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
                let mut core = lock(self.core);
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
        let mut core = lock(core);
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
    tool: HarnessTool<'_>,
    action: &ToolAction,
    file_lane: &Semaphore,
) -> Result<ToolResult, String> {
    let arguments = action.arguments.clone();
    Ok(match tool {
        HarnessTool::AskUserQuestion => return ask_user(core, sink, call_id, action).await,
        HarnessTool::File(file_tool) => {
            // The file calls of one batch run one at a time, in the order they
            // reach this point. The reference's single event loop hands them
            // to its thread pool in call order and they commit in it; parallel
            // blocking threads here would commit them in whichever order the
            // reads happened to finish.
            let _lane = file_lane.acquire().await.ok();
            let workspace = environment.workspace.clone();
            tokio::task::spawn_blocking(move || {
                crate::tools::files::execute(file_tool, &arguments, &workspace)
            })
            .await
            .unwrap_or_else(|error| ToolResult::failed("tool_failed", error.to_string()))
        }
        HarnessTool::Bash => crate::tools::shell::execute(&arguments, &environment.shell).await,
        HarnessTool::Process(operation) => {
            let (manager, session_id) = {
                let core = lock(core);
                (
                    Arc::clone(&core.processes),
                    core.projector.state.session.id.clone(),
                )
            };
            let process_id = crate::processes::process_id(&session_id, &action.action_id, call_id);
            let workspace = environment.workspace.cwd.clone();
            let session_dir = environment.session_dir.clone();
            let shell = environment.shell.clone();
            tokio::task::spawn_blocking(move || match operation {
                Operation::Start => manager.start(
                    process_id,
                    &arguments,
                    &workspace,
                    session_dir.as_deref(),
                    &shell,
                ),
                Operation::Output => manager.output(&arguments),
                Operation::Write => manager.write(&arguments),
                Operation::List => manager.list(),
                Operation::Stop => manager.stop(&arguments),
            })
            .await
            .unwrap_or_else(|error| ToolResult::failed("tool_failed", error.to_string()))
        }
        HarnessTool::Subagent(operation) => {
            let (manager, session_id, message_id, scratchpad) = {
                let core = lock(core);
                (
                    Arc::clone(&core.subagents),
                    core.projector.state.session.id.clone(),
                    core.latest_input_entry_id(),
                    core.vibe.scratchpad.clone(),
                )
            };
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
        HarnessTool::Sleep => {
            let seconds = arguments["seconds"].as_f64().unwrap_or_default();
            tokio::time::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 60.0))).await;
            ToolResult::Success {
                content: Vec::new(),
                structured_content: Some(json!({"seconds": arguments["seconds"]})),
                meta: None,
            }
        }
        HarnessTool::SkillRead => {
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
        HarnessTool::Cron => lock(core).loops.run_tool(&arguments),
        HarnessTool::Vibe(tool) => lock(core).vibe.execute(tool, &arguments),
        HarnessTool::Other(other) => match &environment.provided {
            Some(provided) if provided.routes(other) => provided.call(other, arguments).await,
            _ => ToolResult::failed("tool_failed", format!("Unsupported tool: {other}")),
        },
    })
}
