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
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        check.extras(&object, &["sessionId"], &[]);
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        if self.server.harness_selection().use_unified
            && let Some(deleted) = self.unified_delete(&session_id)
        {
            deleted?;
            return Ok(success_batch(request.id.clone(), BTreeMap::new()));
        }
        let held = self.root_id().as_deref() == Some(session_id.as_str())
            || self.server.lock_sessions().is_ok_and(|sessions| {
                sessions.get(&session_id).is_some_and(|session| {
                    session.id == session_id
                        && session.attachments > 0
                        && session.status != SessionStatus::Closed
                })
            });
        if held {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Deleting a live session is not supported",
            ));
        }
        let store = self.store();
        // Reference `delete` on the unified host looks for a legacy transcript
        // only once the harness holds no such session, and reports a missing
        // one; the legacy host deletes nothing quietly.
        let unified = self.server.harness_selection().use_unified;
        crate::session_lifecycle::delete_session_transactionally(
            &self.server.projects,
            &session_id,
            || {
                store
                    .delete(&session_id)
                    .and_then(|deleted| {
                        if unified && !deleted {
                            Err(StorageError::SessionNotFound(session_id.clone()))
                        } else {
                            Ok(())
                        }
                    })
                    .or_else(|error| match error {
                        StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_)
                            if !unified =>
                        {
                            Ok(())
                        }
                        StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_) => {
                            Err(StorageError::SessionNotFound(session_id.clone()))
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
