//! Renaming a saved session, and reading where its log is written.

use super::*;

impl ServerConnection {
    /// Reference `session/rename`: the live session renames itself, and the
    /// store renames any other (`update_saved_session_title`).
    pub(super) fn rename_session(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let title = check.required_string(&object, "title", &[]);
        check.extras(&object, &["sessionId", "title"], &[]);
        check.finish().map_err(rejected)?;
        let (session_id, title) = (session_id.unwrap_or_default(), title.unwrap_or_default());
        let store = self.store();
        let live = self.root_id().as_deref() == Some(session_id.as_str());
        let metadata = if live {
            rename_live(&store, &session_id, &title)
        } else {
            store.update_title(&session_id, &title)
        }
        .map_err(|error| match error {
            StorageError::InvalidTitle => {
                ProtocolFault::plain(ProtocolErrorCode::InvalidParams, error.to_string())
            }
            StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_) => {
                not_found(&session_id)
            }
            error => storage_fault(error),
        })?;
        if live
            && let Ok(mut sessions) = self.server.lock_sessions()
            && let Some(session) = sessions.get_mut(&session_id)
        {
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.title.clone_from(&metadata.title);
            }
            if let Some(persisted) = session.persisted.as_mut() {
                persisted.metadata.title.clone_from(&metadata.title);
                persisted
                    .metadata
                    .title_source
                    .clone_from(&metadata.title_source);
            }
        }
        let updated_at = metadata
            .is_persisted(&store)
            .then(|| metadata.end_time.clone())
            .flatten();
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("title", json!(metadata.title)),
                ("updatedAt", json!(updated_at)),
                ("lastEventId", Value::Null),
            ]),
        ))
    }

    /// Reference `_session_log_read`: what the root session writes, and where.
    pub(super) fn read_session_log(
        &self,
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
        Ok(success_batch(
            request.id.clone(),
            result_map([("log", self.server.session_log_summary(&session_id))]),
        ))
    }
}

/// Reference `AgentLoop.rename`: the root is renamed whether or not it was
/// written yet, and an unwritten one takes the title with its first save.
fn rename_live(
    store: &SessionStore,
    session_id: &str,
    title: &str,
) -> Result<vibe_core::storage::SessionMetadata, StorageError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(StorageError::InvalidTitle);
    }
    let mut metadata = store.open(session_id)?.metadata;
    metadata.title = Some(title.to_owned());
    "manual".clone_into(&mut metadata.title_source);
    store.update_metadata(&metadata)?;
    Ok(metadata)
}
