//! Routing a review request to the session it names, behind the reference's
//! guards. The guards and the answers are `crate::server::review`'s; what is
//! here is which session a connection may ask about.

use super::*;

impl ServerConnection {
    pub(super) fn review_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.answer_review(&request))
    }

    fn answer_review(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        if self.attached_sessions.is_empty() {
            return Err(review::no_session());
        }
        {
            let sessions = self.server.lock_sessions()?;
            if let Some(session) = self
                .attached_sessions
                .iter()
                .filter_map(|key| sessions.get(key))
                .find(|session| session.compaction_pending)
            {
                return Err(review::in_lifecycle(&session.id));
            }
        }
        let validated = review::validate(request)?;
        let (engine, active_turn) = {
            let sessions = self.server.lock_sessions()?;
            // Reference `_require_session`: only the connection's own current
            // session answers, under its current identifier.
            let session = sessions
                .get(&validated.session_id)
                .filter(|session| session.id == validated.session_id)
                .filter(|_| {
                    sessions
                        .key(&validated.session_id)
                        .is_some_and(|key| self.attached_sessions.contains(key))
                })
                .ok_or_else(|| {
                    ProtocolFault::new(
                        ProtocolErrorCode::NotFound,
                        format!("Session not found: {}", validated.session_id),
                    )
                })?;
            (session.review.clone(), session.active_turn.clone())
        };
        if validated.call.is_mutation()
            && let Some(turn_id) = active_turn
        {
            return Err(review::busy(&turn_id));
        }
        let result = review::answer(&validated.call, engine.as_deref());
        // A read reconciles a hand edit into the log, which is where the log
        // can run out of room; the warning is published the way a turn's is.
        if let Some(engine) = &engine {
            self.server.publish_retention_notice(engine);
        }
        Ok(success_batch(request.id.clone(), result?))
    }
}
