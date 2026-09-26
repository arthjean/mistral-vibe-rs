//! Callback requests the server raises toward a client, and their refusals.
//!
//! A callback leaves the engine, becomes an entry in the session's projection,
//! and parks the turn until an answer settles it. This composes the entry and
//! the request; `callbacks` validates what comes back.

use super::*;

impl AppServer {
    /// The effect an approval raised from a bare prompt presents.
    ///
    /// There is no tool behind it, so it publishes the generic kind and carries
    /// the prompt in the display's free-form content.
    fn approval_prompt_effect(prompt: &str) -> EffectDetail {
        let mut effect = EffectDetail::for_call("callback", &json!({}));
        effect.display.content = Some(prompt.to_owned());
        effect
    }

    pub fn request_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        prompt: impl Into<String>,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        if kind == EngineCallbackKind::ConnectorAuth {
            return Err(ServerError::UnsupportedCallbackKind(kind));
        }
        let prompt = prompt.into();
        let detail = match kind {
            EngineCallbackKind::Approval => json!({
                "kind": "approval",
                "effect": Self::approval_prompt_effect(&prompt),
                "requiredPermissions": [],
                "choices": [
                    "approve",
                    "approve_for_session",
                    "approve_permanently",
                    "deny",
                    "cancel_turn",
                ],
                "relatedEntryId": null,
            }),
            EngineCallbackKind::UserInput => json!({
                "kind": "user_input",
                "request": {
                    "questions": [{
                        "question": prompt,
                        "header": "",
                        "options": [
                            {"label": "Yes", "description": ""},
                            {"label": "No", "description": ""},
                        ],
                        "multiSelect": false,
                        "hideOther": false,
                    }],
                    "footerNote": null,
                },
                "relatedEntryId": null,
            }),
            EngineCallbackKind::ConnectorAuth => {
                return Err(ServerError::UnsupportedCallbackKind(kind));
            }
        };
        self.request_callback_with_detail(session_id, turn_id, kind, prompt, detail)
    }

    pub fn request_callback_with_detail(
        &self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        title: impl Into<String>,
        detail: Value,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        if kind == EngineCallbackKind::ConnectorAuth {
            return Err(ServerError::UnsupportedCallbackKind(kind));
        }
        let title = title.into();
        let CallbackRequestDetail {
            detail,
            plan_review_path,
        } = parse_callback_request(kind, &title, &detail)
            .map_err(|message| ServerError::InvalidCallbackDetail(message.to_owned()))?;
        let related_entry_id = detail.related_entry_id().map(str::to_owned);
        let related = related_entry_id.clone();
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        if session.pending_callback.is_some() {
            return Err(ServerError::CallbackConflict);
        }
        let callback_sequence = self.next_callback.fetch_add(1, Ordering::Relaxed);
        let callback_id = vibe_core::session_id::uuid_v4();
        let timestamp = now_millis();
        let title_for_notice = title.clone();
        let callback = PublicHistoryEntry::Callback {
            metadata: PublicEntryMetadata {
                id: format!("callback:{callback_id}"),
                session_id: session.id.clone(),
                turn_id: Some(turn_id.to_owned()),
                created_at: timestamp,
                updated_at: timestamp,
                generation_status: PublicEntryGenerationStatus::InProgress,
                related_entry_id,
            },
            callback_id: callback_id.clone(),
            title,
            detail,
            state: vibe_core::events::PublicCallbackState::Open,
        };
        session.status = SessionStatus::WaitingCallback;
        session.updated_at = timestamp;
        session.pending_callback = Some(PendingCallback {
            id: callback_id.clone(),
            kind,
            entry: callback.clone(),
        });
        let snapshot = session.snapshot.get_or_insert_with(|| ProjectionSnapshot {
            session_id: session.id.clone(),
            turn_id: Some(turn_id.to_owned()),
            handoff_cause: None,
            watermark: 0,
            lifecycle: LifecycleState::Running,
            title: None,
            history: Vec::new(),
        });
        // The reference publishes a plan review as its own notice rather than as
        // a field on the callback, so the entry that names the plan lands ahead
        // of the question a client is about to be asked.
        if let Some(file_path) = plan_review_path {
            snapshot.history.push(PublicHistoryEntry::Notice {
                metadata: PublicEntryMetadata {
                    id: format!("notice:{callback_id}:plan-review"),
                    session_id: snapshot.session_id.clone(),
                    turn_id: Some(turn_id.to_owned()),
                    created_at: timestamp,
                    updated_at: timestamp,
                    generation_status: PublicEntryGenerationStatus::Completed,
                    related_entry_id: Some(format!("callback:{callback_id}")),
                },
                level: vibe_core::events::PublicNoticeLevel::Info,
                message: title_for_notice.clone(),
                detail: NoticeDetail::PlanReviewStarted { file_path },
            });
        }
        if !snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    callback_id: existing,
                    ..
                } if existing == &callback_id
            )
        }) {
            snapshot.history.push(callback.clone());
        }
        // Reference `_request_approval`: the callback is published, the
        // effect it gates shows as blocked on it, and the session as blocked,
        // before the question is delivered.
        let mut frames = vec![entry_added_frame(session, turn_id, &callback)];
        frames.extend(set_effect_state(
            session,
            turn_id,
            related.as_deref(),
            |output_text| PublicEffectState::Blocked {
                callback_id: callback_id.clone(),
                output_text,
            },
        ));
        frames.push(session_updated_frame(session));
        let request = encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: RequestId::Integer(i64::try_from(callback_sequence).unwrap_or(i64::MAX)),
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback))]),
        }));
        frames.push(request);
        Ok((callback_id, frames))
    }

    /// Reference `TurnController.reject_callback`: the callback the turn
    /// waits on is refused, and the turn ends as failed with `reason` once it
    /// has stopped. Nothing is published until then.
    pub(super) fn reject_callback(
        &self,
        route: &CallbackRoute,
        reason: &str,
    ) -> Result<(Vec<u8>, Vec<DeferredWork>), ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(&route.session_id)
            .ok_or_else(|| ServerError::SessionNotFound(route.session_id.clone()))?;
        if session.active_turn.as_deref() != Some(&route.turn_id) {
            return Err(ServerError::StaleTurn(route.turn_id.clone()));
        }
        let Some(callback) = session.pending_callback.take() else {
            return Ok((Vec::new(), Vec::new()));
        };
        if callback.id != route.callback_id {
            session.pending_callback = Some(callback);
            return Err(ServerError::CallbackConflict);
        }
        session.resolved_callbacks.insert(
            callback.id.clone(),
            ResolvedCallback {
                kind: callback.kind,
                output: rejection_output(reason),
            },
        );
        session.callback_rejection = Some(reason.to_owned());
        Ok((
            Vec::new(),
            vec![DeferredWork::ResolveCallback {
                session_id: route.session_id.clone(),
                turn_id: route.turn_id.clone(),
                callback_id: route.callback_id.clone(),
                accepted: false,
                value: Some(reason.to_owned()),
            }],
        ))
    }
}

/// A sequenced `history/entryAdded` for `entry`.
pub(super) fn entry_added_frame(
    session: &mut SessionRuntime,
    turn_id: &str,
    entry: &PublicHistoryEntry,
) -> Vec<u8> {
    let event_id = next_event_id(session);
    encode_notification(
        "history/entryAdded",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("turnId", json!(turn_id)),
            ("entry", json!(entry)),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// A sequenced `history/entryUpdated` replacing `fields` of an entry, with
/// the `/updatedAt` stamp last, as the reference projector writes it.
pub(super) fn entry_updated_frame(
    session: &mut SessionRuntime,
    turn_id: &str,
    entry_id: &str,
    fields: &[(&str, Value)],
    updated_at: u64,
) -> Vec<u8> {
    let event_id = next_event_id(session);
    let mut patch = fields
        .iter()
        .map(|(field, value)| json!({"op": "replace", "path": format!("/{field}"), "value": value}))
        .collect::<Vec<_>>();
    patch.push(json!({"op": "replace", "path": "/updatedAt", "value": updated_at}));
    encode_notification(
        "history/entryUpdated",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("turnId", json!(turn_id)),
            ("entryId", json!(entry_id)),
            ("patch", json!(patch)),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// Moves the effect a callback gates to the state `state` builds from its
/// output so far, and answers the frame that publishes it. An effect the
/// session does not hold is left alone.
pub(super) fn set_effect_state(
    session: &mut SessionRuntime,
    turn_id: &str,
    effect_id: Option<&str>,
    state: impl FnOnce(String) -> PublicEffectState,
) -> Option<Vec<u8>> {
    let effect_id = effect_id?;
    let timestamp = now_millis();
    let snapshot = session.snapshot.as_mut()?;
    let (metadata, current) = snapshot
        .history
        .iter_mut()
        .rev()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Effect {
                metadata, state, ..
            } if metadata.id == effect_id => Some((metadata, state)),
            _ => None,
        })?;
    let output_text = match current {
        PublicEffectState::Running { output_text }
        | PublicEffectState::Blocked { output_text, .. } => output_text.clone(),
        _ => String::new(),
    };
    *current = state(output_text);
    metadata.updated_at = timestamp;
    let value = json!(current);
    Some(entry_updated_frame(
        session,
        turn_id,
        effect_id,
        &[("state", value)],
        timestamp,
    ))
}

/// The frame that publishes a settled callback: its terminal state and its
/// completion, as reference `resolve_callback` projects them.
pub(super) fn settled_callback_frame(
    session: &mut SessionRuntime,
    turn_id: &str,
    callback_id: &str,
) -> Option<Vec<u8>> {
    let (entry_id, fields, updated_at) =
        session
            .snapshot
            .as_ref()?
            .history
            .iter()
            .rev()
            .find_map(|entry| match entry {
                PublicHistoryEntry::Callback {
                    metadata,
                    callback_id: existing,
                    state,
                    ..
                } if existing == callback_id => Some((
                    metadata.id.clone(),
                    [
                        ("state", json!(state)),
                        ("generationStatus", json!(metadata.generation_status)),
                    ],
                    metadata.updated_at,
                )),
                _ => None,
            })?;
    Some(entry_updated_frame(
        session, turn_id, &entry_id, &fields, updated_at,
    ))
}

/// The effect a callback gates, by the entry the callback names.
pub(super) fn callback_effect_id(session: &SessionRuntime, callback_id: &str) -> Option<String> {
    session
        .snapshot
        .as_ref()?
        .history
        .iter()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Callback {
                metadata,
                callback_id: existing,
                ..
            } if existing == callback_id => metadata.related_entry_id.clone(),
            _ => None,
        })
}

/// How a refusal is remembered among a session's settled callbacks, so a
/// repeated one reads as a duplicate.
pub(super) fn rejection_output(reason: &str) -> Value {
    json!({"error": {"message": reason}})
}

/// Reference `EventProjector.finalize`: every entry of the turn still in
/// progress is closed, an effect as interrupted or failed and a callback as
/// canceled, and the frames publishing that are answered. The reasons are
/// the ones the engine's own projection closes an effect with.
pub(super) fn finalize_turn_entries(
    session: &mut SessionRuntime,
    turn_id: &str,
    cancelled: bool,
) -> Vec<Vec<u8>> {
    let reason = if cancelled {
        "The turn was stopped before this call finished"
    } else {
        "The turn closed before this call finished"
    };
    let timestamp = now_millis();
    let mut closed = Vec::new();
    if let Some(snapshot) = session.snapshot.as_mut() {
        for entry in &mut snapshot.history {
            let metadata = entry.metadata();
            if metadata.turn_id.as_deref() != Some(turn_id)
                || metadata.generation_status == PublicEntryGenerationStatus::Completed
            {
                continue;
            }
            let mut fields = Vec::new();
            match entry {
                PublicHistoryEntry::Effect { state, .. } => {
                    let output_text = match state {
                        PublicEffectState::Running { output_text }
                        | PublicEffectState::Blocked { output_text, .. } => output_text.clone(),
                        _ => String::new(),
                    };
                    let display = EffectResultDisplay {
                        success: false,
                        verb: String::new(),
                        message: reason.to_owned(),
                        warnings: Vec::new(),
                        approval_note: None,
                        suffix: String::new(),
                    };
                    *state = if cancelled {
                        PublicEffectState::Cancelled {
                            reason: reason.to_owned(),
                            output_text,
                            duration_ms: 0,
                            display: Some(display),
                            approval: vibe_core::events::EffectApproval::default(),
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
                            approval: vibe_core::events::EffectApproval::default(),
                        }
                    };
                    fields.push(("state", json!(state)));
                }
                PublicHistoryEntry::Callback { state, .. } => {
                    *state = PublicCallbackState::Cancelled {
                        reason: reason.to_owned(),
                    };
                    fields.push(("state", json!(state)));
                }
                _ => {}
            }
            fields.push(("generationStatus", json!("completed")));
            let metadata = entry.metadata_mut();
            metadata.generation_status = PublicEntryGenerationStatus::Completed;
            metadata.updated_at = timestamp;
            closed.push((metadata.id.clone(), fields));
        }
    }
    closed
        .into_iter()
        .map(|(entry_id, fields)| {
            entry_updated_frame(session, turn_id, &entry_id, &fields, timestamp)
        })
        .collect()
}
