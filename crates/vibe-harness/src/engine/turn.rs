//! One turn: model calls and the tool calls they ask for, until the model
//! answers without asking for one, the turn is interrupted, or it fails.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::json;
use tokio::sync::watch;
use vibe_core::clock::now_millis;
use vibe_core::events::{ModelMessage, ModelToolCall};
use vibe_core::provider::{ProviderInput, RequestDialect, ToolDefinition};
use vibe_core::session_id::uuid_v4;
use vibe_core::sync::lock;

use super::calls::{CallPlan, PreparedCall, invalid_call_content, model_content, prepare_calls};
use super::compaction_run::{CompactionEnd, needs_compaction, run_compaction};
use super::conversation::ConversationMessage;
use super::execute::{CallOutcome, gated_execute};
use super::program::run_program;
use super::provider_errors::turn_error;
use super::stream::{Completion, stream_completion};
use super::{CallbackEvent, SessionCore, Sink, TurnEnd, TurnEnvironment};
use crate::state::{Advance, Observation, PublicError, ReasoningPart, ToolAction, TransitionTurn};

/// A call the turn runs once the answer asking for it commits.
enum Started {
    /// A harness tool, under the running entry it was published with.
    Tool(ToolAction),
    /// A `run_typescript` program.
    Program(String),
}

/// What the turn does before its next model call.
enum NextCall {
    /// Call the model with this input.
    Call(Box<ProviderInput>),
    /// Compact the conversation first.
    Compact,
    /// End the turn on this error without calling the model.
    Refuse(PublicError),
}

/// Runs the turn `turn_id` to its end. `interrupt` turns to the reason the
/// turn is interrupted for.
pub async fn run_turn(
    core: Arc<Mutex<SessionCore>>,
    turn_id: String,
    environment: TurnEnvironment,
    sink: Sink,
    mut interrupt: watch::Receiver<Option<String>>,
) -> TurnEnd {
    let publish = |core: &SessionCore, advance: Advance| {
        sink(core.publication(advance, None));
    };
    let mut first_call = true;
    let mut iterations: u32 = 0;
    loop {
        let action_id = uuid_v4();
        let next = {
            let mut core = lock(&core);
            core.deliver_notifications();
            let message_id = core.latest_input_entry_id().unwrap_or_default();
            match unsupported_image(&core, &environment) {
                Some(error) => NextCall::Refuse(error),
                None if needs_compaction(&core, &environment) => NextCall::Compact,
                None => NextCall::Call(Box::new(provider_input(
                    &core,
                    &environment,
                    first_call,
                    &message_id,
                ))),
            }
        };
        let input = match next {
            NextCall::Call(input) => *input,
            NextCall::Refuse(error) => return fail_turn(&core, &turn_id, &sink, error),
            NextCall::Compact => {
                let compacted = tokio::select! {
                    end = run_compaction(&core, &environment, &sink, "", Some(&turn_id)) => end,
                    reason = interrupted(&mut interrupt) => return interrupt_turn(&core, &turn_id, &sink, &reason),
                };
                match compacted {
                    CompactionEnd::Compacted { .. } => continue,
                    CompactionEnd::Failed { code, message } => {
                        return TurnEnd::Failed(PublicError {
                            code,
                            message,
                            details: None,
                        });
                    }
                }
            }
        };
        first_call = false;
        iterations = iterations.saturating_add(1);
        let streamed = tokio::select! {
            streamed = stream_completion(&core, &environment, &input, &turn_id, &action_id, &sink) => streamed,
            reason = interrupted(&mut interrupt) => return interrupt_turn(&core, &turn_id, &sink, &reason),
        };
        let completion = match streamed {
            Ok(completion) => completion,
            Err(error) => {
                return fail_turn(&core, &turn_id, &sink, turn_error(&error, &environment));
            }
        };
        let calls = prepare_calls(&completion.tool_calls, &environment);
        let committed = {
            let mut core = lock(&core);
            commit_answer(
                &mut core,
                &environment,
                &sink,
                &turn_id,
                &action_id,
                &completion,
                &calls,
            )
        };
        let Some(Committed {
            started,
            mut searched,
        }) = committed
        else {
            return TurnEnd::Completed;
        };
        // Every call answers the model, in the order it asked; a result is
        // committed as soon as its call finishes.
        let mut answers: BTreeMap<String, String> = calls
            .iter()
            .filter_map(|call| match &call.plan {
                CallPlan::Invalid(message) => {
                    Some((call.call_id.clone(), invalid_call_content(message)))
                }
                _ => None,
            })
            .collect();
        answers.append(&mut searched);
        if let Err(end) = run_calls(
            &core,
            &environment,
            &sink,
            &turn_id,
            started,
            &mut interrupt,
            &mut answers,
        )
        .await
        {
            return end;
        }
        let mut core = lock(&core);
        for call in &calls {
            core.conversation.push(ConversationMessage::Tool {
                call_id: call.call_id.clone(),
                name: call.model_name.clone(),
                content: answers.remove(&call.call_id).unwrap_or_default(),
            });
        }
        if let Some(advance) = core.deliver_steering(&turn_id) {
            publish(&core, advance);
        }
        if environment
            .max_iterations
            .is_some_and(|max| iterations >= max)
        {
            let input = core.next_input();
            let advance = core.projector.apply(
                input,
                &[Observation::TurnCompleted {
                    turn_id: turn_id.clone(),
                }],
                Some(&TransitionTurn::Settled),
                now_millis(),
            );
            publish(&core, advance);
            return TurnEnd::Completed;
        }
    }
}

/// What committing an answer leaves the turn to do.
struct Committed {
    /// The calls to run, in the order the model asked for them.
    started: Vec<(String, Started)>,
    /// What each search the engine answered itself found, by call.
    searched: BTreeMap<String, String>,
}

/// Commits the answer `completion`, which asked for `calls`: its message,
/// the searches the engine answers itself, and a running entry for each
/// harness tool it starts. `None` when the answer completes the turn.
fn commit_answer(
    core: &mut SessionCore,
    environment: &TurnEnvironment,
    sink: &Sink,
    turn_id: &str,
    action_id: &str,
    completion: &Completion,
    calls: &[PreparedCall],
) -> Option<Committed> {
    let input = core.next_input();
    // Reference `programmatic_tool_calling`: an answer that runs a program
    // commits nothing of its own until the program ends.
    let runs_program = calls
        .iter()
        .any(|call| matches!(call.plan, CallPlan::Program(_)));
    let mut observations = Vec::new();
    if !runs_program {
        observations.push(Observation::AssistantMessageCommitted {
            turn_id: turn_id.to_owned(),
            action_id: action_id.to_owned(),
            reasoning: completion
                .reasoning
                .iter()
                .map(|text| ReasoningPart {
                    text: vec![text.clone()],
                    summary: Vec::new(),
                    redacted: false,
                })
                .collect(),
            content: if completion.text.is_empty() {
                Vec::new()
            } else {
                vec![json!({"type": "text", "text": completion.text})]
            },
            usage: completion.usage,
        });
    }
    let mut searched = BTreeMap::new();
    for call in calls {
        if let CallPlan::Search(request) = &call.plan {
            let answer = crate::discovery::search(&environment.functions, request.clone());
            observations.push(Observation::ToolDiscoveryFinished {
                turn_id: turn_id.to_owned(),
                call_id: call.call_id.clone(),
                mode: answer.mode.as_str().to_owned(),
                tool_count: answer.tool_count as u64,
                connector_notice_count: answer.connector_notice_count as u64,
            });
            searched.insert(call.call_id.clone(), answer.content);
        }
    }
    let done = calls.is_empty() && core.pending_steer.is_empty();
    let transition = if done {
        observations.push(Observation::TurnCompleted {
            turn_id: turn_id.to_owned(),
        });
        TransitionTurn::Settled
    } else {
        TransitionTurn::Running(turn_id.to_owned())
    };
    let advance = core
        .projector
        .apply(input, &observations, Some(&transition), now_millis());
    core.conversation.push(ConversationMessage::Assistant {
        text: completion.text.clone(),
        reasoning: completion.reasoning.clone(),
        tool_calls: calls
            .iter()
            .map(|call| ModelToolCall {
                id: call.call_id.clone(),
                name: call.model_name.clone(),
                arguments: call.wire_arguments.clone(),
                presentation: None,
            })
            .collect(),
    });
    sink(core.publication(advance, None));
    if done {
        return None;
    }
    let mut started = Vec::new();
    for call in calls {
        match &call.plan {
            CallPlan::Harness { name, arguments } => {
                let action = ToolAction {
                    action_id: uuid_v4(),
                    turn_id: turn_id.to_owned(),
                    tool_name: (*name).to_owned(),
                    arguments: arguments.clone(),
                };
                let advance = core.projector.apply_action_started(&action, now_millis());
                sink(core.publication(advance, None));
                started.push((call.call_id.clone(), Started::Tool(action)));
            }
            CallPlan::Program(code) => {
                started.push((call.call_id.clone(), Started::Program(code.clone())));
            }
            CallPlan::Search(_) | CallPlan::Invalid(_) => {}
        }
    }
    Some(Committed { started, searched })
}

/// Runs the `started` calls together, each answer into `answers` by call
/// and each harness tool's result committed as soon as its call finishes.
/// `Err` holds how the turn ended when a call or an interrupt ended it.
async fn run_calls(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    turn_id: &str,
    started: Vec<(String, Started)>,
    interrupt: &mut watch::Receiver<Option<String>>,
    answers: &mut BTreeMap<String, String>,
) -> Result<(), TurnEnd> {
    let mut running: FuturesUnordered<_> = started
        .into_iter()
        .map(|(call_id, started)| async move {
            let outcome = match &started {
                Started::Tool(action) => {
                    gated_execute(core, environment, sink, &call_id, action).await
                }
                Started::Program(code) => {
                    run_program(core, environment, sink, turn_id, &call_id, code).await
                }
            };
            (call_id, started, outcome)
        })
        .collect();
    loop {
        let next = tokio::select! {
            next = running.next() => next,
            reason = interrupted(interrupt) => return Err(interrupt_turn(core, turn_id, sink, &reason)),
        };
        let Some((call_id, started, outcome)) = next else {
            return Ok(());
        };
        let result = match outcome {
            CallOutcome::Done(result) => result,
            CallOutcome::CancelTurn => {
                return Err(interrupt_turn(core, turn_id, sink, "approval cancel_turn"));
            }
            CallOutcome::FailTurn(message) => {
                let error = PublicError {
                    code: "callback_delivery_failed".to_owned(),
                    message,
                    details: None,
                };
                return Err(fail_turn(core, turn_id, sink, error));
            }
        };
        answers.insert(call_id, model_content(&result));
        let Started::Tool(action) = started else {
            continue;
        };
        let mut core = lock(core);
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
    }
}

async fn interrupted(interrupt: &mut watch::Receiver<Option<String>>) -> String {
    loop {
        if let Some(reason) = interrupt.borrow().as_ref() {
            return reason.clone();
        }
        if interrupt.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Reference `_cancel_active_work` then `interrupt`: the open callbacks are
/// cancelled, then the turn is interrupted for `reason`.
fn interrupt_turn(
    core: &Arc<Mutex<SessionCore>>,
    turn_id: &str,
    sink: &Sink,
    reason: &str,
) -> TurnEnd {
    let mut core = lock(core);
    close_callbacks(&mut core, sink, "Turn interrupted");
    core.keep_pending_steering();
    let input = core.next_input();
    let advance = core.projector.apply(
        input,
        &[Observation::TurnInterrupted {
            turn_id: turn_id.to_owned(),
            reason: Some(reason.to_owned()),
        }],
        Some(&TransitionTurn::Settled),
        now_millis(),
    );
    sink(core.publication(advance, None));
    TurnEnd::Interrupted
}

/// Cancels every open callback, publishing each one settled.
pub(super) fn close_callbacks(core: &mut SessionCore, sink: &Sink, reason: &str) {
    for entry in core.callbacks.close_all(reason, now_millis()) {
        sink(core.publication(Advance::Provisional, Some(CallbackEvent::Resolved(entry))));
    }
}

fn fail_turn(
    core: &Arc<Mutex<SessionCore>>,
    turn_id: &str,
    sink: &Sink,
    error: PublicError,
) -> TurnEnd {
    let mut core = lock(core);
    close_callbacks(&mut core, sink, "Turn failed");
    core.keep_pending_steering();
    let input = core.next_input();
    let advance = core.projector.apply(
        input,
        &[Observation::TurnFailed {
            turn_id: turn_id.to_owned(),
            error: error.clone(),
        }],
        Some(&TransitionTurn::Failed(error.message.clone())),
        now_millis(),
    );
    sink(core.publication(advance, None));
    TurnEnd::Failed(error)
}

/// Whether the session's model reads images (reference
/// `LocalModelRoute.supports_images`).
fn native_images(environment: &TurnEnvironment) -> bool {
    environment
        .model
        .as_ref()
        .is_none_or(|model| model.supports_images)
}

/// Reference `_validate_image_support`: the refusal a call to a model that
/// reads no image makes while the conversation still holds one.
fn unsupported_image(core: &SessionCore, environment: &TurnEnvironment) -> Option<PublicError> {
    if native_images(environment)
        || !core
            .conversation
            .iter()
            .any(|message| message.holds_image(false))
    {
        return None;
    }
    let model = environment
        .model
        .as_ref()
        .map(|model| model.name.clone())
        .unwrap_or_default();
    Some(PublicError {
        message: format!(
            "Model {model} does not support images, but the completion input still contains an image"
        ),
        code: "images_not_supported".to_owned(),
        details: Some(json!({"model": model})),
    })
}

/// The request one model call makes.
pub(super) fn provider_input(
    core: &SessionCore,
    environment: &TurnEnvironment,
    first_call: bool,
    message_id: &str,
) -> ProviderInput {
    let mut messages = vec![ModelMessage::System {
        content: environment.system_prompt.clone(),
    }];
    messages.extend(
        core.conversation
            .iter()
            .map(|message| message.to_model(native_images(environment))),
    );
    let mut metadata = environment.metadata.clone();
    metadata.insert(
        "call_type".to_owned(),
        if first_call {
            "main_call"
        } else {
            "secondary_call"
        }
        .to_owned(),
    );
    metadata.insert(
        "message_id".to_owned(),
        environment
            .message_id
            .clone()
            .unwrap_or_else(|| message_id.to_owned()),
    );
    ProviderInput {
        turn_id: None,
        session_id: Some(core.projector.state.session.id.clone()),
        model_override: environment.model.as_ref().map(|model| model.alias.clone()),
        model: environment.model.clone(),
        messages,
        stream: true,
        images: Vec::new(),
        tools: environment
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.to_owned(),
                description: tool.description.clone(),
                input_schema: tool.parameters.clone(),
            })
            .collect(),
        tool_choice: None,
        thinking: false,
        reasoning_effort: None,
        headers: BTreeMap::new(),
        limits: Default::default(),
        metadata,
        dialect: RequestDialect::Unified,
    }
}
