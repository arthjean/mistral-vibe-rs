//! Projections of server-held session state into the public wire shapes.

use super::*;

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
    let preview =
        history
            .iter()
            .rev()
            .find_map(|entry| match entry {
                PublicHistoryEntry::Message { content, .. } => Some(content_text(content)),
                _ => None,
            })
            .or_else(|| {
                session.persisted.as_ref()?.messages.iter().rev().find_map(
                    |message| match message {
                        vibe_core::events::ModelMessage::System { .. } => None,
                        vibe_core::events::ModelMessage::User { content }
                        | vibe_core::events::ModelMessage::Assistant { content, .. }
                        | vibe_core::events::ModelMessage::Tool { content, .. } => {
                            Some(content.clone())
                        }
                    },
                )
            })
            .unwrap_or_default();
    let parent_session_id = session
        .persisted
        .as_ref()
        .and_then(|persisted| persisted.metadata.parent_session_id.as_deref());
    json!({
        "format": "vibe.public-session-state/v1",
        "eventId": session.event_watermark,
        "session": {
            "id": session.id,
            "rootSessionId": session.aliases.first().unwrap_or(&session.id),
            "parentSessionId": parent_session_id,
            "title": session.snapshot.as_ref().and_then(|snapshot| snapshot.title.as_ref()),
            "preview": preview,
            "status": status,
            "createdAt": session.created_at,
            "updatedAt": session.updated_at,
            "cwd": session.working_directory,
            "workspaceRoots": session.intent.add_directories,
            "model": session.intent.model,
            "agent": session.agent_summary,
            "tokenUsage": {
                "inputTokens": session.stats.session_prompt_tokens,
                "outputTokens": session.stats.session_completion_tokens,
                "totalTokens": session
                    .stats
                    .session_prompt_tokens
                    .saturating_add(session.stats.session_completion_tokens),
            },
        },
        "history": {
            "entries": history,
            "cursor": {"before": null, "after": null},
            "range": "latest",
        },
        "activeCallbacks": session
            .pending_callback
            .iter()
            .map(|callback| callback.entry.clone())
            .collect::<Vec<_>>(),
        "latestTurn": session.latest_turn,
    })
}

pub(super) fn persisted_projection(
    hydrated: &HydratedSession,
    history_limit: u16,
) -> ProjectionSnapshot {
    let session_id = &hydrated.metadata.id;
    let base_timestamp = hydrated.metadata.created_at_ms;
    // The call's name and arguments are what the effect detail is rebuilt from,
    // so a resumed transcript renders through the same typed path a live turn
    // publishes rather than through a generic fallback.
    let mut tool_calls_by_id = BTreeMap::<String, (String, String, usize)>::new();
    let mut history = Vec::new();
    let metadata = |index: usize, suffix: &str| PublicEntryMetadata {
        id: format!("persisted:{index}:{suffix}"),
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
            ModelMessage::User { content } => history.push(PublicHistoryEntry::Message {
                metadata: metadata(index, "user"),
                role: PublicMessageRole::User,
                content: vec![PublicContentBlock::Text {
                    text: content.clone(),
                }],
                source: Some(PublicMessageSource::TurnStart),
                user_display_content: None,
            }),
            ModelMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                if let Some(reasoning) = reasoning.as_ref().filter(|value| !value.is_empty()) {
                    history.push(PublicHistoryEntry::Reasoning {
                        metadata: metadata(index, "reasoning"),
                        text: reasoning.clone(),
                        summary: Vec::new(),
                    });
                }
                if !content.is_empty() {
                    history.push(PublicHistoryEntry::Message {
                        metadata: metadata(index, "assistant"),
                        role: PublicMessageRole::Assistant,
                        content: vec![PublicContentBlock::Text {
                            text: content.clone(),
                        }],
                        source: None,
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
                let detail = EffectDetail::for_encoded_call(&title, &arguments);
                let state = if *is_error {
                    PublicEffectState::Failed {
                        error: PublicError {
                            message: content.clone(),
                            code: Some("persisted_tool_error".to_owned()),
                            details: Value::Null,
                        },
                        output_text: content.clone(),
                        duration_ms: 0,
                        display: EffectResultDisplay::failed(&detail.display),
                    }
                } else {
                    let output = json!(content);
                    PublicEffectState::Completed {
                        display: EffectResultDisplay::completed(
                            detail.kind,
                            &detail.display,
                            &output,
                            &Value::Null,
                        ),
                        output,
                        output_text: content.clone(),
                        duration_ms: 0,
                    }
                };
                history.push(PublicHistoryEntry::Effect {
                    metadata: metadata(call_index, "effect"),
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
            metadata: metadata(index, "effect"),
            detail: Box::new(EffectDetail::for_encoded_call(&title, &arguments)),
            state: PublicEffectState::Skipped {
                reason: "Persisted tool call has no recorded result".to_owned(),
                display: EffectResultDisplay::skipped(&title),
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
        watermark: 0,
        lifecycle: LifecycleState::Idle,
        title: hydrated.metadata.title.clone(),
        history,
    }
}
