//! The turn lifecycle a session moves through, as the server records it.
//!
//! A turn is reserved by a dispatch, run by a driver and settled here: what it
//! cost, what stopped it, what it left in the projection, and what the client
//! reads next. The scheduled-loop half is included, because a loop fire is a
//! turn the server started on the session's behalf rather than the client's.

use super::*;

impl AppServer {
    pub fn complete_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        self.complete_turn_with_stop_reason(session_id, turn_id, snapshot, None)
    }

    /// The frames a started turn puts on the wire, in emission order.
    ///
    /// `turn/started` first, then the `session/updated` that moves the session
    /// to `running`, matching the order the reference emits them in.
    pub fn turn_started(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let turn = session
            .latest_turn
            .as_ref()
            .filter(|turn| turn.id == turn_id)
            .ok_or_else(|| ServerError::StaleTurn(turn_id.to_owned()))?;
        let turn = turn.clone();
        session.stats.begin_turn();
        let event_id = next_event_id(session);
        let started = encode_notification(
            "turn/started",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session.id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        );
        Ok(vec![started, session_updated_frame(session)])
    }

    pub fn reserve_due_loop(
        &self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledLoopWork>, ServerError> {
        let (canonical_session_id, idle) = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            (
                session.id.clone(),
                session.active_turn.is_none()
                    && !session.compaction_pending
                    && session.status != SessionStatus::Closed,
            )
        };
        if !idle {
            return Ok(None);
        }
        let Some(loop_id) = self
            .release4
            .next_due_loop_id(&canonical_session_id, now_seconds)
            .map_err(|error| ServerError::Release4(error.to_string()))?
        else {
            return Ok(None);
        };
        let mut fire = self
            .release4
            .fire_loop_for_session(&loop_id, &canonical_session_id, now_seconds, true)
            .map_err(|error| ServerError::Release4(error.to_string()))?;
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(&canonical_session_id)
            .ok_or_else(|| ServerError::SessionNotFound(canonical_session_id.clone()))?;
        if session.active_turn.is_some()
            || session.compaction_pending
            || session.status == SessionStatus::Closed
        {
            self.release4
                .finish_loop_fire(&loop_id, now_seconds)
                .map_err(|error| ServerError::Release4(error.to_string()))?;
            return Ok(None);
        }
        let turn_sequence = self.next_turn.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!("turn-{turn_sequence}");
        if let Some(review) = &session.review {
            let message_index = review_message_index(&self.release3, session)?;
            review
                .begin_turn_at(&turn_id, message_index)
                .map_err(|error| ServerError::Resource(error.to_string()))?;
        }
        let started_at = now_millis();
        session.active_turn = Some(turn_id.clone());
        session.active_turn_started_at = Some(started_at);
        session.active_scheduled_loop = Some(loop_id.clone());
        session.status = SessionStatus::Running;
        session.latest_turn = Some(PublicTurn {
            id: turn_id.clone(),
            session_id: canonical_session_id.clone(),
            status: PublicTurnStatus::InProgress,
            started_at,
            completed_at: None,
            error: None,
            stop_reason: None,
        });
        session.updated_at = started_at;
        let event_id = next_event_id(session);
        fire.notice
            .params
            .insert("eventId".to_owned(), json!(event_id));
        fire.notice
            .params
            .insert("sessionId".to_owned(), json!(canonical_session_id.clone()));
        fire.notice
            .params
            .insert("turnId".to_owned(), json!(turn_id.clone()));
        fire.notice.params.insert(
            "emittedAt".to_owned(),
            json!(now_seconds.saturating_mul(1_000)),
        );
        if let Some(entry) = fire.notice.params.get_mut("entry") {
            entry["id"] = json!(format!("scheduled-loop:{turn_id}"));
            entry["sessionId"] = json!(canonical_session_id.clone());
            entry["turnId"] = json!(turn_id.clone());
        }
        let prompt = fire.prompt.clone();
        Ok(Some(ScheduledLoopWork {
            fire,
            work: DeferredWork::RunTurn {
                session_id: canonical_session_id,
                turn_id,
                prompt: prompt.clone(),
                input: vec![PublicContentBlock::Text { text: prompt }],
                client_user_message_id: None,
                auto_title: None,
                user_display_content: Some(json!({
                    "kind": "scheduled_loop",
                    "loopId": loop_id,
                    "firedAt": now_seconds,
                })),
                mention_stats: None,
            },
        }))
    }

    pub fn finish_scheduled_loop(
        &self,
        loop_id: &str,
        completed_at_seconds: u64,
    ) -> Result<(), ServerError> {
        match self
            .release4
            .finish_loop_fire(loop_id, completed_at_seconds)
        {
            Ok(()) | Err(Release4Error::Conflict(_)) => Ok(()),
            Err(error) => Err(ServerError::Release4(error.to_string())),
        }
    }

    pub fn complete_turn_with_stop_reason(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
        stop_reason: Option<vibe_core::events::PublicTurnStopReason>,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        self.complete_turn_with_details(session_id, turn_id, snapshot, stop_reason, None)
    }

    /// The frames a settled turn puts on the wire, in emission order.
    ///
    /// The reference publishes the session's new status before the turn's own
    /// terminal notification, so a client that renders status from
    /// `session/updated` never shows a running session after the turn is gone.
    pub fn complete_turn_with_details(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
        stop_reason: Option<vibe_core::events::PublicTurnStopReason>,
        error: Option<PublicError>,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let source_key = sessions
            .key(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
            .to_owned();
        let (was_closed, started_at, active_scheduled_loop, review) = {
            let session = sessions
                .get(&source_key)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            if session.active_turn.as_deref() != Some(turn_id) {
                return Err(ServerError::StaleTurn(turn_id.to_owned()));
            }
            (
                session.status == SessionStatus::Closed,
                session.active_turn_started_at.unwrap_or_default(),
                session.active_scheduled_loop.clone(),
                session.review.clone(),
            )
        };
        if !matches!(
            snapshot.lifecycle,
            LifecycleState::Completed | LifecycleState::Cancelled | LifecycleState::Failed
        ) {
            return Err(ServerError::NonTerminalCompletion(snapshot.lifecycle));
        }
        let target_session_id = snapshot.session_id.clone();
        if sessions
            .key(&target_session_id)
            .is_some_and(|existing| existing != source_key)
        {
            return Err(ServerError::SessionConflict(target_session_id));
        }
        let status = match snapshot.lifecycle {
            LifecycleState::Completed => PublicTurnStatus::Completed,
            LifecycleState::Cancelled => PublicTurnStatus::Interrupted,
            LifecycleState::Failed => PublicTurnStatus::Failed,
            _ => return Err(ServerError::NonTerminalCompletion(snapshot.lifecycle)),
        };
        let completed_at = now_millis();
        if let Some(loop_id) = &active_scheduled_loop {
            self.release4
                .finish_loop_fire(loop_id, completed_at / 1_000)
                .map_err(|error| ServerError::Release4(error.to_string()))?;
        }
        if let Some(review) = &review {
            review
                .seal_turn()
                .map_err(|error| ServerError::Resource(error.to_string()))?;
            self.publish_retention_notice(review);
        }
        let turn = PublicTurn {
            id: turn_id.to_owned(),
            session_id: target_session_id.clone(),
            status,
            started_at,
            completed_at: Some(completed_at),
            error: (status == PublicTurnStatus::Failed).then(|| {
                error.unwrap_or_else(|| {
                    public_turn_failure(TurnErrorCode::BackendError, "Turn failed")
                })
            }),
            stop_reason,
        };
        sessions.alias(&source_key, session_id);
        sessions.rename(&source_key, &target_session_id)?;
        let session = sessions
            .get_mut(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        cancel_pending_callback(session, "Turn completed before callback was answered");
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.active_scheduled_loop = None;
        session.status = if was_closed {
            SessionStatus::Closed
        } else {
            match snapshot.lifecycle {
                LifecycleState::Completed => SessionStatus::Idle,
                LifecycleState::Cancelled => SessionStatus::Cancelled,
                LifecycleState::Failed => SessionStatus::Failed,
                _ => SessionStatus::Idle,
            }
        };
        session.snapshot = Some(merge_server_callback_history(
            session.snapshot.as_ref(),
            snapshot.clone(),
        ));
        session.latest_turn = Some(turn.clone());
        session.updated_at = completed_at;
        session.stats.last_turn_duration_ms = completed_at.saturating_sub(started_at);
        let stats = stats_updated_frame(session);
        let status = session_updated_frame(session);
        let event_id = next_event_id(session);
        let completed = encode_notification(
            "turn/completed",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(target_session_id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        );
        Ok(vec![stats, status, completed])
    }

    pub fn fail_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let was_closed = session.status == SessionStatus::Closed;
        let started_at = session.active_turn_started_at.unwrap_or_default();
        if let Some(loop_id) = &session.active_scheduled_loop {
            self.release4
                .finish_loop_fire(loop_id, now_millis() / 1_000)
                .map_err(|error| ServerError::Release4(error.to_string()))?;
        }
        if let Some(review) = &session.review {
            review
                .seal_turn()
                .map_err(|error| ServerError::Resource(error.to_string()))?;
            self.publish_retention_notice(review);
        }
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.active_scheduled_loop = None;
        cancel_pending_callback(session, message);
        session.status = if was_closed {
            SessionStatus::Closed
        } else {
            SessionStatus::Failed
        };
        let turn = PublicTurn {
            id: turn_id.to_owned(),
            session_id: session_id.to_owned(),
            status: PublicTurnStatus::Failed,
            started_at,
            completed_at: Some(now_millis()),
            error: Some(public_turn_failure(code, message)),
            stop_reason: None,
        };
        session.latest_turn = Some(turn.clone());
        session.updated_at = turn.completed_at.unwrap_or(started_at);
        session.stats.last_turn_duration_ms = turn
            .completed_at
            .unwrap_or(started_at)
            .saturating_sub(started_at);
        let stats = stats_updated_frame(session);
        let status = session_updated_frame(session);
        let event_id = next_event_id(session);
        let completed = encode_notification(
            "turn/completed",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session_id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        );
        Ok(vec![stats, status, completed])
    }

    /// Records the usage one provider round trip reported and publishes it.
    ///
    /// The engine reports usage while the turn runs, which is what lets a client
    /// show context pressure before the turn settles rather than after.
    pub fn record_turn_stats(
        &self,
        session_id: &str,
        turn_id: &str,
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<Vec<u8>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        session
            .stats
            .observe(context_tokens, input_tokens, output_tokens);
        Ok(stats_updated_frame(session))
    }

    /// Publishes the handoff of a session running a turn, under the name its
    /// cause earns.
    pub(crate) fn handoff_active_turn(
        &self,
        old_session_id: &str,
        new_session_id: &str,
        turn_id: &str,
        mut snapshot: ProjectionSnapshot,
        notice: &HandoffNotice,
        emitted_at: u64,
    ) -> Result<Vec<u8>, ServerError> {
        if snapshot.session_id != new_session_id {
            return Err(ServerError::SessionConflict(new_session_id.to_owned()));
        }
        // A reference projection validates a handoff by the two identifiers
        // differing; one that rotated onto its own name is a state change it
        // rejects rather than applies.
        if old_session_id == new_session_id {
            return Err(ServerError::SessionConflict(new_session_id.to_owned()));
        }
        let mut sessions = self.lock_sessions()?;
        let source_key = sessions
            .key(old_session_id)
            .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?
            .to_owned();
        let previous_id = {
            let session = sessions
                .get(&source_key)
                .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?;
            if session.active_turn.as_deref() != Some(turn_id) {
                return Err(ServerError::StaleTurn(turn_id.to_owned()));
            }
            session.id.clone()
        };
        sessions.rename(&source_key, new_session_id)?;
        self.release4
            .rebind_session(&previous_id, new_session_id)
            .map_err(|error| ServerError::Release4(error.to_string()))?;
        for entry in &mut snapshot.history {
            entry.rebind_session(new_session_id);
        }
        sessions.alias(&source_key, old_session_id);
        let session = sessions
            .get_mut(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?;
        session.intent.resume = Some(new_session_id.to_owned());
        session.snapshot = Some(snapshot);
        session.updated_at = now_millis();
        session.event_watermark = 0;
        if let Some(turn) = session.latest_turn.as_mut() {
            turn.session_id = new_session_id.to_owned();
        }
        if let Some(callback) = session.pending_callback.as_mut() {
            callback.entry.rebind_session(new_session_id);
        }
        let event_id = next_event_id(session);
        let state = public_session_state(session);
        let (method, cause_field) = match notice {
            HandoffNotice::Compacted { summary_length } => (
                "session/compacted",
                ("summaryLength", json!(summary_length)),
            ),
            HandoffNotice::ContextCleared { plan_file_path } => (
                "session/contextCleared",
                ("planFilePath", json!(plan_file_path)),
            ),
        };
        Ok(encode_notification(
            method,
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(new_session_id)),
                ("oldSessionId", json!(old_session_id)),
                ("state", state),
                ("sessionLog", json!({"enabled": false})),
                cause_field,
                ("emittedAt", json!(emitted_at)),
            ]),
        ))
    }

    /// Turns the run a dispatch deferred into the reservation a driver executes.
    ///
    /// The working directory, the intent, the compaction settings and the tool
    /// registry are read under one lock rather than through separate reads:
    /// they describe a single generation of the session, and a close landing
    /// between two reads would otherwise hand the driver a reservation
    /// assembled from two of them.
    ///
    /// # Errors
    ///
    /// Reports work that reserved no turn, and a session that has since closed.
    pub fn reserve_turn(&self, work: DeferredWork) -> Result<TurnReservation, ServerError> {
        let DeferredWork::RunTurn {
            session_id,
            turn_id,
            prompt,
            input,
            client_user_message_id,
            auto_title,
            user_display_content,
            mention_stats,
        } = work
        else {
            return Err(ServerError::MissingTurnReservation);
        };
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.clone()))?;
        Ok(TurnReservation {
            working_directory: session.working_directory.clone(),
            intent: session.intent.clone(),
            compaction: session.compaction.clone(),
            tools: session.tools.clone(),
            session_id,
            turn_id,
            prompt,
            input,
            prepared_images: None,
            client_user_message_id,
            auto_title,
            user_display_content,
            mention_stats,
        })
    }
}

pub(super) fn scheduled_loop_turn(
    user_display_content: &Option<Value>,
) -> Result<Option<(String, u64)>, &'static str> {
    let Some(content) = user_display_content else {
        return Ok(None);
    };
    if content.get("kind").and_then(Value::as_str) != Some("scheduled_loop") {
        return Ok(None);
    }
    let loop_id = content
        .get("loopId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("scheduled loop turn requires loopId")?;
    let fired_at = content
        .get("firedAt")
        .and_then(Value::as_u64)
        .ok_or("scheduled loop turn requires integer firedAt")?;
    Ok(Some((loop_id.to_owned(), fired_at)))
}
