//! Deleting a saved session, with whatever links to it.
//!
//! Reference `_delete_session` (`vibe/app_server/server.py`).

use super::*;

impl ServerConnection {
    /// Reference `_delete_session`: the root cannot be deleted from under
    /// itself, and any other session is removed with whatever links to it.
    pub(super) fn delete_saved_session(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let SessionParams { session_id } =
            wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        self.delete_transcript(request, &session_id, false)
    }

    /// Reference `delete` on the unified host: a session the harness stores is
    /// removed from its store, and any other from the legacy transcripts,
    /// where a missing one is reported.
    pub(in crate::server::connection) fn unified_delete_session(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let SessionParams { session_id } =
            wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        if let Some(deleted) = self.unified_delete(&session_id) {
            deleted?;
            return Ok(success_batch(request.id.clone(), BTreeMap::new()));
        }
        self.delete_transcript(request, &session_id, true)
    }

    /// Deletes the saved transcript of `session_id` with whatever links to
    /// it, unless a connection holds it. A missing transcript is reported
    /// when `report_missing`, and quietly deleted otherwise.
    fn delete_transcript(
        &self,
        request: &ServerRequest,
        session_id: &str,
        report_missing: bool,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let held = self.root_id().as_deref() == Some(session_id)
            || self
                .server
                .lock_sessions()
                .get(session_id)
                .is_some_and(|session| {
                    session.id == session_id
                        && session.attachments > 0
                        && session.status != SessionStatus::Closed
                });
        if held {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Deleting a live session is not supported",
            ));
        }
        let store = self.store();
        crate::session_lifecycle::delete_session_transactionally(
            &self.server.projects,
            session_id,
            || {
                store
                    .delete(session_id)
                    .and_then(|deleted| {
                        if report_missing && !deleted {
                            Err(StorageError::SessionNotFound(session_id.to_owned()))
                        } else {
                            Ok(())
                        }
                    })
                    .or_else(|error| match error {
                        StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_)
                            if !report_missing =>
                        {
                            Ok(())
                        }
                        StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_) => {
                            Err(StorageError::SessionNotFound(session_id.to_owned()))
                        }
                        error => Err(error),
                    })
            },
        )
        .map_err(|error| match error {
            crate::session_lifecycle::DeleteSessionError::Prepare(error) => {
                ProtocolFault::from(error)
            }
            crate::session_lifecycle::DeleteSessionError::Delete(
                StorageError::SessionNotFound(missing),
            ) => ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {missing}"),
            ),
            crate::session_lifecycle::DeleteSessionError::Delete(error) => storage_fault(error),
            crate::session_lifecycle::DeleteSessionError::Rollback { delete, rollback } => {
                ProtocolFault::internal(format!(
                    "session delete failed ({delete}); rollback failed ({rollback})"
                ))
            }
        })?;
        Ok(success_batch(request.id.clone(), BTreeMap::new()))
    }
}
