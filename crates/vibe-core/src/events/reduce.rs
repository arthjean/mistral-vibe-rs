//! Folding one engine event into the public projection.
//!
//! The engine publishes what happened; a client renders a transcript. The two
//! are not the same shape: streamed text arrives as many events and is one
//! entry, a tool call and its result are one effect, and a compaction is a
//! checkpoint that is created and then patched. This is where that translation
//! lives, one arm per event.
//!
//! Every arm answers its refusals before it touches the state. That is not a
//! style choice: [`ProjectionReducer::apply`] reduces in place rather than into
//! a copy, so an arm that mutated and then failed would leave a rejected event's
//! debris behind. `a_rejected_event_leaves_the_projection_untouched` holds the
//! invariant.

use std::path::Path;

use serde_json::{Value, json};

use super::detail::{
    CallbackDetail, CallbackOutput, EffectDetail, EffectResultDisplay, NoticeDetail,
    RemoteSettlement, has_output_model, project_output, shell_transcript,
};
use super::*;

/// The checkpoint kind a compaction is published under, and the two labels its
/// entry carries: the reference creates the entry with the first and patches it
/// to the second. They are values a client renders, so they are reproduced
/// rather than reworded.
const COMPACTION_ENTRY_KIND: &str = "compaction";
const COMPACTION_STARTED_MESSAGE: &str = "Compacting context";
const COMPACTION_COMPLETED_MESSAGE: &str = "Context compacted";

/// What a running compaction publishes: how large the context was and the
/// threshold it crossed, which is what a client renders progress against.
///
/// The field names are the aliases the `CompactionDetails` model declares, and
/// only the fields the reference sends at this point are present: every field of
/// that model is optional, so a client reads the pair it was given rather than
/// five keys of which three are null. Reference `_project_compaction_started`.
fn compaction_progress_details(current_context_tokens: u64, threshold: u64) -> Value {
    json!({
        "currentContextTokens": current_context_tokens,
        "threshold": threshold,
    })
}

/// What a finished compaction publishes: how long the summary is and the two
/// identifiers the session moved between. Reference
/// `_project_compaction_completed`.
fn compaction_handoff_details(
    summary_length: u64,
    old_session_id: &str,
    new_session_id: &str,
) -> Value {
    json!({
        "summaryLength": summary_length,
        "oldSessionId": old_session_id,
        "newSessionId": new_session_id,
    })
}

/// Folds one event into the projection.
///
/// Every arm answers its refusals first: no arm may mutate `state` and then
/// fail. [`ProjectionReducer::apply`] reduces in place and relies on that, so
/// an arm that grows a check after a mutation silently makes a rejected event
/// leave debris behind.
pub(super) fn reduce_event(
    state: &mut ProjectionSnapshot,
    event_id: u64,
    emitted_at: u64,
    working_directory: Option<&Path>,
    event: &EngineEvent,
) -> Result<(), ProjectionError> {
    match event {
        EngineEvent::UserMessage {
            content,
            message_id,
            attachments,
        } => {
            require_lifecycle(
                state,
                &[LifecycleState::Idle, LifecycleState::Completed],
                "user_message",
            )?;
            state.lifecycle = LifecycleState::Running;
            let mut entry = user_message(
                state,
                event_id,
                emitted_at,
                content,
                PublicMessageSource::TurnStart,
            );
            claim_identity(state, &mut entry, message_id.as_deref());
            if let PublicHistoryEntry::Message { content, .. } = &mut entry {
                content.extend(attachments.iter().cloned());
            }
            state.history.push(entry);
        }
        EngineEvent::UserSteer { content } => {
            require_lifecycle(state, &[LifecycleState::Running], "user_steer")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(user_message(
                state,
                event_id,
                emitted_at,
                content,
                PublicMessageSource::TurnSteer,
            ));
        }
        EngineEvent::ContextInjected {
            content,
            as_message,
        } => {
            require_active(state, "context_injected")?;
            complete_streaming_entries(state, emitted_at);
            let entry = if *as_message {
                user_message(
                    state,
                    event_id,
                    emitted_at,
                    content,
                    PublicMessageSource::Harness,
                )
            } else {
                PublicHistoryEntry::Checkpoint {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::Completed,
                    ),
                    kind: "context_injected".to_owned(),
                    message: None,
                    details: json!({"content": content}),
                }
            };
            state.history.push(entry);
        }
        EngineEvent::ModelText { text, message_id } => {
            require_active(state, "model_text")?;
            let appended = match state.history.last_mut() {
                Some(PublicHistoryEntry::Message {
                    metadata,
                    role: PublicMessageRole::Assistant,
                    content,
                    ..
                }) if metadata.generation_status == PublicEntryGenerationStatus::InProgress => {
                    if let Some(PublicContentBlock::Text { text: current }) = content.first_mut() {
                        current.push_str(text);
                    }
                    metadata.updated_at = emitted_at;
                    true
                }
                _ => false,
            };
            if !appended {
                let mut entry = PublicHistoryEntry::Message {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::InProgress,
                    ),
                    role: PublicMessageRole::Assistant,
                    content: vec![PublicContentBlock::Text { text: text.clone() }],
                    source: None,
                    user_display_content: None,
                };
                claim_identity(state, &mut entry, message_id.as_deref());
                state.history.push(entry);
            }
        }
        EngineEvent::ModelReasoning { text, message_id } => {
            require_active(state, "model_reasoning")?;
            let appended = match state.history.last_mut() {
                Some(PublicHistoryEntry::Reasoning {
                    metadata,
                    text: current,
                    ..
                }) if metadata.generation_status == PublicEntryGenerationStatus::InProgress => {
                    current.push_str(text);
                    metadata.updated_at = emitted_at;
                    true
                }
                _ => false,
            };
            if !appended {
                let mut entry = PublicHistoryEntry::Reasoning {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::InProgress,
                    ),
                    text: text.clone(),
                    summary: Vec::new(),
                };
                claim_identity(state, &mut entry, message_id.as_deref());
                state.history.push(entry);
            }
        }
        EngineEvent::ToolCallAnnounced {
            call_id,
            name,
            remote,
        } => {
            require_active(state, "tool_call")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Effect {
                metadata: effect_metadata(state, call_id, event_id, emitted_at),
                title: name.clone(),
                detail: Box::new(EffectDetail::announced(name, remote.as_ref())),
                state: PublicEffectState::Running {
                    output_text: String::new(),
                },
                tool_call_id: call_id.clone(),
            });
        }
        EngineEvent::ToolCallUnresolved { call_id, name } => {
            require_active(state, "tool_call")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Effect {
                metadata: effect_metadata(state, call_id, event_id, emitted_at),
                title: name.clone(),
                detail: Box::new(EffectDetail::unresolved(name)),
                state: PublicEffectState::Running {
                    output_text: String::new(),
                },
                tool_call_id: call_id.clone(),
            });
        }
        EngineEvent::ToolCall {
            call_id,
            name,
            arguments,
            remote,
        } => {
            require_active(state, "tool_call")?;
            let resolved = match remote {
                Some(remote) => EffectDetail::for_proxied_call(name, arguments, remote),
                None => EffectDetail::for_encoded_call_at(name, arguments, working_directory),
            };
            // Reference `start_effect`: an announced call keeps its entry and
            // only its detail is replaced.
            if let Ok(PublicHistoryEntry::Effect {
                metadata, detail, ..
            }) = effect_entry(state, call_id, "tool_call")
            {
                **detail = resolved;
                metadata.updated_at = emitted_at;
                return Ok(());
            }
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Effect {
                metadata: effect_metadata(state, call_id, event_id, emitted_at),
                title: name.clone(),
                detail: Box::new(resolved),
                state: PublicEffectState::Running {
                    output_text: String::new(),
                },
                tool_call_id: call_id.clone(),
            });
        }
        EngineEvent::ToolStream { call_id, chunk } => {
            require_active(state, "tool_stream")?;
            let entry = effect_entry(state, call_id, "tool_stream_without_call")?;
            if let PublicHistoryEntry::Effect {
                metadata,
                state: PublicEffectState::Running { output_text },
                ..
            } = entry
            {
                output_text.push_str(chunk);
                metadata.updated_at = emitted_at;
            }
        }
        EngineEvent::ToolResult {
            call_id,
            content,
            typed_result,
            projected_result,
            display,
            duration_ms,
            is_error,
            cancelled,
            skipped,
            approval,
        } => {
            require_active(state, "tool_result")?;
            let approval = EffectApproval::from(*approval);
            let entry = effect_entry(state, call_id, "tool_result_without_call")?;
            if let PublicHistoryEntry::Effect {
                metadata,
                detail,
                state: current_state,
                ..
            } = entry
            {
                // Two documents, two readers. The header a client renders is
                // derived from what the tool answered, and the payload it
                // renders under that header is the projection the tool
                // published for the UI, which is the typed result again
                // whenever no projection was published.
                let answered = if typed_result.is_null() {
                    json!(content)
                } else {
                    typed_result.clone()
                };
                // Reference `project_effect_output`: the projection the tool
                // published for the UI, else its typed result, read through
                // the output model of the effect's kind.
                let output = if has_output_model(detail.kind) && detail.remote.is_none() {
                    let result = if projected_result.is_null() {
                        typed_result
                    } else {
                        projected_result
                    };
                    project_output(detail.kind, result)
                } else if projected_result.is_null() {
                    answered.clone()
                } else {
                    projected_result.clone()
                };
                // Reference `_effect_output_text`: only what the call streamed
                // while it ran is carried over; a shell that streamed nothing
                // publishes its transcript instead.
                let streamed = match &*current_state {
                    PublicEffectState::Running { output_text }
                    | PublicEffectState::Blocked { output_text, .. } => output_text.clone(),
                    _ => String::new(),
                };
                // Reference `project_effect_state`: a declined call settles as
                // cancelled when the decline ended the turn and as skipped
                // otherwise, its header the reason with its tags removed.
                let declined = || EffectResultDisplay {
                    success: false,
                    message: untagged(content),
                    ..EffectResultDisplay::default()
                };
                *current_state = if *skipped && *cancelled {
                    PublicEffectState::Cancelled {
                        reason: "Cancelled".to_owned(),
                        output_text: streamed,
                        duration_ms: *duration_ms,
                        display: Some(declined()),
                        approval,
                    }
                } else if *skipped {
                    PublicEffectState::Skipped {
                        reason: content.clone(),
                        display: declined(),
                        approval,
                    }
                } else if *cancelled {
                    PublicEffectState::Cancelled {
                        reason: content.clone(),
                        output_text: streamed,
                        duration_ms: *duration_ms,
                        display: EffectResultDisplay::cancelled(
                            &detail.tool_name,
                            typed_result,
                            display,
                        ),
                        approval,
                    }
                } else if *is_error {
                    // Reference `project_effect_state`: the error is the
                    // result's message with its tags removed, and nothing else.
                    PublicEffectState::Failed {
                        error: PublicError {
                            message: untagged(content),
                            code: None,
                            details: Value::Null,
                        },
                        output: Value::Null,
                        output_text: streamed,
                        duration_ms: *duration_ms,
                        display: match &detail.remote {
                            // The reference settles an errored call ahead of
                            // the tool's own branch, so a remote failure names
                            // what the server reported.
                            Some(remote) => EffectResultDisplay::for_remote(
                                remote,
                                &detail.display,
                                &RemoteSettlement::failed(content, &answered),
                            ),
                            // Reference `_result_display`: an error is shown
                            // as its own message.
                            None => failure_display(content),
                        },
                        approval,
                    }
                } else {
                    let output_text = if streamed.is_empty() && detail.kind == ToolEffectKind::Shell
                    {
                        shell_transcript(&output)
                    } else {
                        streamed
                    };
                    PublicEffectState::Completed {
                        output_text,
                        duration_ms: *duration_ms,
                        display: match &detail.remote {
                            Some(remote) => EffectResultDisplay::for_remote(
                                remote,
                                &detail.display,
                                &RemoteSettlement::answered(&answered),
                            ),
                            None => EffectResultDisplay::completed_at(
                                detail.kind,
                                &detail.display,
                                &answered,
                                display,
                                working_directory,
                            ),
                        },
                        output,
                        approval,
                    }
                };
                // The child session is named by the delegation the tool answers
                // with, which is the earliest this projection learns it: the
                // engine raises no event when the child opens.
                if detail.kind == ToolEffectKind::Subagent {
                    detail.child_session_id = subagent_child_session(display);
                }
                metadata.updated_at = emitted_at;
                metadata.generation_status = PublicEntryGenerationStatus::Completed;
            }
        }
        EngineEvent::CallbackRequested {
            callback_id,
            kind,
            prompt,
        } => {
            require_lifecycle(state, &[LifecycleState::Running], "callback_requested")?;
            complete_streaming_entries(state, emitted_at);
            state.lifecycle = LifecycleState::WaitingCallback;
            state.history.push(PublicHistoryEntry::Callback {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::InProgress,
                ),
                callback_id: callback_id.clone(),
                title: prompt.clone(),
                detail: engine_callback_detail(*kind, prompt),
                state: PublicCallbackState::Open,
            });
        }
        EngineEvent::CallbackResolved {
            callback_id,
            accepted,
            value,
        } => {
            require_lifecycle(
                state,
                &[LifecycleState::WaitingCallback],
                "callback_resolved",
            )?;
            let callback = state
                .history
                .iter_mut()
                .rev()
                .find(|entry| {
                    matches!(entry, PublicHistoryEntry::Callback {
                        callback_id: id,
                        state: PublicCallbackState::Open,
                        ..
                    } if id == callback_id)
                })
                .ok_or_else(|| ProjectionError::CallbackNotPending(callback_id.clone()))?;
            if let PublicHistoryEntry::Callback {
                metadata,
                detail,
                state: callback_state,
                ..
            } = callback
            {
                *callback_state = if *accepted {
                    PublicCallbackState::Answered {
                        // The answer's type must match the callback that is
                        // open, which is what a reference client validates the
                        // answered state against.
                        output: accepted_callback_output(detail, value.as_deref()),
                    }
                } else {
                    PublicCallbackState::Cancelled {
                        reason: value
                            .clone()
                            .unwrap_or_else(|| "Callback rejected".to_owned()),
                    }
                };
                metadata.updated_at = emitted_at;
                metadata.generation_status = PublicEntryGenerationStatus::Completed;
            }
            state.lifecycle = if *accepted {
                LifecycleState::Running
            } else {
                LifecycleState::Cancelled
            };
        }
        EngineEvent::Hook { name, message } => {
            require_active(state, "hook")?;
            state.history.push(PublicHistoryEntry::Notice {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                level: PublicNoticeLevel::Info,
                message: message.clone(),
                detail: NoticeDetail::HookCompleted(HookNotice {
                    hook_name: Some(name.clone()),
                    content: Some(message.clone()),
                    ..HookNotice::default()
                }),
            });
        }
        EngineEvent::Compaction { summary } => {
            require_active(state, "compaction")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Checkpoint {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                kind: "compaction".to_owned(),
                message: Some(summary.clone()),
                details: Value::Null,
            });
        }
        EngineEvent::CompactionStarted {
            current_context_tokens,
            threshold,
            ..
        } => {
            require_active(state, "compaction_started")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Checkpoint {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::InProgress,
                ),
                kind: COMPACTION_ENTRY_KIND.to_owned(),
                message: Some(COMPACTION_STARTED_MESSAGE.to_owned()),
                details: compaction_progress_details(*current_context_tokens, *threshold),
            });
        }
        EngineEvent::CompactionCompleted {
            summary_length,
            old_session_id,
            new_session_id,
            ..
        } => {
            require_active(state, "compaction_completed")?;
            // The entry the start created is the one that is patched. A late
            // subscriber that never saw the start still gets a coherent entry,
            // which is why a missing one is created here rather than refused.
            let open = state.history.iter_mut().rev().find(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Checkpoint { metadata, kind, .. }
                        if kind == COMPACTION_ENTRY_KIND
                            && metadata.generation_status
                                == PublicEntryGenerationStatus::InProgress
                )
            });
            // The end replaces the details rather than merging them, which is
            // what the reference's patch does: the two progress numbers describe
            // a compaction that is still running and are stale once it is not.
            let handoff =
                compaction_handoff_details(*summary_length, old_session_id, new_session_id);
            match open {
                Some(PublicHistoryEntry::Checkpoint {
                    metadata,
                    message,
                    details,
                    ..
                }) => {
                    metadata.updated_at = emitted_at;
                    metadata.generation_status = PublicEntryGenerationStatus::Completed;
                    *message = Some(COMPACTION_COMPLETED_MESSAGE.to_owned());
                    *details = handoff;
                }
                _ => state.history.push(PublicHistoryEntry::Checkpoint {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::Completed,
                    ),
                    kind: COMPACTION_ENTRY_KIND.to_owned(),
                    message: Some(COMPACTION_COMPLETED_MESSAGE.to_owned()),
                    details: handoff,
                }),
            }
        }
        EngineEvent::Title { title } => {
            require_active(state, "title")?;
            state.title = Some(title.clone());
            state.history.push(PublicHistoryEntry::Notice {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                level: PublicNoticeLevel::Info,
                message: title.clone(),
                detail: NoticeDetail::SessionTitleUpdated {
                    title: title.clone(),
                },
            });
        }
        EngineEvent::SessionHandoff {
            from_session_id,
            to_session_id,
            // The cause names the notification the app-server publishes, and a
            // clearing also earns a transcript entry; the rebind below is the
            // same either way.
            cause,
        } => {
            require_active(state, "session_handoff")?;
            if from_session_id != &state.session_id {
                return Err(ProjectionError::InvalidHandoff {
                    expected: state.session_id.clone(),
                    actual: from_session_id.clone(),
                });
            }
            state.session_id.clone_from(to_session_id);
            state.handoff_cause = Some(cause.clone());
            for entry in &mut state.history {
                entry.metadata_mut().session_id.clone_from(to_session_id);
            }
            // A compaction is published as its own checkpoint; a clearing is
            // the handoff that leaves a notice behind, naming the plan whose
            // acceptance cleared the context when one did.
            if let SessionHandoffCause::ContextCleared { plan_file_path } = cause {
                state.history.push(PublicHistoryEntry::Notice {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::Completed,
                    ),
                    level: PublicNoticeLevel::Info,
                    message: "Session context cleared".to_owned(),
                    detail: NoticeDetail::ContextCleared {
                        plan_file_path: plan_file_path.clone(),
                    },
                });
            }
        }
        EngineEvent::Stats { .. }
        | EngineEvent::Retrying { .. }
        | EngineEvent::RequestSent { .. }
        | EngineEvent::CompactionOutcome { .. } => {}
        EngineEvent::Lifecycle {
            state: next,
            message,
        } => {
            validate_lifecycle_transition(state.lifecycle, *next)?;
            state.lifecycle = *next;
            if matches!(
                next,
                LifecycleState::Completed | LifecycleState::Failed | LifecycleState::Cancelled
            ) {
                settle_unfinished_effects(state, *next == LifecycleState::Cancelled, emitted_at);
            }
            complete_streaming_entries(state, emitted_at);
            // A failed turn used to append a notice whose `kind` the reference
            // does not declare, which a conforming client rejects. The failure
            // reaches a client through `turn/completed` and the session status,
            // both of which carry the message this notice repeated.
            let _ = message;
        }
    }
    Ok(())
}

/// The session a subagent ran in, as its delegation result names it.
///
/// The public identifier is the one a client may open; the internal one is the
/// fallback for a result that only carries it.
/// A completed user turn, however it reached the transcript.
///
/// The operator's own message, a steer landing mid-turn and a harness injection
/// published as a message are the same entry under three sources, so the shape
/// a client renders is declared once.
fn user_message(
    state: &ProjectionSnapshot,
    event_id: u64,
    emitted_at: u64,
    content: &str,
    source: PublicMessageSource,
) -> PublicHistoryEntry {
    PublicHistoryEntry::Message {
        metadata: entry_metadata(
            state,
            event_id,
            emitted_at,
            PublicEntryGenerationStatus::Completed,
        ),
        role: PublicMessageRole::User,
        content: vec![PublicContentBlock::Text {
            text: content.to_owned(),
        }],
        source: Some(source),
        user_display_content: None,
    }
}

/// Reference `_result_display` for a call that errored.
fn failure_display(error: &str) -> EffectResultDisplay {
    EffectResultDisplay {
        success: false,
        message: untagged(error),
        ..EffectResultDisplay::default()
    }
}

/// Reference `EventProjector.finalize`: an effect the turn ended under fails,
/// or is cancelled when the turn was, each with a reason of this port's own.
fn settle_unfinished_effects(state: &mut ProjectionSnapshot, cancelled: bool, emitted_at: u64) {
    let reason = if cancelled {
        "The turn was stopped before this call finished"
    } else {
        "The turn closed before this call finished"
    };
    for entry in &mut state.history {
        let PublicHistoryEntry::Effect {
            metadata,
            state: effect_state,
            ..
        } = entry
        else {
            continue;
        };
        if metadata.generation_status == PublicEntryGenerationStatus::Completed {
            continue;
        }
        let output_text = match &*effect_state {
            PublicEffectState::Running { output_text }
            | PublicEffectState::Blocked { output_text, .. } => output_text.clone(),
            _ => String::new(),
        };
        let display = EffectResultDisplay {
            success: false,
            message: reason.to_owned(),
            ..EffectResultDisplay::default()
        };
        *effect_state = if cancelled {
            PublicEffectState::Cancelled {
                reason: reason.to_owned(),
                output_text,
                duration_ms: 0,
                display: Some(display),
                approval: EffectApproval::default(),
            }
        } else {
            PublicEffectState::Failed {
                error: PublicError {
                    message: reason.to_owned(),
                    code: None,
                    details: Value::Null,
                },
                output: Value::Null,
                output_text,
                duration_ms: 0,
                display,
                approval: EffectApproval::default(),
            }
        };
        metadata.generation_status = PublicEntryGenerationStatus::Completed;
        metadata.updated_at = emitted_at;
    }
}

/// The effect entry answering `call_id`, or the refusal a stream or a result
/// arriving without its call raises.
///
/// The most recent match wins: a call id the model reused names the effect it
/// most recently opened, which is the one still running.
fn effect_entry<'a>(
    state: &'a mut ProjectionSnapshot,
    call_id: &str,
    event: &'static str,
) -> Result<&'a mut PublicHistoryEntry, ProjectionError> {
    let lifecycle = state.lifecycle;
    state
        .history
        .iter_mut()
        .rev()
        .find(|entry| {
            matches!(entry, PublicHistoryEntry::Effect { tool_call_id, .. }
                if tool_call_id == call_id)
        })
        .ok_or(ProjectionError::IllegalTransition {
            from: lifecycle,
            event,
        })
}

/// The child session a delegation opened, read off the display payload.
///
/// The typed result the `task` tool publishes carries the reference's three
/// `TaskResult` fields and nothing else, so the delegation effect and the two
/// session identifiers on it travel in the display payload instead. That is
/// the only place this projection can learn the child's name from.
fn subagent_child_session(display: &Value) -> Option<String> {
    let effect = display.get("effect")?;
    ["publicSessionId", "childSessionId"]
        .iter()
        .find_map(|key| effect.get(*key).and_then(Value::as_str))
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_owned)
}

/// The callback detail an engine-raised callback publishes.
///
/// The engine names a kind and a prompt; the wire union wants a typed body, so
/// an approval borrows the generic effect shape and a question becomes a
/// single-question request.
fn engine_callback_detail(kind: CallbackKind, prompt: &str) -> CallbackDetail {
    match kind {
        CallbackKind::UserInput => CallbackDetail::UserInput {
            request: UserQuestionRequest {
                questions: vec![UserQuestion {
                    question: prompt.to_owned(),
                    header: String::new(),
                    options: Vec::new(),
                    multi_select: false,
                    hide_other: false,
                }],
                footer_note: None,
            },
            related_entry_id: None,
        },
        // A connector-authorization callback has no reference variant, and the
        // app-server refuses to raise one. It is projected as the approval it
        // behaves like rather than as an invented kind.
        CallbackKind::Approval | CallbackKind::ConnectorAuth => CallbackDetail::Approval {
            effect: Box::new(EffectDetail::for_call("callback", &json!({}))),
            required_permissions: Vec::new(),
            choices: ApprovalDecisionType::ALL.to_vec(),
            related_entry_id: None,
            reason: None,
        },
    }
}

/// The answer an engine-resolved callback records, in the form its own kind
/// declares. A mismatched pair never reaches here: the app-server rejects an
/// output whose type is not the open callback's.
fn accepted_callback_output(detail: &CallbackDetail, value: Option<&str>) -> CallbackOutput {
    match detail {
        CallbackDetail::Approval { .. } => CallbackOutput::Approval {
            decision: ApprovalDecision {
                decision: ApprovalDecisionType::Approve,
            },
            feedback: value.map(str::to_owned),
        },
        CallbackDetail::UserInput { request, .. } => CallbackOutput::UserInput {
            result: UserQuestionResult {
                answers: request
                    .questions
                    .iter()
                    .map(|question| UserAnswer {
                        question: question.question.clone(),
                        answer: value.unwrap_or_default().to_owned(),
                        is_other: false,
                    })
                    .collect(),
                cancelled: false,
            },
        },
    }
}

fn entry_metadata(
    state: &ProjectionSnapshot,
    event_id: u64,
    emitted_at: u64,
    generation_status: PublicEntryGenerationStatus,
) -> PublicEntryMetadata {
    let id = state.turn_id.as_ref().map_or_else(
        || format!("entry-{event_id}"),
        |turn_id| format!("entry-{turn_id}-{event_id}"),
    );
    PublicEntryMetadata {
        id,
        session_id: state.session_id.clone(),
        turn_id: state.turn_id.clone(),
        created_at: emitted_at,
        updated_at: emitted_at,
        generation_status,
        related_entry_id: None,
    }
}

/// Gives `entry` the identity its message carries, unless an entry already
/// holds it: a model call whose text is interrupted by reasoning opens a
/// second entry, which keeps the positional identity instead.
fn claim_identity(state: &ProjectionSnapshot, entry: &mut PublicHistoryEntry, id: Option<&str>) {
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return;
    };
    if state
        .history
        .iter()
        .any(|existing| existing.metadata().id == id)
    {
        return;
    }
    entry.metadata_mut().id = id.to_owned();
}

/// Reference `TaggedText.from_string(...).message`: the text with every known
/// tag pair replaced by its content.
fn untagged(text: &str) -> String {
    const TAGS: [&str; 4] = [
        "user_cancellation",
        "tool_error",
        "vibe_stop_event",
        "vibe_warning",
    ];
    let mut result = text.to_owned();
    for tag in TAGS {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(start) = result.find(&open) {
            let Some(end) = result[start + open.len()..].find(&close) else {
                break;
            };
            let inner = result[start + open.len()..start + open.len() + end].to_owned();
            result.replace_range(start..start + open.len() + end + close.len(), &inner);
        }
    }
    result.trim().to_owned()
}

/// The metadata of an effect, which the reference keys on its tool call
/// (`EventProjector.start_effect`) rather than on the event that raised it.
fn effect_metadata(
    state: &ProjectionSnapshot,
    call_id: &str,
    event_id: u64,
    emitted_at: u64,
) -> PublicEntryMetadata {
    let mut metadata = entry_metadata(
        state,
        event_id,
        emitted_at,
        PublicEntryGenerationStatus::InProgress,
    );
    if !call_id.is_empty() {
        call_id.clone_into(&mut metadata.id);
    }
    metadata
}

/// Seals every entry still streaming.
///
/// Entries complete in order, so the scan stops at the first completed entry
/// walking backward instead of sweeping the whole history each time.
fn complete_streaming_entries(state: &mut ProjectionSnapshot, emitted_at: u64) {
    for entry in state.history.iter_mut().rev() {
        let metadata = entry.metadata_mut();
        if metadata.generation_status == PublicEntryGenerationStatus::Completed {
            break;
        }
        metadata.generation_status = PublicEntryGenerationStatus::Completed;
        metadata.updated_at = emitted_at;
    }
}

fn require_active(state: &ProjectionSnapshot, event: &'static str) -> Result<(), ProjectionError> {
    if state.lifecycle.is_active() {
        Ok(())
    } else {
        Err(ProjectionError::IllegalTransition {
            from: state.lifecycle,
            event,
        })
    }
}

fn require_lifecycle(
    state: &ProjectionSnapshot,
    allowed: &[LifecycleState],
    event: &'static str,
) -> Result<(), ProjectionError> {
    if allowed.contains(&state.lifecycle) {
        Ok(())
    } else {
        Err(ProjectionError::IllegalTransition {
            from: state.lifecycle,
            event,
        })
    }
}

fn validate_lifecycle_transition(
    from: LifecycleState,
    to: LifecycleState,
) -> Result<(), ProjectionError> {
    let valid = matches!(
        (from, to),
        (LifecycleState::Idle, LifecycleState::Running)
            | (LifecycleState::Running, LifecycleState::WaitingCallback)
            | (LifecycleState::WaitingCallback, LifecycleState::Running)
            | (
                LifecycleState::Running | LifecycleState::WaitingCallback,
                LifecycleState::Completed | LifecycleState::Failed | LifecycleState::Cancelled
            )
            | (LifecycleState::Completed, LifecycleState::Running)
    );
    if valid {
        Ok(())
    } else {
        Err(ProjectionError::IllegalTransition {
            from,
            event: "lifecycle",
        })
    }
}
