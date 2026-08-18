//! The saved-session methods: listing, resuming, continuing, forking, rewinding,
//! retitling, clearing and deleting.
//!
//! `vibe_core::storage` owns the transcript on disk. What is here is the
//! boundary shape, the parameters each method reads, and the runtime attachment
//! a resumed session hands back to the server.

use super::config::config_map;
use super::*;

impl Release3Service {
    pub(crate) fn message_count(&self, session_id: &str) -> Result<Option<usize>, Release3Error> {
        match self.store.load(session_id) {
            Ok(hydrated) => Ok(Some(hydrated.messages.len())),
            Err(StorageError::SessionNotFound(_)) => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }

    pub(crate) fn snapshot_session(
        &self,
        session_id: &str,
    ) -> Result<HydratedSession, Release3Error> {
        self.store.load(session_id).map_err(storage_error)
    }

    /// Where `entry_id` sits in the stored message list.
    ///
    /// # Errors
    ///
    /// Reports the session storage failure, and answers `NotFound` when no
    /// rewindable user entry carries the identifier.
    pub(crate) fn rewind_entry_index(
        &self,
        session_id: &str,
        entry_id: &str,
    ) -> Result<usize, Release3Error> {
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        rewind_entry_index(&hydrated.messages, entry_id)
    }

    pub(crate) fn rollback_rewind(
        &self,
        source: HydratedSession,
        result_session_id: &str,
    ) -> Result<(), Release3Error> {
        let mut failures = Vec::new();
        if result_session_id == source.metadata.id {
            let mut metadata = source.metadata.clone();
            match self.store.replace_messages(
                &mut metadata,
                &source.messages,
                source.metadata.updated_at_ms,
            ) {
                Ok(()) => {
                    if let Err(error) = self.store.update_metadata(&source.metadata) {
                        failures.push(error.to_string());
                    }
                }
                Err(error) => failures.push(error.to_string()),
            }
        } else {
            match self.store.delete(result_session_id) {
                Ok(()) | Err(StorageError::SessionNotFound(_)) => {}
                Err(error) => failures.push(error.to_string()),
            }
            if let Err(error) = self.continuity.remove(result_session_id) {
                failures.push(error.to_string());
            }
        }
        if let Err(error) = self.store.select_for_continue(&source.metadata.id) {
            failures.push(error.to_string());
        }
        if let Err(error) = self.continuity.refresh(source) {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Release3Error::Storage(failures.join("; ")))
        }
    }

    pub(crate) fn rewind_after_workspace_restore(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        self.rewind_impl(params, true)
    }

    pub fn update_runtime_settings(
        &self,
        session_id: &str,
        settings: &BTreeMap<String, Value>,
    ) -> Result<Option<HydratedSession>, Release3Error> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        let mut hydrated = self.store.load(session_id).map_err(storage_error)?;
        hydrated.metadata.config.extend(settings.clone());
        hydrated.metadata.updated_at_ms = now_millis();
        self.store
            .update_metadata(&hydrated.metadata)
            .map_err(storage_error)?;
        hydrated.current_config = hydrated.metadata.config.clone();
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(Some(hydrated))
    }

    pub fn create_runtime_session(
        &self,
        session_id: &str,
        working_directory: &str,
        now_ms: u64,
    ) -> Result<HydratedSession, Release3Error> {
        match self.store.load(session_id) {
            Ok(_) => {
                return Err(Release3Error::Storage(format!(
                    "session `{session_id}` already exists"
                )));
            }
            Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => return Err(storage_error(error)),
        }
        self.store
            .create(session_id, working_directory, None, now_ms)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated)
    }

    pub fn update_runtime_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<HydratedSession>, Release3Error> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        self.set_session_agent(session_id, name)
            .map(|(_, hydrated)| Some(hydrated))
    }

    pub fn close_saved_session(&self, session_id: &str, now_ms: u64) -> Result<(), Release3Error> {
        match self.store.close(session_id, now_ms) {
            Ok(_) | Err(StorageError::SessionNotFound(_)) => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
    }

    pub(super) fn session_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", 50, 0, usize::MAX)?;
        let cwd = params.get("cwd").and_then(Value::as_str);
        // The legacy migration still runs before the page is read, so a store
        // written by an older layout is listed; what it moved is not published,
        // because `SessionListResponse` declares the page and nothing else.
        self.store.migrate_legacy().map_err(storage_error)?;
        let page = self.store.list(cwd, offset, limit).map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "sessions",
            serde_json::to_value(page.sessions)?,
        )]))
    }

    pub(super) fn history_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let history = self
            .store
            .history(
                session_id,
                usize_param(params, "offset", 0, 0, usize::MAX)?,
                usize_param(params, "limit", 100, 0, usize::MAX)?,
            )
            .map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "history",
            serde_json::to_value(history)?,
        )]))
    }

    pub(super) fn session_log(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .load(required_string(params, "sessionId")?)
            .map_err(storage_error)?;
        Ok(hydrated_result(&hydrated, None))
    }

    pub(super) fn resume(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .resume(
                required_string(params, "sessionId")?,
                swallowed_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn continue_session(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let cwd = swallowed_string(params, "cwd")
            .unwrap_or_else(|| self.paths.working_directory.to_string_lossy().into_owned());
        let hydrated = self
            .store
            .continue_session(
                &cwd,
                swallowed_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn fork(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let source = required_string(params, "sessionId")?;
        let keep_messages = fork_keep_messages(params)?;
        let new_id = swallowed_string(params, "newSessionId").unwrap_or_else(|| {
            format!(
                "session-{}-{}",
                now_millis(),
                self.next_session.fetch_add(1, Ordering::Relaxed)
            )
        });
        let mut hydrated = self
            .store
            .fork(
                source,
                &new_id,
                &swallowed_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
                now_millis(),
            )
            .map_err(storage_error)?;
        if let Some(keep_messages) = keep_messages {
            hydrated = self
                .store
                .rewind(
                    &hydrated.metadata.id,
                    keep_messages,
                    hydrated.metadata.statistics.clone(),
                    now_millis(),
                )
                .map_err(storage_error)?;
        }
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn title_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let metadata = self
            .store
            .update_title(
                required_string(params, "sessionId")?,
                required_string(params, "title")?,
                now_millis(),
            )
            .map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "metadata",
            serde_json::to_value(metadata)?,
        )]))
    }

    pub(super) fn delete(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let snapshot = match self.store.load(session_id) {
            Ok(snapshot) => Some(snapshot),
            Err(StorageError::SessionNotFound(_)) => None,
            Err(error) => return Err(storage_error(error)),
        };
        self.continuity
            .remove(session_id)
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        match self.store.delete(session_id) {
            Ok(()) | Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => {
                if let Some(snapshot) = snapshot
                    && let Err(rollback) = self.continuity.refresh(snapshot)
                {
                    return Err(Release3Error::Storage(format!(
                        "session delete failed ({error}); continuity rollback failed ({rollback})"
                    )));
                }
                return Err(storage_error(error));
            }
        }
        Ok(Release3Dispatch::result([("deleted", json!(true))]))
    }

    pub(super) fn rewind(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        self.rewind_impl(params, false)
    }

    pub(super) fn rewind_impl(
        &self,
        params: &BTreeMap<String, Value>,
        workspace_restore_handled: bool,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let entry_id = required_string(params, "entryId")?;
        let restore_files = params
            .get("restoreFiles")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    Release3Error::InvalidParams("restoreFiles must be a boolean".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(false);
        if restore_files && !workspace_restore_handled {
            return Err(Release3Error::InvalidParams(
                "this session has no restorable file checkpoint".to_owned(),
            ));
        }
        let inplace = params
            .get("inplace")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    Release3Error::InvalidParams("inplace must be a boolean".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(false);
        let source = self.store.load(session_id).map_err(storage_error)?;
        let keep_messages = rewind_entry_index(&source.messages, entry_id)?;
        let message = source
            .messages
            .get(keep_messages)
            .and_then(|message| match message {
                ModelMessage::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let requested_statistics = statistics_map(params.get("statistics"))?;
        let rewind_statistics = if requested_statistics.is_empty() {
            source.metadata.statistics.clone()
        } else {
            requested_statistics
        };
        let timestamp = now_millis();
        let hydrated = if inplace {
            self.store
                .rewind(session_id, keep_messages, rewind_statistics, timestamp)
                .map_err(storage_error)?
        } else {
            let new_id = format!(
                "session-{}-{}",
                timestamp,
                self.next_session.fetch_add(1, Ordering::Relaxed)
            );
            self.store
                .fork_rewound(
                    session_id,
                    &new_id,
                    keep_messages,
                    rewind_statistics,
                    timestamp,
                )
                .map_err(storage_error)?
        };
        if let Err(error) = self.continuity.refresh(hydrated.clone()) {
            if let Err(rollback) = self.rollback_rewind(source, &hydrated.metadata.id) {
                return Err(Release3Error::Storage(format!(
                    "continuity refresh failed ({error}); rewind rollback failed ({rollback})"
                )));
            }
            return Err(Release3Error::Storage(error.to_string()));
        }
        // `SessionRewindResponse` declares five fields and this service can
        // answer three of them: `state` and `sessionLog` are composed from the
        // live session the attachment below rebinds, which only the server
        // holds. The two lists are placeholders the workspace restore replaces.
        Ok(Release3Dispatch {
            result: [
                ("message".to_owned(), json!(message)),
                ("restoreErrors".to_owned(), json!([])),
                ("restoredPaths".to_owned(), json!([])),
            ]
            .into_iter()
            .collect(),
            attachment: Some(runtime_attachment(&hydrated)),
        })
    }

    /// Whether rewinding to one history entry would change files, and which.
    ///
    /// The entry is resolved here so an identifier no rewindable message
    /// carries is refused before anything reads a workspace. The two fields are
    /// answered empty: the paths come from the session's checkpoint log, which
    /// the server holds and fills in.
    pub(super) fn rewind_read(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let entry_id = required_string(params, "entryId")?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        rewind_entry_index(&hydrated.messages, entry_id)?;
        Ok(Release3Dispatch::result([
            ("hasFileChanges", json!(false)),
            ("paths", json!([])),
        ]))
    }

    pub(super) fn history_clear(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .rewind(
                required_string(params, "sessionId")?,
                0,
                BTreeMap::new(),
                now_millis(),
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn set_session_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(AgentProfile, HydratedSession), Release3Error> {
        let profile = self.agent_profile(name)?;
        let mut metadata = self.store.load(session_id).map_err(storage_error)?.metadata;
        metadata.agent_profile = Some(serde_json::to_value(&profile)?);
        self.store
            .update_metadata(&metadata)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok((profile, hydrated))
    }
}

pub(super) fn fork_keep_messages(
    params: &BTreeMap<String, Value>,
) -> Result<Option<usize>, Release3Error> {
    let explicit = params
        .get("keepMessages")
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    Release3Error::InvalidParams(
                        "keepMessages must be a non-negative integer".to_owned(),
                    )
                })
        })
        .transpose()?;
    let anchored = params
        .get("messageId")
        .map(|value| {
            let message_id = value.as_str().ok_or_else(|| {
                Release3Error::InvalidParams("messageId must be a string".to_owned())
            })?;
            let index = message_id
                .strip_prefix("history-")
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| {
                    Release3Error::InvalidParams(
                        "messageId must use the stable `history-N` form".to_owned(),
                    )
                })?;
            index.checked_add(1).ok_or_else(|| {
                Release3Error::InvalidParams("messageId index is too large".to_owned())
            })
        })
        .transpose()?;
    match (explicit, anchored) {
        (Some(explicit), Some(anchored)) if explicit != anchored => {
            Err(Release3Error::InvalidParams(
                "keepMessages and messageId identify different fork anchors".to_owned(),
            ))
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

/// The identity a stored message is addressed by on the wire.
///
/// A stored message carries no identifier of its own here, so the identity is
/// the one the reference falls back to when a message has none: its position in
/// the list and its role. Mirrors `history_message_id`
/// (`vibe/app_server/_projection.py:607`).
pub(crate) fn history_entry_id(index: usize, role: &str) -> String {
    format!("history:{index}:{role}")
}

/// Which stored message `entry_id` names, among the ones a rewind may target.
///
/// Only a user message is rewindable, which is what makes the position the
/// rewind cuts at the position of the message the operator is about to edit.
/// Mirrors `history_user_message_index`
/// (`vibe/app_server/_projection.py:611`).
pub(super) fn rewind_entry_index(
    messages: &[ModelMessage],
    entry_id: &str,
) -> Result<usize, Release3Error> {
    messages
        .iter()
        .enumerate()
        .find(|(index, message)| {
            matches!(message, ModelMessage::User { .. })
                && history_entry_id(*index, "user") == entry_id
        })
        .map(|(index, _message)| index)
        .ok_or_else(|| {
            Release3Error::NotFound(format!("Rewindable history entry not found: {entry_id}"))
        })
}

pub(super) fn hydrated_result(
    hydrated: &HydratedSession,
    attachment: Option<RuntimeAttachment>,
) -> Release3Dispatch {
    Release3Dispatch {
        result: [
            ("metadata".to_owned(), json!(hydrated.metadata)),
            ("messages".to_owned(), json!(hydrated.messages)),
            ("currentConfig".to_owned(), json!(hydrated.current_config)),
        ]
        .into_iter()
        .collect(),
        attachment,
    }
}

pub(super) fn runtime_attachment(hydrated: &HydratedSession) -> RuntimeAttachment {
    let agent_profile: Option<AgentProfile> = hydrated
        .metadata
        .agent_profile
        .as_ref()
        .and_then(|profile| serde_json::from_value(profile.clone()).ok());
    RuntimeAttachment {
        id: hydrated.metadata.id.clone(),
        working_directory: hydrated.metadata.working_directory.clone(),
        parent_session_id: hydrated.metadata.parent_session_id.clone(),
        agent: agent_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .or_else(|| {
                hydrated
                    .metadata
                    .agent_profile
                    .as_ref()
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        agent_profile,
        hydrated: hydrated.clone(),
    }
}

pub(super) fn statistics_map(
    value: Option<&Value>,
) -> Result<BTreeMap<String, Value>, Release3Error> {
    config_map(value)
}

pub(super) fn swallowed_string(values: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    crate::params::optional_string(values, key)
        .ok()
        .flatten()
        .map(ToOwned::to_owned)
}
