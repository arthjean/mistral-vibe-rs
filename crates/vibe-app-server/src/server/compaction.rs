//! Manual compaction, and the state it answers with.
//!
//! A compaction appends its envelope to the conversation and keeps the session
//! it ran in, as upstream does since the envelope stopped replacing the
//! transcript: the history a client holds stays valid, and a checkpoint marks
//! where the model's context now starts. Reference `_handler._compact`
//! (`vibe/app_server/_handler.py`).

use super::*;

impl AppServer {
    pub fn complete_manual_compaction(
        &self,
        request_id: RequestId,
        session_id: &str,
        summary: &str,
        hydrated: HydratedSession,
    ) -> DispatchBatch {
        let id = request_id.clone();
        answered(
            id,
            self.settle_manual_compaction(request_id, session_id, summary, hydrated),
        )
    }

    fn settle_manual_compaction(
        &self,
        request_id: RequestId,
        session_id: &str,
        summary: &str,
        hydrated: HydratedSession,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let (state, canonical_session_id) = {
            let mut sessions = self.lock_sessions()?;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| session_missing("Session was not found"))?;
            // Past this point the reservation belongs to this call, so every
            // refusal releases it: a compaction that failed must not leave the
            // session unable to close or to take another turn.
            session.compaction_pending = false;
            if session.active_turn.is_some() {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "Compaction reservation is stale",
                ));
            }
            if hydrated.metadata.id != session.id {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::CompactionFailed,
                    "Compaction wrote to another session",
                ));
            }
            session.status = SessionStatus::Idle;
            session.persisted = Some(hydrated);
            session.updated_at = now_millis();
            // Reference `replace_idle`: the history is kept and closed by the
            // checkpoint, and the turns start over.
            let checkpoint = checkpoint_entry(
                &session.id,
                "compaction",
                "Context compacted",
                json!({"summaryLength": summary.chars().count()}),
            );
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.turn_id = None;
                snapshot.lifecycle = LifecycleState::Idle;
                snapshot.history.push(checkpoint);
            }
            session.turns.clear();
            session.latest_turn = None;
            (public_session_state(session), session.id.clone())
        };
        let result = result_map([
            ("summary", json!(summary)),
            ("state", state),
            (
                "sessionLog",
                self.session_log_summary(&canonical_session_id),
            ),
        ]);
        Ok(success_batch(request_id, result))
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
