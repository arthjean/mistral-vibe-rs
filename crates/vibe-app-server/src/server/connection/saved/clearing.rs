//! Clearing the root's history: the root continues under a new identifier,
//! and the session it leaves stays saved.
//!
//! Reference `_history_clear` (`vibe/app_server/_handler.py`).

use super::*;

impl ServerConnection {
    /// Reference `_history_clear`: the root continues under a new identifier
    /// with nothing said yet, and the session it leaves stays saved.
    pub(super) fn clear_history(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        check.extras(&object, &["sessionId"], &[]);
        if self.root_key().is_none() {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        }
        check.finish().map_err(rejected)?;
        let session_id = self.require_root(&session_id.unwrap_or_default())?;
        let key = self.root_key().unwrap_or_else(|| session_id.clone());
        {
            let sessions = self.server.lock_sessions()?;
            if sessions
                .get(&key)
                .is_some_and(|session| session.active_turn.is_some())
            {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "Cannot clear history while a turn is active",
                ));
            }
        }
        let workspace = &self.server.workspace;
        let source = match workspace.publish_draft(&session_id)? {
            Some(published) => published,
            None => workspace.load_session(&session_id)?,
        };
        let draft = workspace.clear_session(&source)?;
        let new_id = draft.metadata.id.clone();
        self.server.transfer_lease(&session_id, &new_id)?;
        self.server
            .projects
            .rebind_session(&session_id, &new_id)
            .map_err(|error| ServerError::Projects(error.to_string()))?;
        let now = now_millis();
        let mut sessions = self.server.lock_sessions()?;
        sessions.rename(&key, &new_id)?;
        let session = sessions
            .get_mut(&new_id)
            .ok_or_else(|| not_found(&new_id))?;
        session.intent.resume = Some(new_id.clone());
        session.persisted = Some(draft);
        session.created_at = now;
        session.updated_at = now;
        session.bumped_at = None;
        session.event_watermark = 0;
        session.stats = crate::server::runtime::SessionStats::default();
        // Reference `_reset_title_state`: a title still running names the
        // session left behind, and the cadence starts over.
        session.title_cadence = vibe_core::session_title::TitleCadence::default();
        session.title_in_flight = false;
        let mut kept = session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.history.clone())
            .unwrap_or_default();
        for entry in &mut kept {
            entry.rebind_session(new_id.clone());
        }
        kept.push(checkpoint_entry(
            &new_id,
            "clear",
            "New conversation started",
            Value::Null,
        ));
        let snapshot = session.projection();
        snapshot.title = None;
        snapshot.watermark = 0;
        session.replace_idle(kept);
        session.status = SessionStatus::Idle;
        let state = public_session_state(session);
        drop(sessions);
        super::session_management::reset_checkpoint_log(self, &new_id, 1)?;
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("state", state),
                ("sessionLog", self.server.session_log_summary(&new_id)),
            ]),
        ))
    }
}
