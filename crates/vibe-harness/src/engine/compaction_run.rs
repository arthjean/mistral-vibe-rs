//! A compaction: the conversation replaced by a summary the model writes of
//! it, between turns when a client asks or inside a turn whose context
//! outgrew the model.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_core::clock::now_millis;
use vibe_core::session_id::uuid_v4;

use super::conversation::ConversationMessage;
use super::provider_errors::turn_error;
use super::turn::{close_callbacks, provider_input};
use super::{SessionCore, Sink, TurnEnvironment};
use crate::state::{Observation, PublicError, TokenUsage, TransitionTurn};

/// What a compaction between turns ended with.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionEnd {
    /// The conversation was replaced; the summary that replaced it.
    Compacted { summary: String },
    /// The compaction failed and the conversation is unchanged.
    Failed { code: String, message: String },
}

/// Reference `start_compaction` then `finish_compaction` for a compaction a
/// client asked for between turns.
pub async fn compact_session(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    extra_instructions: &str,
) -> CompactionEnd {
    run_compaction(core, environment, sink, extra_instructions, None).await
}

/// One compaction: a manual one between turns, or an automatic one inside
/// `turn_id` once the context outgrew the model's threshold. A refused
/// summary is asked for again until the attempts run out.
pub(super) async fn run_compaction(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    extra_instructions: &str,
    turn_id: Option<&str>,
) -> CompactionEnd {
    let trigger = if turn_id.is_some() {
        "automatic"
    } else {
        "manual"
    };
    let compaction_id = uuid_v4();
    let mut attempt = 1;
    let mut retried = false;
    loop {
        let input = {
            let Ok(mut core) = core.lock() else {
                return CompactionEnd::Failed {
                    code: "internal_error".to_owned(),
                    message: "the session lock is poisoned".to_owned(),
                };
            };
            let now = now_millis();
            let input_entry_id = turn_id.and_then(|_| core.latest_input_entry_id());
            let entry = crate::state::running_compaction_entry(
                &core.projector.state.session.id.clone(),
                &core.projector.state.entries,
                &compaction_id,
                turn_id,
                trigger,
                attempt,
                now,
                input_entry_id.as_deref(),
            );
            if !retried {
                let advance = core.projector.upsert_provisional(entry, now);
                sink(core.publication(advance, None));
            }
            let message_id = core.latest_input_entry_id().unwrap_or_default();
            let mut input = provider_input(&core, environment, false, &message_id);
            input.stream = false;
            input.messages.push(
                ConversationMessage::User {
                    content: vec![json!({"type": "text", "text": crate::compaction::prompt(extra_instructions)})],
                    injected: true,
                }
                .to_model(true),
            );
            input
        };
        let answered = environment.provider.complete(&input).await;
        let Ok(mut core) = core.lock() else {
            return CompactionEnd::Failed {
                code: "internal_error".to_owned(),
                message: "the session lock is poisoned".to_owned(),
            };
        };
        let failed = |error: PublicError, reason: Option<String>| {
            (
                Observation::ContextCompactionFailed {
                    turn_id: turn_id.map(ToOwned::to_owned),
                    compaction_id: compaction_id.clone(),
                    trigger: trigger.to_owned(),
                    attempt,
                    code: error.code.clone(),
                    reason,
                },
                CompactionEnd::Failed {
                    code: error.code.clone(),
                    message: error.message.clone(),
                },
                Some(error),
            )
        };
        let (observation, end, error) = match answered {
            // Reference `complete` (the harness's adapters): an answer with
            // no text is a failed call, not a summary to refuse.
            Ok(message) if message.text.is_empty() => failed(
                compaction_error(
                    environment,
                    "Completion response has no text content",
                    "ValueError",
                ),
                None,
            ),
            Ok(message) => {
                match crate::compaction::extract_summary(
                    &message.text,
                    !message.tool_calls.is_empty(),
                ) {
                    Ok(summary) => {
                        let replaced = crate::compaction::replacement(&core.conversation, &summary);
                        core.conversation = replaced;
                        (
                            Observation::ContextCompacted {
                                turn_id: turn_id.map(ToOwned::to_owned),
                                compaction_id: compaction_id.clone(),
                                trigger: trigger.to_owned(),
                                attempt,
                                summary_len: summary.chars().count(),
                                usage: Some(TokenUsage {
                                    input_tokens: message.usage.input_tokens,
                                    output_tokens: message.usage.output_tokens,
                                    total_tokens: message.usage.input_tokens
                                        + message.usage.output_tokens,
                                    cached_input_tokens: message.usage.cached_tokens,
                                }),
                            },
                            CompactionEnd::Compacted { summary },
                            None,
                        )
                    }
                    Err(_) if attempt < crate::compaction::MAX_ATTEMPTS => {
                        // The retry shows only once the compaction settles:
                        // until then the checkpoint keeps its first attempt.
                        attempt += 1;
                        let advance = core.projector.touch(now_millis());
                        sink(core.publication(advance, None));
                        retried = true;
                        continue;
                    }
                    // Reference `_compaction_failure_reason`: the public
                    // reason a refused summary is recovered as.
                    Err(rejection) => failed(
                        PublicError {
                            code: "invalid_compaction_summary".to_owned(),
                            message: rejection.message().to_owned(),
                            details: None,
                        },
                        Some(
                            if rejection == crate::compaction::Rejection::ToolCalls {
                                "tool_call"
                            } else {
                                "empty_summary"
                            }
                            .to_owned(),
                        ),
                    ),
                }
            }
            Err(error) => {
                let mut error = turn_error(&error, environment);
                if let Some(details) = error.details.as_mut().and_then(Value::as_object_mut) {
                    details.insert("purpose".to_owned(), json!("compaction"));
                }
                failed(error, None)
            }
        };
        let input_id = core.next_input();
        let mut observations = vec![observation];
        let transition = match (turn_id, error) {
            (Some(turn_id), Some(error)) => {
                close_callbacks(&mut core, sink, "Turn failed");
                core.keep_pending_steering();
                let message = error.message.clone();
                observations.push(Observation::TurnFailed {
                    turn_id: turn_id.to_owned(),
                    error,
                });
                Some(TransitionTurn::Failed(message))
            }
            (Some(turn_id), None) => Some(TransitionTurn::Running(turn_id.to_owned())),
            (None, _) => None,
        };
        let advance =
            core.projector
                .apply(input_id, &observations, transition.as_ref(), now_millis());
        sink(core.publication(advance, None));
        return end;
    }
}

/// A compaction call's failure as the turn it ran in reports it.
fn compaction_error(environment: &TurnEnvironment, message: &str, exception: &str) -> PublicError {
    let model = environment.model.as_ref();
    PublicError {
        code: "model_stream_failed".to_owned(),
        message: message.to_owned(),
        details: Some(json!({
            "provider": model.map_or("mistral", |model| model.provider.as_str()),
            "model": model.map(|model| model.name.clone()).unwrap_or_default(),
            "purpose": "compaction",
            "exceptionType": exception,
        })),
    }
}

/// Reference automatic compaction: whether the context the last call
/// reported has reached the model's threshold.
pub(super) fn needs_compaction(core: &SessionCore, environment: &TurnEnvironment) -> bool {
    environment.compact_threshold > 0
        && core
            .projector
            .state
            .session
            .context_usage
            .is_some_and(|usage| usage.total_tokens >= environment.compact_threshold)
}
