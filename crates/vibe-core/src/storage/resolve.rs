//! Finding a saved session: by its identifier, by the short form a directory
//! is named with, and by what a `continue` from a working directory reopens.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use super::{
    HydratedSession, MESSAGES_FILE, SHORT_SESSION_ID_LENGTH, SessionMetadata, SessionStore,
    StorageError, is_internal_directory, short_session_id, validate_session_id,
};

impl SessionStore {
    /// The session a `continue` in `working_directory` reopens.
    ///
    /// Reference `_find_session_to_continue`: the terminal's last-session
    /// pointer when it names a loadable session reachable from here, and
    /// otherwise the loadable session with the most recently written log.
    pub fn continue_target(&self, working_directory: &str) -> Result<String, StorageError> {
        if let Some(pointer) = self.read_pointer()?
            && let Ok(metadata) = self.resolve(&pointer)
            && session_reaches(&metadata, working_directory)
            && self.read_messages(&metadata).is_ok()
        {
            return Ok(metadata.id);
        }
        let mut candidates = self
            .session_directories()?
            .into_iter()
            .filter_map(|directory| {
                let modified = fs::metadata(self.root.join(&directory).join(MESSAGES_FILE))
                    .and_then(|item| item.modified())
                    .ok()?;
                Some((modified, directory))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
        candidates
            .into_iter()
            .filter_map(|(_, directory)| self.read_metadata_from_directory(&directory).ok())
            .find(|metadata| {
                session_reaches(metadata, working_directory) && self.read_messages(metadata).is_ok()
            })
            .map(|metadata| metadata.id)
            .ok_or(StorageError::NoSessions)
    }

    pub fn continue_session(
        &self,
        working_directory: &str,
        current_system_prompt: impl Into<String>,
        current_config: BTreeMap<String, Value>,
    ) -> Result<HydratedSession, StorageError> {
        let _handoff_lock = self.acquire_handoff_lock(false)?;
        if let Some(journal_path) = self.handoff_journal_path() {
            self.recover_handoff_locked(&journal_path)?;
        }
        let target = self.continue_target(working_directory)?;
        self.resume(&target, current_system_prompt, current_config)
    }

    /// The directory a session is written under.
    pub fn session_directory(&self, selector: &str) -> Result<PathBuf, StorageError> {
        let metadata = self.resolve(selector)?;
        Ok(self.session_path(&metadata))
    }

    /// The session `selector` names for reopening it.
    ///
    /// Reference `resolve_legacy_session_reference` then `find_session_by_id`:
    /// the full identifier, or its first eight characters when exactly one
    /// session shortens to them; among directories carrying that short
    /// identifier, the one whose log was written last.
    pub(super) fn resolve(&self, selector: &str) -> Result<SessionMetadata, StorageError> {
        match self.resolve_exact(selector) {
            Ok(metadata) => return Ok(metadata),
            Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        if selector.chars().count() != SHORT_SESSION_ID_LENGTH {
            return Err(StorageError::SessionNotFound(selector.to_owned()));
        }
        let mut matches = self
            .candidate_directories(selector)?
            .into_iter()
            .filter_map(|directory| self.read_metadata_from_directory(&directory).ok())
            .filter(|metadata| short_session_id(&metadata.id) == selector)
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.id.cmp(&right.id));
        matches.dedup_by(|left, right| left.id == right.id);
        match matches.len() {
            0 => Err(StorageError::SessionNotFound(selector.to_owned())),
            1 => self.resolve_exact(&matches.remove(0).id),
            _ => Err(StorageError::AmbiguousSession(selector.to_owned())),
        }
    }

    /// The session recorded under exactly `session_id`: among the directories
    /// named by its short identifier, the most recently written one that
    /// records it. A session whose directory predates that naming is found by
    /// its metadata.
    pub(super) fn resolve_exact(&self, session_id: &str) -> Result<SessionMetadata, StorageError> {
        if validate_session_id(session_id).is_err() {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        let mut named = Vec::new();
        for directory in self.candidate_directories(session_id)? {
            match self.read_metadata_from_directory(&directory) {
                Ok(metadata) if metadata.id == session_id => {
                    let modified = fs::metadata(self.root.join(&directory).join(MESSAGES_FILE))
                        .and_then(|item| item.modified())
                        .ok();
                    named.push((modified, metadata));
                }
                _ => {}
            }
        }
        named.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
        if let Some((_, metadata)) = named.into_iter().next() {
            return Ok(metadata);
        }
        self.valid_metadata()?
            .into_iter()
            .find(|metadata| metadata.id == session_id)
            .ok_or_else(|| StorageError::SessionNotFound(session_id.to_owned()))
    }

    /// The directories named after `selector`'s short identifier.
    fn candidate_directories(&self, selector: &str) -> Result<Vec<String>, StorageError> {
        let suffix = format!("_{}", short_session_id(selector));
        Ok(self
            .session_directories()?
            .into_iter()
            .filter(|directory| directory.ends_with(&suffix))
            .collect())
    }

    /// Every directory under the root named with this store's prefix.
    pub(super) fn session_directories(&self) -> Result<Vec<String>, StorageError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StorageError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };
        let prefix = format!("{}_", self.prefix());
        let mut directories = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StorageError::Io {
                path: self.root.clone(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && entry.path().is_dir() {
                directories.push(name);
            }
        }
        directories.sort();
        Ok(directories)
    }

    pub(super) fn valid_metadata(&self) -> Result<Vec<SessionMetadata>, StorageError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StorageError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };
        let mut metadata = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StorageError::Io {
                path: self.root.clone(),
                source,
            })?;
            if !entry
                .file_type()
                .map_err(|source| StorageError::Io {
                    path: entry.path(),
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let directory = entry.file_name().to_string_lossy().into_owned();
            if is_internal_directory(&directory) {
                continue;
            }
            if let Ok(item) = self.read_metadata_from_directory(&directory) {
                metadata.push(item);
            }
        }
        Ok(metadata)
    }
}

/// Reference `SessionLoader._session_reaches`: a session is offered from the
/// directory it began in and from the one it now works in.
fn session_reaches(metadata: &SessionMetadata, working_directory: &str) -> bool {
    [
        metadata.origin_directory.as_deref(),
        Some(metadata.working_directory.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|stored| !stored.is_empty())
    .any(|stored| same_working_directory(stored, working_directory))
}

fn same_working_directory(stored: &str, current: &str) -> bool {
    if stored == current {
        return true;
    }
    let stored = crate::platform::canonicalize(stored);
    let current = crate::platform::canonicalize(current);
    matches!((stored, current), (Ok(stored), Ok(current)) if stored == current)
}
