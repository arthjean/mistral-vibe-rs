//! A `run_typescript` program, evaluated in the code mode sandbox until it
//! settles, its tool calls run as the turn's own.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::{Value, json};
use vibe_core::clock::now_millis;
use vibe_core::session_id::{random_bytes, uuid_v4};

use super::calls::compact;
use super::execute::{CallOutcome, gated_execute};
use super::{SessionCore, Sink, TurnEnvironment};
use crate::state::{Observation, ToolAction, ToolResult, TransitionTurn};

/// Reference `programmatic_tool_calling`: runs a `run_typescript` program to
/// its end. Each evaluation replays the program against the calls answered so
/// far; the calls it then waits on run as tools of their own, each with its
/// effect entry and its gate, and the next evaluation sees their results.
pub(super) async fn run_program(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    turn_id: &str,
    call_id: &str,
    code: &str,
) -> CallOutcome {
    use crate::code_mode::{self, Facts, Recorded, Settled, Stopped};
    let javascript = match code_mode::transpile(code) {
        Ok(javascript) => javascript,
        Err(error) => return CallOutcome::Done(program_failure(&error, "", "")),
    };
    let message_count = core
        .lock()
        .map(|core| core.conversation.len() + 1)
        .unwrap_or_default();
    let seed_bytes = random_bytes::<4>();
    let tools = crate::programmatic::namespaces(&environment.callables);
    let now = now_millis();
    let mut record: Vec<Recorded> = Vec::new();
    let mut barriers = StartBarriers {
        manager: core.lock().ok().map(|core| Arc::clone(&core.processes)),
        session_id: core
            .lock()
            .map(|core| core.projector.state.session.id.clone())
            .unwrap_or_default(),
        started: Vec::new(),
        applied: Vec::new(),
    };
    for round in 0usize.. {
        let facts = Facts {
            tools: tools.clone(),
            record: record.clone(),
            now,
            seed: u32::from_le_bytes(seed_bytes),
        };
        let program = javascript.clone();
        let evaluated =
            tokio::task::spawn_blocking(move || code_mode::evaluate(&program, &facts)).await;
        let evaluation = match evaluated {
            Ok(Ok(evaluation)) => evaluation,
            Ok(Err(Stopped::Memory { operations })) => {
                let error = json!({
                    "name": "MemoryLimitError",
                    "message": code_mode::memory_message(operations),
                    "details": {"memoryLimitMiB": 16, "operationCount": operations},
                });
                return CallOutcome::Done(program_failure(&error, "", ""));
            }
            Ok(Err(Stopped::Deadline)) => {
                return CallOutcome::FailTurn(code_mode::DEADLINE_MESSAGE.to_owned());
            }
            Err(error) => return CallOutcome::FailTurn(error.to_string()),
        };
        let calls = match evaluation.settled {
            Settled::Returned(value) => {
                return CallOutcome::Done(program_success(
                    &value,
                    &evaluation.stdout,
                    &evaluation.stderr,
                ));
            }
            Settled::Threw(error) => {
                return CallOutcome::Done(program_failure(
                    &error,
                    &evaluation.stdout,
                    &evaluation.stderr,
                ));
            }
            Settled::Pending(calls) => calls,
        };
        // Calls whose arguments do not fit are refused without running;
        // the others start in order and run together.
        let mut slots: Vec<Option<Result<Value, Value>>> = Vec::with_capacity(calls.len());
        let mut started = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let Some(callable) = environment
                .callables
                .iter()
                .find(|callable| callable.qualified() == call.name)
            else {
                slots.push(Some(Err(json!({"name": "Error", "message": format!("Unknown tool function: {}", call.name)}))));
                continue;
            };
            if let Some(error) = crate::programmatic::argument_error(callable, &call.arguments) {
                slots.push(Some(Err(error)));
                continue;
            }
            let action = ToolAction {
                action_id: uuid_v4(),
                turn_id: turn_id.to_owned(),
                tool_name: callable.runtime.to_string(),
                arguments: callable.runtime_arguments(call.arguments.clone()),
            };
            {
                let Ok(mut core) = core.lock() else {
                    return CallOutcome::FailTurn("the session is gone".to_owned());
                };
                let advance = core.projector.apply_action_started(&action, now_millis());
                sink(core.publication(advance, None));
            }
            let operation = format!(
                "typescript:{message_count}:{call_id}:{round}:tool:{}",
                record.len() + index
            );
            barriers.started(&action, &operation);
            started.push((slots.len(), operation, action));
            slots.push(None);
        }
        // The calls this round started are published, which is the step the
        // reference's engine takes together with applying the last results.
        barriers.open_applied();
        let mut running: FuturesUnordered<_> = started
            .into_iter()
            .map(|(slot, operation, action)| async move {
                let outcome = gated_execute(core, environment, sink, &operation, &action).await;
                (slot, action, outcome)
            })
            .collect();
        while let Some((slot, action, outcome)) = running.next().await {
            let result = match outcome {
                CallOutcome::Done(result) => result,
                other => return other,
            };
            slots[slot] = Some(crate::programmatic::settled_value(&result));
            let Ok(mut core) = core.lock() else {
                return CallOutcome::FailTurn("the session is gone".to_owned());
            };
            let input = core.next_input();
            let advance = core.projector.apply(
                input,
                &[Observation::ToolResultCommitted {
                    turn_id: turn_id.to_owned(),
                    action_id: action.action_id.clone(),
                    result,
                }],
                Some(&TransitionTurn::Running(turn_id.to_owned())),
                now_millis(),
            );
            sink(core.publication(advance, None));
            barriers.applied(&action);
        }
        drop(running);
        for (call, slot) in calls.into_iter().zip(slots) {
            record.push(Recorded {
                name: call.name,
                arguments: call.arguments,
                outcome: slot.unwrap_or_else(|| {
                    Err(json!({"name": "Error", "message": "the call did not finish"}))
                }),
            });
        }
    }
    CallOutcome::FailTurn("run_typescript never settled".to_owned())
}

/// The background processes a program started, held until it moves past them.
///
/// The reference's engine applies a call's result and starts the program's
/// next calls in one step, and only then opens the barrier that lets the
/// started process's end be published; a process that ends at once therefore
/// shows after both. Here a result is committed and the next calls start in
/// two publications, so a start is acknowledged after the second. A program
/// that ends, fails or is dropped acknowledges every start it made.
struct StartBarriers {
    manager: Option<Arc<crate::processes::ProcessManager>>,
    session_id: String,
    /// Each `process.start` action started, with the process it names.
    started: Vec<(String, String)>,
    applied: Vec<String>,
}

impl StartBarriers {
    /// `operation` is the call identifier the action runs under, which
    /// [`crate::processes::process_id`] reads.
    fn started(&mut self, action: &ToolAction, operation: &str) {
        if action.tool_name == "process.start" {
            let process_id =
                crate::processes::process_id(&self.session_id, &action.action_id, operation);
            self.started.push((action.action_id.clone(), process_id));
        }
    }

    fn applied(&mut self, action: &ToolAction) {
        if let Some((_, process_id)) = self
            .started
            .iter()
            .find(|(action_id, _)| *action_id == action.action_id)
        {
            self.applied.push(process_id.clone());
        }
    }

    fn open_applied(&mut self) {
        if let Some(manager) = &self.manager {
            for process_id in self.applied.drain(..) {
                manager.acknowledge_start(&process_id);
            }
        }
    }
}

impl Drop for StartBarriers {
    fn drop(&mut self) {
        self.open_applied();
        if let Some(manager) = &self.manager {
            for (_, process_id) in self.started.drain(..) {
                manager.acknowledge_start(&process_id);
            }
        }
    }
}

/// The logs a program wrote, as the blocks after its result.
fn program_logs(stdout: &str, stderr: &str) -> Vec<Value> {
    [("stdout", stdout), ("stderr", stderr)]
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(stream, text)| json!({"type": "text", "text": format!("{stream}:\n{text}")}))
        .collect()
}

/// What a program that returned `value` (compact JSON) answers: the value,
/// or the content blocks it returned, then its logs.
fn program_success(value: &str, stdout: &str, stderr: &str) -> ToolResult {
    let parsed: Value = serde_json::from_str(value).unwrap_or(Value::Null);
    let blocks = parsed
        .as_array()
        .filter(|items| !items.is_empty() && items.iter().all(is_content_block))
        .cloned();
    let mut content = blocks.unwrap_or_else(|| vec![json!({"type": "text", "text": value})]);
    content.extend(program_logs(stdout, stderr));
    ToolResult::Success {
        content,
        structured_content: Some(parsed),
        meta: None,
    }
}

fn is_content_block(value: &Value) -> bool {
    match value["type"].as_str() {
        Some("text") => value["text"].is_string(),
        Some("image" | "audio") => value["data"].is_string() && value["mimeType"].is_string(),
        Some("resource_link") => value["uri"].is_string() && value["name"].is_string(),
        Some("resource") => value["resource"].is_object(),
        _ => false,
    }
}

/// What a program that threw `error` answers.
fn program_failure(error: &Value, stdout: &str, stderr: &str) -> ToolResult {
    let mut content =
        vec![json!({"type": "text", "text": format!("run_typescript failed: {}", compact(error))})];
    content.extend(program_logs(stdout, stderr));
    ToolResult::Failure {
        content,
        code: error["name"]
            .as_str()
            .unwrap_or("ExecutionError")
            .to_owned(),
        message: error["message"]
            .as_str()
            .map_or_else(|| compact(error), str::to_owned),
        details: error
            .get("details")
            .filter(|details| !details.is_null())
            .cloned(),
        retryable: false,
        meta: None,
    }
}
