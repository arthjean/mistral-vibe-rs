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
