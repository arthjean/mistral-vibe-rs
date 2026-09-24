//! Moving a conversation onto a new identifier without losing it.
//!
//! A compaction or a clearing rotates the session, which means a new directory
//! has to be complete before the pointer names it and the old one is only
//! detached once it does. A process that dies between those two steps would
//! otherwise leave the pointer naming a session that does not exist yet, or two
//! sessions both claiming to be current.
//!
//! The journal is what closes that window: the plan is written before anything
//! moves, and a journal found at startup is rolled forward from wherever it
//! stopped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::fs;
use std::sync::atomic::Ordering;

use super::{
    FileLock, HANDOFF_JOURNAL_PREFIX, HANDOFF_LOCK_PREFIX, HandoffJournal, HandoffPlan,
    HydratedSession, LAST_SESSION_DIRECTORY, SessionMetadata, SessionStore, StorageError,
    TEMP_SEQUENCE, ensure_private_directory, is_safe_handoff_component, session_directory_name,
    sync_directory, validate_session_id,
};
use crate::atomic_file::write_atomically;
use crate::events::ModelMessage;

impl SessionStore {
    /// Publishes `messages` under `new_id`, continuing `parent`.
    ///
    /// `retain_parent` is the reference's `keep_parent`: a compaction records
    /// the session it came from, so the two read as one conversation, and a
    /// clearing records nothing, because what it continues was discarded
    /// (`vibe/core/agent_loop/_loop.py:2665`). Everything else the new session
    /// inherits is the same either way.
    pub fn handoff_messages(
        &self,
        parent: &SessionMetadata,
        new_id: &str,
        messages: &[ModelMessage],
        now_ms: u64,
        retain_parent: bool,
    ) -> Result<SessionMetadata, StorageError> {
        self.publish_handoff(
            parent,
            new_id,
            messages.to_vec(),
            HandoffPlan {
                current_config: parent.config.clone(),
                config_overlay: BTreeMap::new(),
                retain_parent,
            },
            now_ms,
        )
        .map(|hydrated| hydrated.metadata)
    }

    /// The session a rewind forks onto, composed without writing anything.
    ///
    /// Reference `RewindManager.rewind_to_message` resets the session to a new
    /// identifier and leaves it unpersisted until its next save
    /// (`vibe/core/session/session_logger.py:735-748`), so a fork nobody types
    /// into never reaches disk. The draft carries everything
    /// [`SessionStore::publish_draft`] needs to write it later: the kept
    /// messages, the parent it continues, and what a handoff inherits.
    #[must_use]
    pub fn draft_handoff(
        &self,
        parent: &HydratedSession,
        new_id: &str,
        keep_messages: usize,
        statistics: BTreeMap<String, serde_json::Value>,
        now_ms: u64,
    ) -> HydratedSession {
        let mut messages = parent.messages.clone();
        messages.truncate(keep_messages);
        let mut metadata = parent.metadata.clone();
        new_id.clone_into(&mut metadata.id);
        metadata.directory = String::new();
        metadata.start_time = super::format_iso_timestamp(now_ms);
        metadata.end_time = None;
        metadata.child_sessions = Vec::new();
        metadata.loops = Vec::new();
        metadata.title = None;
        metadata.title_source = super::default_title_source();
        metadata.message_count = 0;
        metadata.last_message_fingerprint = None;
        metadata.statistics = statistics;
        metadata.created_at_ms = now_ms;
        metadata.updated_at_ms = now_ms;
        metadata.parent_session_id = Some(parent.metadata.id.clone());
        HydratedSession {
            metadata,
            messages,
            current_config: parent.current_config.clone(),
        }
    }

    /// Writes a draft [`SessionStore::draft_handoff`] composed, under its own
    /// identifier and continuing the parent it names.
    ///
    /// # Errors
    ///
    /// Fails as a handoff fails: the draft's identifier is invalid, or the
    /// directory, the messages or the pointer cannot be written.
    pub fn publish_draft(
        &self,
        draft: &HydratedSession,
        now_ms: u64,
    ) -> Result<HydratedSession, StorageError> {
        // A handoff copies what the new session inherits from the one it
        // continues; the draft already holds those values, so it stands in for
        // its parent under the parent's identifier.
        let mut template = draft.metadata.clone();
        template.id = draft.metadata.parent_session_id.clone().unwrap_or_default();
        self.publish_handoff(
            &template,
            &draft.metadata.id,
            draft.messages.clone(),
            HandoffPlan {
                current_config: draft.current_config.clone(),
                config_overlay: BTreeMap::new(),
                retain_parent: draft.metadata.parent_session_id.is_some(),
            },
            now_ms,
        )
    }

    pub(super) fn publish_handoff(
        &self,
        parent: &SessionMetadata,
        new_id: &str,
        messages: Vec<ModelMessage>,
        plan: HandoffPlan,
        now_ms: u64,
    ) -> Result<HydratedSession, StorageError> {
        let HandoffPlan {
            current_config,
            config_overlay,
            retain_parent,
        } = plan;
        validate_session_id(new_id)?;
        ensure_private_directory(&self.root)?;
        let _handoff_lock = self.acquire_handoff_lock(true)?;
        let journal_path = self.handoff_journal_path();
        if let Some(journal_path) = &journal_path
            && self
                .recover_handoff_locked(journal_path)?
                .as_deref()
                .is_some_and(|recovered_id| recovered_id == new_id)
        {
            let mut recovered = self.load(new_id)?;
            recovered.current_config = current_config;
            return Ok(recovered);
        }
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staging_directory = format!(".handoff-{sequence}-{new_id}");
        let staging_path = self.root.join(&staging_directory);
        let destination_directory = session_directory_name(now_ms, new_id);
        let destination = self.root.join(&destination_directory);
        let mut journal_written = false;
        let result = (|| {
            let mut child = self.initialize_session(
                &staging_directory,
                new_id,
                &parent.working_directory,
                retain_parent.then(|| parent.id.clone()),
                now_ms,
            )?;
            child.statistics = parent.statistics.clone();
            child.experiment_state = parent.experiment_state.clone();
            child.config = current_config.clone();
            child.config.extend(config_overlay);
            child.agent_profile = parent.agent_profile.clone();
            child.tools_available = parent.tools_available.clone();
            self.replace_messages(&mut child, &messages, now_ms)?;
            sync_directory(&self.root)?;
            if let Some(journal_path) = &journal_path {
                self.write_handoff_journal(
                    journal_path,
                    &HandoffJournal {
                        session_id: new_id.to_owned(),
                        staging_directory: staging_directory.clone(),
                        destination_directory: destination_directory.clone(),
                    },
                )?;
                journal_written = true;
            }
            fs::rename(&staging_path, &destination).map_err(|source| StorageError::Io {
                path: destination.clone(),
                source,
            })?;
            sync_directory(&self.root)?;
            child.directory.clone_from(&destination_directory);
            self.write_pointer(new_id)?;
            if let Some(journal_path) = &journal_path {
                self.remove_handoff_journal(journal_path)?;
            }
            Ok(HydratedSession {
                metadata: child,
                messages,
                current_config,
            })
        })();
        if result.is_err() && !journal_written && staging_path.exists() {
            let _ = fs::remove_dir_all(staging_path);
        }
        result
    }

    pub(super) fn acquire_handoff_lock(
        &self,
        create_root: bool,
    ) -> Result<Option<FileLock>, StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(None);
        };
        if !create_root && !self.root.exists() {
            return Ok(None);
        }
        let pointer_directory = self.root.join(LAST_SESSION_DIRECTORY);
        ensure_private_directory(&pointer_directory)?;
        FileLock::acquire(&pointer_directory.join(format!("{HANDOFF_LOCK_PREFIX}{pointer_key}")))
            .map(Some)
    }

    pub(super) fn handoff_journal_path(&self) -> Option<PathBuf> {
        self.pointer_key.as_ref().map(|pointer_key| {
            self.root
                .join(LAST_SESSION_DIRECTORY)
                .join(format!("{HANDOFF_JOURNAL_PREFIX}{pointer_key}.json"))
        })
    }

    pub(super) fn write_handoff_journal(
        &self,
        path: &Path,
        journal: &HandoffJournal,
    ) -> Result<(), StorageError> {
        let mut encoded = serde_json::to_vec_pretty(journal).map_err(StorageError::Json)?;
        encoded.push(b'\n');
        write_atomically(path, "handoff-journal", &encoded).map_err(StorageError::from)
    }

    pub(super) fn recover_handoff_locked(
        &self,
        path: &Path,
    ) -> Result<Option<String>, StorageError> {
        let encoded = match fs::read(path) {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StorageError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let journal: HandoffJournal = serde_json::from_slice(&encoded)
            .map_err(|_| StorageError::InvalidHandoffJournal(path.to_path_buf()))?;
        validate_session_id(&journal.session_id)?;
        if !is_safe_handoff_component(&journal.staging_directory, ".handoff-")
            || !is_safe_handoff_component(&journal.destination_directory, "session_")
        {
            return Err(StorageError::InvalidHandoffJournal(path.to_path_buf()));
        }
        let staging_path = self.root.join(&journal.staging_directory);
        let destination = self.root.join(&journal.destination_directory);
        match (staging_path.exists(), destination.exists()) {
            (true, false) => {
                self.validate_handoff_directory(&journal.staging_directory, &journal.session_id)?;
                fs::rename(&staging_path, &destination).map_err(|source| StorageError::Io {
                    path: destination.clone(),
                    source,
                })?;
                sync_directory(&self.root)?;
            }
            (false, true) => {}
            _ => return Err(StorageError::InvalidHandoffJournal(path.to_path_buf())),
        }
        self.validate_handoff_directory(&journal.destination_directory, &journal.session_id)?;
        self.write_pointer(&journal.session_id)?;
        self.remove_handoff_journal(path)?;
        Ok(Some(journal.session_id))
    }

    pub(super) fn validate_handoff_directory(
        &self,
        directory: &str,
        session_id: &str,
    ) -> Result<(), StorageError> {
        let metadata = self.read_metadata_from_directory(directory)?;
        if metadata.id != session_id {
            return Err(StorageError::InvalidHandoffJournal(
                self.handoff_journal_path()
                    .unwrap_or_else(|| self.root.clone()),
            ));
        }
        self.read_messages(&metadata)?;
        Ok(())
    }

    pub(super) fn remove_handoff_journal(&self, path: &Path) -> Result<(), StorageError> {
        match fs::remove_file(path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StorageError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}
