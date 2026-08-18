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
        let callback_id = format!("callback-{callback_sequence}");
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
        // The block is published before the callback is delivered, so a client
        // that renders status from `session/updated` is already showing the
        // session as blocked when the question arrives.
        let status = session_updated_frame(session);
        let request = encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: RequestId::Integer(i64::try_from(callback_sequence).unwrap_or(i64::MAX)),
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback))]),
        }));
        Ok((callback_id, vec![status, request]))
    }

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
        let Some(callback) = &session.pending_callback else {
            return Ok((Vec::new(), Vec::new()));
        };
        if callback.id != route.callback_id {
            return Err(ServerError::CallbackConflict);
        }
        settle_pending_callback(
            session,
            &route.callback_id,
            PublicCallbackState::Cancelled {
                reason: reason.to_owned(),
            },
        );
        session.pending_callback = None;
        session.status = SessionStatus::Cancelled;
        session.updated_at = now_millis();
        let status = session_updated_frame(session);
        Ok((
            status,
            vec![
                DeferredWork::ResolveCallback {
                    session_id: route.session_id.clone(),
                    turn_id: route.turn_id.clone(),
                    callback_id: route.callback_id.clone(),
                    accepted: false,
                    value: Some(reason.to_owned()),
                },
                DeferredWork::InterruptTurn {
                    session_id: route.session_id.clone(),
                    turn_id: route.turn_id.clone(),
                },
            ],
        ))
    }
}
