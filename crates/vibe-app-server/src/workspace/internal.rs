//! The saved-session operations this port's own clients reach in process.
//!
//! The terminal client and the editor adapter predate the reference's session
//! surface and still read the shapes this port first answered: a page of
//! summaries, the stored transcript by offset, and a hydrated session with its
//! metadata and messages. They are addressed as `internal/<method>` over an
//! in-process connection only (`crate::server::INTERNAL_METHOD_PREFIX`), so no stdio
//! client reaches them, and the reference methods of the same names are
//! answered by `server/connection/saved.rs`.

use super::config::config_map;
use super::sessions::{hydrated_result, runtime_attachment};
use super::*;

impl WorkspaceService {
    pub(super) fn session_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", 50, 1, 500)?;
        let cwd = optional_string(params, "cwd")?;
        self.store.migrate_legacy().map_err(storage_error)?;
        let sessions = self
            .store
            .sessions(cwd)
            .map_err(storage_error)?
            .into_iter()
            .skip(offset)
            .take(limit)
            .filter_map(|session| {
                let metadata = self.store.metadata(&session.session_id).ok()?;
                Some(json!({
                    "id": session.session_id,
                    "title": session.title,
                    "startTime": metadata.start_time,
                    "endTime": metadata.end_time,
                    "workingDirectory": session.cwd,
                    "parentSessionId": session.parent_session_id,
                    "messageCount": metadata.message_count,
                }))
            })
            .collect::<Vec<_>>();
        Ok(WorkspaceDispatch::result([("sessions", json!(sessions))]))
    }

    pub(super) fn history_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", 100, 1, 500)?;
        // A fork a rewind left unwritten is read where it is held.
        let draft = self.lock_drafts()?.get(session_id).map(|draft| {
            draft
                .messages
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>()
        });
        let history = match draft {
            Some(history) => history,
            None => self
                .store
                .open(session_id)
                .map_err(storage_error)?
                .messages
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect(),
        };
        Ok(WorkspaceDispatch::result([(
            "history",
            serde_json::to_value(history)?,
        )]))
    }

    pub(super) fn session_log(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let draft = self.lock_drafts()?.get(session_id).cloned();
        let hydrated = match draft {
            Some(draft) => draft,
            None => self.store.open(session_id).map_err(storage_error)?,
        };
        Ok(hydrated_result(&hydrated, None))
    }

    pub(super) fn fork(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let source = required_string(params, "sessionId")?;
        let keep_messages = match optional_string(params, "messageId")? {
            Some(message_id) if !message_id.starts_with("history-") => {
                let stored = self.store.load(source).map_err(storage_error)?;
                Some(fork_point(&stored.messages, message_id)?)
            }
            _ => fork_keep_messages(params)?,
        };
        let new_id = match optional_string(params, "newSessionId")? {
            Some(new_id) => new_id.to_owned(),
            None => vibe_core::session_id::rotate_session_id(source),
        };
        let mut hydrated = self
            .store
            .fork(
                source,
                &new_id,
                optional_string(params, "systemPrompt")?.unwrap_or_default(),
                config_map(params.get("config"))?,
                now_millis(),
            )
            .map_err(storage_error)?;
        // Reference `SessionRuntime.fork` saves the copy with fresh
        // statistics: the fork has spent nothing yet.
        if let Some(keep_messages) = keep_messages {
            hydrated = self
                .store
                .rewind(
                    &hydrated.metadata.id,
                    keep_messages,
                    BTreeMap::new(),
                    now_millis(),
                )
                .map_err(storage_error)?;
        } else {
            hydrated.metadata.statistics = BTreeMap::new();
            self.store
                .update_metadata(&hydrated.metadata)
                .map_err(storage_error)?;
        }
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn title_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let title = required_string(params, "title")?.trim();
        if title.is_empty() {
            return Err(storage_error(StorageError::InvalidTitle));
        }
        let mut metadata = self.store.open(session_id).map_err(storage_error)?.metadata;
        metadata.title = Some(title.to_owned());
        "manual".clone_into(&mut metadata.title_source);
        self.store
            .update_metadata(&metadata)
            .map_err(storage_error)?;
        Ok(WorkspaceDispatch::result([(
            "metadata",
            serde_json::to_value(metadata)?,
        )]))
    }

    pub(super) fn delete(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let snapshot = match self.store.load(session_id) {
            Ok(snapshot) => Some(snapshot),
            Err(StorageError::SessionNotFound(_)) => None,
            Err(error) => return Err(storage_error(error)),
        };
        self.continuity
            .remove(session_id)
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        match self.store.delete(session_id) {
            Ok(_) | Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => {
                if let Some(snapshot) = snapshot
                    && let Err(rollback) = self.continuity.refresh(snapshot)
                {
                    return Err(WorkspaceServiceError::Storage(format!(
                        "session delete failed ({error}); continuity rollback failed ({rollback})"
                    )));
                }
                return Err(storage_error(error));
            }
        }
        Ok(WorkspaceDispatch::result([("deleted", json!(true))]))
    }

    /// Reference `_history_clear`: the conversation continues under a rotated
    /// identifier with nothing said yet, and the session it replaces is left on
    /// disk untouched, so `vibe --resume` can still reach it. The new session
    /// records no parent, because what it continues was discarded.
    pub(super) fn history_clear(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let source = self
            .store
            .open(required_string(params, "sessionId")?)
            .map_err(storage_error)?;
        let new_id = vibe_core::session_id::rotate_session_id(&source.metadata.id);
        let now = now_millis();
        let mut metadata = self
            .store
            .handoff_messages(&source.metadata, &new_id, &[], now, false)
            .map_err(storage_error)?;
        metadata.statistics = BTreeMap::new();
        self.store
            .update_metadata(&metadata)
            .map_err(storage_error)?;
        let hydrated = self.store.open(&new_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }
}

/// How many stored messages a fork anchored at the user message `message_id`
/// keeps: that message and the turn it opened. Reference `_messages_for_fork`
/// (`vibe/app_server/_runtime.py`).
fn fork_point(messages: &[ModelMessage], message_id: &str) -> Result<usize, WorkspaceServiceError> {
    let anchor = messages
        .iter()
        .position(|message| match message {
            ModelMessage::User { message_id: id, .. } => id.as_deref() == Some(message_id),
            ModelMessage::Assistant { message_id: id, .. } => id.as_deref() == Some(message_id),
            _ => false,
        })
        .ok_or_else(|| {
            WorkspaceServiceError::InvalidParams(format!(
                "no message named `{message_id}` can anchor a fork"
            ))
        })?;
    if !matches!(messages.get(anchor), Some(ModelMessage::User { .. })) {
        return Err(WorkspaceServiceError::InvalidParams(
            "a fork can only be anchored at a user message".to_owned(),
        ));
    }
    Ok(messages
        .iter()
        .enumerate()
        .skip(anchor + 1)
        .find(|(_, message)| matches!(message, ModelMessage::User { .. }))
        .map_or(messages.len(), |(index, _)| index))
}

pub(super) fn fork_keep_messages(
    params: &BTreeMap<String, Value>,
) -> Result<Option<usize>, WorkspaceServiceError> {
    let explicit = params
        .get("keepMessages")
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    WorkspaceServiceError::InvalidParams(
                        "keepMessages must be a non-negative integer".to_owned(),
                    )
                })
        })
        .transpose()?;
    let anchored = params
        .get("messageId")
        .map(|value| {
            let message_id = value.as_str().ok_or_else(|| {
                WorkspaceServiceError::InvalidParams("messageId must be a string".to_owned())
            })?;
            let index = message_id
                .strip_prefix("history-")
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| {
                    WorkspaceServiceError::InvalidParams(
                        "messageId must use the stable `history-N` form".to_owned(),
                    )
                })?;
            index.checked_add(1).ok_or_else(|| {
                WorkspaceServiceError::InvalidParams("messageId index is too large".to_owned())
            })
        })
        .transpose()?;
    match (explicit, anchored) {
        (Some(explicit), Some(anchored)) if explicit != anchored => {
            Err(WorkspaceServiceError::InvalidParams(
                "keepMessages and messageId identify different fork anchors".to_owned(),
            ))
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}
