//! The saved-session methods: listing, resuming, continuing, forking, rewinding,
//! retitling, clearing and deleting.
//!
//! `vibe_core::storage` owns the transcript on disk. What is here is the
//! boundary shape, the parameters each method reads, and the runtime attachment
//! a resumed session hands back to the server.

use super::config::config_map;
use super::*;

impl WorkspaceService {
    /// Where the store writes a session it names `directory`.
    pub(crate) fn session_path(&self, directory: &str) -> PathBuf {
        self.paths.session_root.join(directory)
    }

    /// The transcript `session_id` holds now: the fork a rewind left unwritten
    /// when there is one, and the stored session otherwise.
    ///
    /// # Errors
    ///
    /// Answers `NotFound` for a session neither holds, and reports any other
    /// storage failure.
    pub(crate) fn load_session(
        &self,
        session_id: &str,
    ) -> Result<HydratedSession, WorkspaceServiceError> {
        if let Some(draft) = self.lock_drafts()?.get(session_id) {
            return Ok(draft.clone());
        }
        self.store.load(session_id).map_err(|error| match error {
            StorageError::SessionNotFound(_) => {
                WorkspaceServiceError::NotFound(format!("Session not found: {session_id}"))
            }
            error => storage_error(error),
        })
    }

    /// Whether `session_id` is a fork no turn has written to disk yet.
    #[must_use]
    pub(crate) fn is_draft(&self, session_id: &str) -> bool {
        self.lock_drafts()
            .is_ok_and(|drafts| drafts.contains_key(session_id))
    }

    /// Forks `source` before the message at `keep_messages` without writing
    /// the fork, which stays in memory until [`Self::publish_draft`].
    ///
    /// Reference `AgentLoop._reset_session`: the fork keeps the stable suffix
    /// of the identifier it continues (`vibe/core/session/session_id.py`) and
    /// names that session its parent.
    pub(crate) fn fork_draft(
        &self,
        source: &HydratedSession,
        keep_messages: usize,
    ) -> Result<HydratedSession, WorkspaceServiceError> {
        let new_id = vibe_core::session_id::rotate_session_id(&source.metadata.id);
        let draft = self.store.draft_handoff(
            source,
            &new_id,
            keep_messages,
            source.metadata.statistics.clone(),
            now_millis(),
        );
        self.continuity
            .refresh(draft.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        self.lock_drafts()?.insert(new_id, draft.clone());
        Ok(draft)
    }

    /// Writes the draft fork `session_id` names, which is what its first save
    /// does upstream. Answers `None` when the session is not a draft.
    ///
    /// # Errors
    ///
    /// Reports the storage failure, leaving the draft in place so a later save
    /// can try again.
    pub(crate) fn publish_draft(
        &self,
        session_id: &str,
    ) -> Result<Option<HydratedSession>, WorkspaceServiceError> {
        let mut drafts = self.lock_drafts()?;
        let Some(draft) = drafts.get(session_id) else {
            return Ok(None);
        };
        let published = self
            .store
            .publish_draft(draft, now_millis())
            .map_err(storage_error)?;
        drafts.remove(session_id);
        drop(drafts);
        self.continuity
            .refresh(published.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(Some(published))
    }

    /// Keeps the messages before `keep_messages` under the same identifier and
    /// writes them, even when none are left.
    ///
    /// Reference `RewindManager.rewind_to_message` with `inplace`: the
    /// truncated history is saved with `allow_empty`, so a draft fork rewound
    /// in place is written here for the first time.
    ///
    /// # Errors
    ///
    /// Reports the storage failure.
    pub(crate) fn truncate_session(
        &self,
        session_id: &str,
        keep_messages: usize,
    ) -> Result<HydratedSession, WorkspaceServiceError> {
        let draft = self.lock_drafts()?.get(session_id).cloned();
        let hydrated = match draft {
            Some(mut draft) => {
                draft.messages.truncate(keep_messages);
                self.lock_drafts()?.insert(session_id.to_owned(), draft);
                self.publish_draft(session_id)?.ok_or_else(|| {
                    WorkspaceServiceError::Storage(format!(
                        "the rewound draft `{session_id}` disappeared before it was saved"
                    ))
                })?
            }
            None => {
                let statistics = self
                    .store
                    .load(session_id)
                    .map_err(storage_error)?
                    .metadata
                    .statistics;
                self.store
                    .rewind(session_id, keep_messages, statistics, now_millis())
                    .map_err(storage_error)?
            }
        };
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated)
    }

    /// Forgets a draft fork, which is what closing a session nobody typed into
    /// leaves of it upstream: nothing.
    pub(crate) fn discard_draft(&self, session_id: &str) {
        if let Ok(mut drafts) = self.lock_drafts()
            && drafts.remove(session_id).is_some()
        {
            let _ = self.continuity.remove(session_id);
        }
    }

    fn lock_drafts(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, HydratedSession>>, WorkspaceServiceError>
    {
        self.drafts
            .lock()
            .map_err(|_| WorkspaceServiceError::StatePoisoned)
    }

    pub fn update_runtime_settings(
        &self,
        session_id: &str,
        settings: &BTreeMap<String, Value>,
    ) -> Result<Option<HydratedSession>, WorkspaceServiceError> {
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
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(Some(hydrated))
    }

    pub fn create_runtime_session(
        &self,
        session_id: &str,
        working_directory: &str,
        now_ms: u64,
    ) -> Result<HydratedSession, WorkspaceServiceError> {
        match self.store.load(session_id) {
            Ok(_) => {
                return Err(WorkspaceServiceError::Storage(format!(
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
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated)
    }

    pub fn update_runtime_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<HydratedSession>, WorkspaceServiceError> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        self.set_session_agent(session_id, name)
            .map(|(_, hydrated)| Some(hydrated))
    }

    pub fn close_saved_session(
        &self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<(), WorkspaceServiceError> {
        self.discard_draft(session_id);
        match self.store.close(session_id, now_ms) {
            Ok(_) | Err(StorageError::SessionNotFound(_)) => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
    }

    pub(super) fn session_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", 50, SESSION_PAGE_MIN, SESSION_PAGE_MAX)?;
        let cwd = optional_string(params, "cwd")?;
        // The legacy migration still runs before the page is read, so a store
        // written by an older layout is listed; what it moved is not published,
        // because `SessionListResponse` declares the page and nothing else.
        self.store.migrate_legacy().map_err(storage_error)?;
        let page = self.store.list(cwd, offset, limit).map_err(storage_error)?;
        Ok(WorkspaceDispatch::result([(
            "sessions",
            serde_json::to_value(page.sessions)?,
        )]))
    }

    pub(super) fn history_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", 100, SESSION_PAGE_MIN, SESSION_PAGE_MAX)?;
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
                .history(session_id, offset, limit)
                .map_err(storage_error)?,
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
            None => self.store.load(session_id).map_err(storage_error)?,
        };
        Ok(hydrated_result(&hydrated, None))
    }

    /// Refuses a `worktree` on a method that reopens a recorded session.
    ///
    /// A saved session was recorded against a directory, and resolving a
    /// request here would mint a worktree and reopen it somewhere else. The
    /// reference refuses both methods for the same reason
    /// (`vibe/app_server/_worktree_session.py:176-189`).
    fn refuse_worktree_request(
        params: &BTreeMap<String, Value>,
    ) -> Result<(), WorkspaceServiceError> {
        match params.get("worktree") {
            None | Some(Value::Null) => Ok(()),
            Some(_) => Err(WorkspaceServiceError::InvalidParams(
                crate::worktrees::REOPEN_REFUSAL.to_owned(),
            )),
        }
    }

    pub(super) fn resume(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        Self::refuse_worktree_request(params)?;
        let hydrated = self
            .store
            .resume(
                required_string(params, "sessionId")?,
                optional_string(params, "systemPrompt")?.unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(|error| match error {
                // Reference `SavedSessions.resolve` (`vibe/core/session/saved_sessions.py`).
                StorageError::SessionNotFound(selector) => {
                    WorkspaceServiceError::NotFound(format!("Session not found: {selector}"))
                }
                other => storage_error(other),
            })?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn continue_session(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        Self::refuse_worktree_request(params)?;
        let cwd = match optional_string(params, "cwd")? {
            Some(cwd) => cwd.to_owned(),
            None => self.paths.working_directory.to_string_lossy().into_owned(),
        };
        let hydrated = self
            .store
            .continue_session(
                &cwd,
                optional_string(params, "systemPrompt")?.unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(|error| self.continuation_error(&cwd, error))?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    /// The refusal a continuation reports, told apart by where the launch stands.
    ///
    /// A directory under the managed worktree root has its own answer: it is a
    /// fresh worktree that has simply not been used yet, and saying so is what
    /// keeps the operator from reading an empty history as a lost one. The
    /// reference appends the same distinction to the same failure, testing the
    /// working directory against `WORKTREES_DIR`
    /// (`vibe/app_server/_runtime.py:640-648`). The sentence is this port's own.
    fn continuation_error(&self, cwd: &str, error: StorageError) -> WorkspaceServiceError {
        // Reference `_resolve_continue_session_id` names where it looked and
        // for which directory.
        let mapped = match error {
            StorageError::NoSessions => WorkspaceServiceError::NotFound(format!(
                "No previous sessions found in {} for cwd={cwd}",
                self.paths.session_root.display()
            )),
            other => storage_error(other),
        };
        let WorkspaceServiceError::NotFound(message) = &mapped else {
            return mapped;
        };
        let managed_root = vibe_core::worktree::managed_worktrees_root(&self.paths.vibe_home);
        let standing = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        if !standing.starts_with(&managed_root) {
            return mapped;
        }
        WorkspaceServiceError::NotFound(format!(
            "{message}. This worktree has no session of its own yet: start one here, or name an \
             existing session with --resume <ID>"
        ))
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
            None => format!(
                "session-{}-{}",
                now_millis(),
                self.next_session.fetch_add(1, Ordering::Relaxed)
            ),
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
        let metadata = self
            .store
            .update_title(
                required_string(params, "sessionId")?,
                required_string(params, "title")?,
                now_millis(),
            )
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
            Ok(()) | Err(StorageError::SessionNotFound(_)) => {}
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
            .load(required_string(params, "sessionId")?)
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
        let hydrated = self.store.load(&new_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    pub(super) fn set_session_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(AgentProfile, HydratedSession), WorkspaceServiceError> {
        let profile = self.agent_profile(name)?;
        let mut metadata = self.store.load(session_id).map_err(storage_error)?.metadata;
        metadata.agent_profile = Some(serde_json::to_value(&profile)?);
        self.store
            .update_metadata(&metadata)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| WorkspaceServiceError::Storage(error.to_string()))?;
        Ok((profile, hydrated))
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

/// The identity the reference gives a message that carries none of its own:
/// its position in the list and its role. Mirrors the fallback of
/// `history_message_id` (`vibe/app_server/_projection.py:830-831`).
pub(crate) fn history_entry_id(index: usize, role: &str) -> String {
    format!("history:{index}:{role}")
}

/// The position the reference numbers `index` by.
///
/// The reference's message list always opens with the system prompt, which a
/// stored transcript here leaves out, so a position read from one counts one
/// further. A list that does open with the prompt is already numbered alike.
pub(crate) fn reference_message_index(messages: &[ModelMessage], index: usize) -> usize {
    if matches!(messages.first(), Some(ModelMessage::System { .. })) {
        index
    } else {
        index.saturating_add(1)
    }
}

/// Which stored message `entry_id` names, among the ones a rewind may target.
///
/// Reference `history_user_message_index` (`vibe/app_server/_projection.py:834`):
/// only a user message the operator wrote is rewindable, and it is named by its
/// own identity, or by its position when it carries none.
pub(crate) fn rewind_entry_index(messages: &[ModelMessage], entry_id: &str) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .find(|(index, message)| match message {
            ModelMessage::User {
                message_id,
                injected: false,
                ..
            } => {
                message_id.clone().unwrap_or_else(|| {
                    history_entry_id(reference_message_index(messages, *index), "user")
                }) == entry_id
            }
            _ => false,
        })
        .map(|(index, _message)| index)
}

pub(super) fn hydrated_result(
    hydrated: &HydratedSession,
    attachment: Option<RuntimeAttachment>,
) -> WorkspaceDispatch {
    WorkspaceDispatch {
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
