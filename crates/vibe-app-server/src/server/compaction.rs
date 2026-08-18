//! Manual compaction, and the identity handoff it performs.
//!
//! A compaction summarizes a transcript into a new session, so the session the
//! client is talking to has to move onto the new identifier without the client
//! losing its handle on it. The reservation, the rebinding and the release of
//! the reservation on failure are all here.

use super::*;

impl AppServer {
    pub fn complete_manual_compaction(
        &self,
        request_id: RequestId,
        old_session_id: &str,
        new_session_id: &str,
        summary: &str,
        hydrated: HydratedSession,
    ) -> DispatchBatch {
        let id = request_id.clone();
        answered(
            id,
            self.settle_manual_compaction(
                request_id,
                old_session_id,
                new_session_id,
                summary,
                hydrated,
            ),
        )
    }

    fn settle_manual_compaction(
        &self,
        request_id: RequestId,
        old_session_id: &str,
        new_session_id: &str,
        summary: &str,
        hydrated: HydratedSession,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let (state, event_id, emitted_at) = {
            let mut sessions = self.lock_sessions()?;
            let source_key = sessions
                .key(old_session_id)
                .map(ToOwned::to_owned)
                .ok_or_else(|| session_missing("Session was not found"))?;
            // Past this point the reservation belongs to this call, so every
            // refusal releases it: a compaction that failed must not leave the
            // session unable to close or to take another turn.
            let rebound = self.rebind_compacted_session(
                &mut sessions,
                &source_key,
                old_session_id,
                new_session_id,
                hydrated,
            );
            if rebound.is_err()
                && let Some(session) = sessions.get_mut(&source_key)
            {
                session.compaction_pending = false;
            }
            rebound?
        };
        let result = result_map([
            ("summary", json!(summary)),
            ("state", state.clone()),
            ("sessionLog", json!({"enabled": false})),
        ]);
        let notification = encode_notification(
            "session/compacted",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(new_session_id)),
                ("oldSessionId", json!(old_session_id)),
                ("state", state),
                ("sessionLog", json!({"enabled": false})),
                ("summaryLength", json!(summary.chars().count())),
                ("emittedAt", json!(emitted_at)),
            ]),
        );
        Ok(DispatchBatch {
            outbound: vec![success_bytes(request_id, result), notification],
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    /// Moves a compacted session onto the identifier its summary was written
    /// under, keeping the previous one addressable as an alias.
    ///
    /// Returns the state a client reads the compacted session through, the
    /// event identifier the notification carries and the time it was emitted.
    fn rebind_compacted_session(
        &self,
        sessions: &mut SessionRegistry,
        source_key: &str,
        old_session_id: &str,
        new_session_id: &str,
        hydrated: HydratedSession,
    ) -> Result<(Value, u64, u64), ProtocolFault> {
        if sessions
            .key(new_session_id)
            .is_some_and(|existing| existing != source_key)
        {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Compaction target session already exists",
            ));
        }
        let previous_id = {
            let session = sessions
                .get(source_key)
                .ok_or_else(|| session_missing("Session was not found"))?;
            if !session.compaction_pending || session.active_turn.is_some() {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "Compaction reservation is stale",
                ));
            }
            session.id.clone()
        };
        if hydrated.metadata.id != new_session_id
            || hydrated.metadata.parent_session_id.as_deref() != Some(old_session_id)
        {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::CompactionFailed,
                "Compaction produced an invalid session handoff",
            ));
        }
        self.release4
            .rebind_session(&previous_id, new_session_id)
            .map_err(|error| ProtocolFault::from(ServerError::Release4(error.to_string())))?;
        sessions.rename(source_key, new_session_id)?;
        sessions.alias(source_key, old_session_id);
        let session = sessions
            .get_mut(source_key)
            .ok_or_else(|| session_missing("Session was not found"))?;
        session.intent.resume = Some(new_session_id.to_owned());
        session.status = SessionStatus::Idle;
        session.compaction_pending = false;
        // Compaction replaces the message list, so every turn identifier the
        // checkpoint log holds now points at a position that moved. The log is
        // emptied rather than renumbered, and a turn that was open is reopened
        // at the new length so the tool loop still running has somewhere to
        // record.
        let message_count = hydrated.messages.len();
        if let Some(review) = &session.review {
            review
                .reset_messages(message_count)
                .map_err(|error| ProtocolFault::from(ServerError::Resource(error.to_string())))?;
        }
        session.persisted = Some(hydrated);
        session.updated_at = now_millis();
        session.event_watermark = 0;
        if let Some(snapshot) = session.snapshot.as_mut() {
            snapshot.session_id = new_session_id.to_owned();
            snapshot.turn_id = None;
            snapshot.watermark = 0;
            snapshot.lifecycle = LifecycleState::Idle;
            for entry in &mut snapshot.history {
                entry.rebind_session(new_session_id);
            }
        }
        if let Some(callback) = session.pending_callback.as_mut() {
            callback.entry.rebind_session(new_session_id);
        }
        if let Some(turn) = session.latest_turn.as_mut() {
            turn.session_id = new_session_id.to_owned();
        }
        let event_id = next_event_id(session);
        Ok((public_session_state(session), event_id, now_millis()))
    }

    pub fn fail_manual_compaction(
        &self,
        request_id: RequestId,
        session_id: &str,
        reason: &str,
    ) -> DispatchBatch {
        if let Ok(mut sessions) = self.lock_sessions()
            && let Some(session) = sessions.get_mut(session_id)
        {
            session.compaction_pending = false;
            session.updated_at = now_millis();
        }
        error_batch(request_id, ProtocolErrorCode::CompactionFailed, reason)
    }
}
