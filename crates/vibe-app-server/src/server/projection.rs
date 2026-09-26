//! Projections of server-held session state into the public wire shapes.

use super::*;
use crate::workspace::{history_entry_id, reference_message_index};
use vibe_core::compaction::context::is_compaction_context_message;
use vibe_core::events::EffectApproval;

/// The session's status as the wire union publishes it.
///
/// This is what a `session/updated` patch replaces `/status` with, so it lives
/// beside the full state rather than inside it: a client reconstructs the same
/// value from either notification.
pub(super) fn public_session_status(session: &SessionRuntime) -> Value {
    match session.status {
        SessionStatus::Idle | SessionStatus::Cancelled => json!({"type": "idle"}),
        SessionStatus::Running => json!({
            "type": "running",
            "activeTurnId": session.active_turn,
        }),
        SessionStatus::WaitingCallback => json!({
            "type": "blocked",
            "activeTurnId": session.active_turn,
            "callbackId": session.pending_callback.as_ref().map(|callback| &callback.id),
            // The reference names the callback kind here, which is what lets a
            // client explain the block without reading the callback entry.
            "reason": session
                .pending_callback
                .as_ref()
                .map_or("approval", |callback| match callback.kind {
                    EngineCallbackKind::Approval => "approval",
                    EngineCallbackKind::UserInput => "user_input",
                    EngineCallbackKind::ConnectorAuth => "connector_auth",
                }),
        }),
        SessionStatus::Failed => json!({
            "type": "failed",
            "message": session
                .latest_turn
                .as_ref()
                .and_then(|turn| turn.error.as_ref())
                .map_or("Turn failed", |error| error.message.as_str()),
        }),
        SessionStatus::Closed => json!({"type": "archived"}),
    }
}

pub(super) fn public_session_state(session: &SessionRuntime) -> Value {
    let history = session
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.history.clone())
        .unwrap_or_default();
    let status = public_session_status(session);
    // Reference `message_preview`: the first user message the operator typed,
    // cut at 160 characters. Context the harness injected during a turn never
    // names a session; a replayed message, which carries no turn, does.
    // A cleared conversation starts over, so nothing said before the clear
    // previews it: the reference reads the preview from the loop's messages,
    // which the clear emptied.
    let since_clear = history
        .iter()
        .rposition(
            |entry| matches!(entry, PublicHistoryEntry::Checkpoint { kind, .. } if kind == "clear"),
        )
        .map_or(0, |index| index + 1);
    let preview = history
        .get(since_clear..)
        .unwrap_or_default()
        .iter()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Message {
                metadata,
                role: PublicMessageRole::User,
                content,
                source,
                ..
            } if *source != Some(PublicMessageSource::Harness) || metadata.turn_id.is_none() => {
                Some(content_text(content)).filter(|text| !text.is_empty())
            }
            _ => None,
        })
        .or_else(|| {
            session
                .persisted
                .as_ref()?
                .messages
                .iter()
                .find_map(|message| match message {
                    vibe_core::events::ModelMessage::User {
                        content,
                        injected: false,
                        ..
                    } if !content.is_empty() => Some(content.clone()),
                    _ => None,
                })
        })
        .map(|text| text.chars().take(160).collect::<String>())
        .unwrap_or_default();
    let parent_session_id = session
        .persisted
        .as_ref()
        .and_then(|persisted| persisted.metadata.parent_session_id.as_deref());
    // Reference `build_public_state`: the latest `history_limit` entries, and
    // the identifier of the oldest one kept when older ones were left out.
    let retained_from = history.len().saturating_sub(PUBLIC_HISTORY_LIMIT);
    let history_before_cursor = (retained_from > 0)
        .then(|| {
            history
                .get(retained_from)
                .map(|entry| entry.metadata().id.clone())
        })
        .flatten();
    let turns = session
        .turns
        .get(session.turns.len().saturating_sub(PUBLIC_HISTORY_LIMIT)..)
        .unwrap_or_default();
    // Reference `harness_files.workspace_roots`: the working directory first,
    // then every other directory the session was given, each once.
    let mut workspace_roots = vec![&session.working_directory];
    for root in &session.intent.add_directories {
        if !workspace_roots.contains(&root) {
            workspace_roots.push(root);
        }
    }
    json!({
        "format": "vibe.public-session-state/v1",
        "eventId": session.event_watermark,
        "session": {
            "id": session.id,
            // Reference `root_session_id`: the session this one continues, or
            // itself.
            "rootSessionId": parent_session_id.unwrap_or(&session.id),
            "parentSessionId": parent_session_id,
            "title": session.snapshot.as_ref().and_then(|snapshot| snapshot.title.as_ref()),
            "preview": preview,
            "status": status,
            "createdAt": session.created_at,
            "updatedAt": session.updated_at,
            "bumpedAt": session.bumped_at,
            // `session/pin` is not served, so no session is ever pinned.
            "pinnedAt": null,
            "cwd": session.working_directory,
            "workspaceRoots": workspace_roots,
            "model": session.intent.model.as_ref().or(session.active_model_alias.as_ref()),
            // Reference `build_public_state` never sets it on the legacy
            // harness.
            "reasoningEffort": null,
            "agent": session.agent_summary,
            "tokenUsage": {
                "inputTokens": session.stats.session_prompt_tokens,
                "outputTokens": session.stats.session_completion_tokens,
                "totalTokens": session
                    .stats
                    .session_prompt_tokens
                    .saturating_add(session.stats.session_completion_tokens),
            },
            "contextUsage": null,
            // The legacy harness answers every session here (row 36).
            "harness": null,
        },
        // No background work runs beside a turn in this port, so a session is
        // always quiescent.
        "isQuiescent": true,
        "history": history.get(retained_from..).unwrap_or_default(),
        "historyBeforeCursor": history_before_cursor,
        "turns": turns,
        "activeCallbacks": session
            .pending_callback
            .iter()
            .map(|callback| callback.entry.clone())
            .collect::<Vec<_>>(),
        "childSessions": [],
        "turnQueue": {"items": [], "paused": false, "maxItems": TURN_QUEUE_MAX_ITEMS},
        "retrying": null,
    })
}

/// Reference `rebind_history_with_checkpoint`
/// (`vibe/app_server/_root_session.py`): the entry that closes a history a
/// resume, a rewind or a clearing replaced, under a fresh identity.
pub(super) fn checkpoint_entry(
    session_id: &str,
    kind: &str,
    message: &str,
    details: Value,
) -> PublicHistoryEntry {
    let timestamp = now_millis();
    PublicHistoryEntry::Checkpoint {
        metadata: vibe_core::events::PublicEntryMetadata {
            id: format!("checkpoint:{kind}:{}", vibe_core::session_id::uuid_v4()),
            session_id: session_id.to_owned(),
            turn_id: None,
            created_at: timestamp,
            updated_at: timestamp,
            generation_status: vibe_core::events::PublicEntryGenerationStatus::Completed,
            related_entry_id: None,
        },
        kind: kind.to_owned(),
        message: Some(message.to_owned()),
        details,
    }
}

/// Reference `SessionOpenParams.history_limit` and `PageRequest.limit`, the
/// default window a public state carries.
pub(super) const PUBLIC_HISTORY_LIMIT: usize = 200;

/// Reference `TURN_QUEUE_MAX_ITEMS`, the capacity every queue announces.
const TURN_QUEUE_MAX_ITEMS: u32 = 32;

/// The identity an effect entry carries: the call it projects, as the
/// reference names it, unless the call carries none.
fn effect_id(call_id: &String) -> Option<&String> {
    (!call_id.is_empty()).then_some(call_id)
}

pub(super) fn persisted_projection(
    hydrated: &HydratedSession,
    history_limit: u16,
    working_directory: &str,
) -> ProjectionSnapshot {
    // A file path a resumed header names is displayed against where the
    // session sits now, which is the directory a live turn stamps as well.
    let working_directory = Some(Path::new(working_directory));
    let session_id = &hydrated.metadata.id;
    // Reference `project_message_history` stamps a replayed entry with the
    // moment it was read back plus its position, so the replay orders before
    // anything the reopened session goes on to add.
    let base_timestamp = crate::host::now_millis();
    // The call's name and arguments are what the effect detail is rebuilt from,
    // so a resumed transcript renders through the same typed path a live turn
    // publishes rather than through a generic fallback.
    let mut tool_calls_by_id = BTreeMap::<String, (String, String, usize)>::new();
    let mut history = Vec::new();
    // A message stored with the identity its live entry had keeps it, which
    // is what lets a client that saw the turn live address it after a reload
    // (reference `project_message_history` reads `message_id`).
    // One without falls back to its position and role, counted the way the
    // reference counts them (`history_message_id`), which is also the
    // identity a rewind resolves.
    let metadata = |index: usize, suffix: &str, id: Option<&String>| PublicEntryMetadata {
        id: id.cloned().unwrap_or_else(|| {
            history_entry_id(reference_message_index(&hydrated.messages, index), suffix)
        }),
        session_id: session_id.clone(),
        turn_id: None,
        created_at: base_timestamp.saturating_add(u64::try_from(index).unwrap_or(u64::MAX)),
        updated_at: base_timestamp.saturating_add(u64::try_from(index).unwrap_or(u64::MAX)),
        generation_status: PublicEntryGenerationStatus::Completed,
        related_entry_id: None,
    };
    for (index, message) in hydrated.messages.iter().enumerate() {
        match message {
            ModelMessage::System { .. } => {}
            // Reference `_append_compaction_history`: the envelope a compaction
            // appended reads as the checkpoint that marks it.
            message if is_compaction_context_message(message) => {
                let message_id = match message {
                    ModelMessage::User {
                        message_id: Some(message_id),
                        ..
                    } => message_id.clone(),
                    _ => history_entry_id(
                        reference_message_index(&hydrated.messages, index),
                        "compaction",
                    ),
                };
                history.push(PublicHistoryEntry::Checkpoint {
                    metadata: metadata(
                        index,
                        "compaction",
                        Some(&format!("checkpoint:compaction:{message_id}")),
                    ),
                    kind: "compaction".to_owned(),
                    message: Some("Context compacted".to_owned()),
                    details: json!({}),
                });
            }
            // Any other turn the harness wrote stays out of the history.
            ModelMessage::User { injected: true, .. } => {}
            ModelMessage::User {
                content,
                message_id,
                attachments,
                ..
            } => history.push(PublicHistoryEntry::Message {
                metadata: metadata(index, "user", message_id.as_ref()),
                role: PublicMessageRole::User,
                content: std::iter::once(PublicContentBlock::Text {
                    text: content.clone(),
                })
                .chain(attachments.iter().cloned())
                .collect(),
                // Reference `_history_user_message`: a replayed message is the
                // harness's, whoever typed it first.
                source: Some(PublicMessageSource::Harness),
                user_display_content: None,
            }),
            ModelMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                message_id,
                reasoning_message_id,
                ..
            } => {
                if let Some(reasoning) = reasoning.as_ref().filter(|value| !value.is_empty()) {
                    history.push(PublicHistoryEntry::Reasoning {
                        metadata: metadata(index, "reasoning", reasoning_message_id.as_ref()),
                        text: reasoning.clone(),
                        summary: Vec::new(),
                    });
                }
                if !content.is_empty() {
                    history.push(PublicHistoryEntry::Message {
                        metadata: metadata(index, "assistant", message_id.as_ref()),
                        role: PublicMessageRole::Assistant,
                        content: vec![PublicContentBlock::Text {
                            text: content.clone(),
                        }],
                        source: Some(PublicMessageSource::Harness),
                        user_display_content: None,
                    });
                }
                for tool_call in tool_calls {
                    tool_calls_by_id.insert(
                        tool_call.id.clone(),
                        (tool_call.name.clone(), tool_call.arguments.clone(), index),
                    );
                }
            }
            ModelMessage::Tool {
                call_id,
                content,
                is_error,
            } => {
                let (title, arguments, call_index) = tool_calls_by_id
                    .remove(call_id)
                    .unwrap_or_else(|| ("Tool".to_owned(), String::new(), index));
                let detail =
                    EffectDetail::for_encoded_call_at(&title, &arguments, working_directory);
                let state = if *is_error {
                    PublicEffectState::Failed {
                        error: PublicError {
                            message: content.clone(),
                            code: Some("persisted_tool_error".to_owned()),
                            details: Value::Null,
                        },
                        output: Value::Null,
                        output_text: content.clone(),
                        duration_ms: 0,
                        display: EffectResultDisplay::failed(&detail.display),
                        approval: EffectApproval::default(),
                    }
                } else {
                    let output = json!(content);
                    PublicEffectState::Completed {
                        display: EffectResultDisplay::completed_at(
                            detail.kind,
                            &detail.display,
                            &output,
                            &Value::Null,
                            working_directory,
                        ),
                        output,
                        output_text: content.clone(),
                        duration_ms: 0,
                        approval: EffectApproval::default(),
                    }
                };
                history.push(PublicHistoryEntry::Effect {
                    metadata: metadata(call_index, "effect", effect_id(call_id)),
                    title,
                    detail: Box::new(detail),
                    state,
                    tool_call_id: call_id.clone(),
                });
            }
        }
    }
    for (call_id, (title, arguments, index)) in tool_calls_by_id {
        history.push(PublicHistoryEntry::Effect {
            metadata: metadata(index, "effect", effect_id(&call_id)),
            detail: Box::new(EffectDetail::for_encoded_call_at(
                &title,
                &arguments,
                working_directory,
            )),
            state: PublicEffectState::Skipped {
                reason: "Persisted tool call has no recorded result".to_owned(),
                display: EffectResultDisplay::skipped(&title),
                approval: EffectApproval::default(),
            },
            title,
            tool_call_id: call_id,
        });
    }
    history.sort_by_key(|entry| entry.metadata().created_at);
    let retained_from = history.len().saturating_sub(usize::from(history_limit));
    history.drain(..retained_from);
    ProjectionSnapshot {
        session_id: session_id.clone(),
        turn_id: None,
        handoff_cause: None,
        watermark: 0,
        lifecycle: LifecycleState::Idle,
        title: hydrated.metadata.title.clone(),
        history,
    }
}
