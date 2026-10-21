//! Saved sessions: one directory per session under the configured save
//! directory, holding its metadata and an append-only message log.
//!
//! The layout is the reference's (`vibe/core/session/`), so a session either
//! implementation wrote is one the other lists, resumes and appends to:
//!
//! - a session lives in `<save_dir>/<prefix>_<YYYYmmdd_HHMMSS>_<short id>`,
//!   the short identifier being the first eight characters of the session's,
//!   which is also how a lookup by identifier finds it
//!   (`session_logger.py` `save_folder`, `session_loader.py`
//!   `_find_session_dirs_by_short_id`);
//! - `meta.json` carries the reference's `SessionMetadata` keys in its order,
//!   and every key this port does not model is kept on rewrite;
//! - nothing is written until the session holds a message, so a session that
//!   was opened and never used leaves nothing behind (`save_interaction`);
//! - `.session_index.json` caches the listing, `.last_session/<tty>` names the
//!   session a terminal last used, and `active/<id>.lock` is held by the
//!   process that has the session open ([`lease`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::Value;

use crate::atomic_file;
use crate::events::ModelMessage;

mod error;
mod handoff;
pub mod index;
pub mod lease;
mod log;
mod metadata;
mod migration;
pub mod permissions;
mod pointer;
mod record;
mod registry;
mod resolve;
pub(crate) mod time;

pub use error::StorageError;
pub use index::SessionInfo;
pub use metadata::{SessionMetadata, relocate_metadata};
use pointer::{current_tty_key, sanitize_pointer_key};
pub use registry::register_session_prefix;
use time::format_compact_timestamp;

pub(super) const METADATA_FILE: &str = "meta.json";
pub(super) const MESSAGES_FILE: &str = "messages.jsonl";
pub(super) const LAST_SESSION_DIRECTORY: &str = ".last_session";
/// The directory under a session's own that its subagents' sessions live in.
pub const CHILD_SESSIONS_DIRECTORY: &str = "agents";
pub(super) const HANDOFF_JOURNAL_PREFIX: &str = ".handoff-transaction-";
pub(super) const HANDOFF_LOCK_PREFIX: &str = ".handoff-lock-";
pub(super) const MIGRATION_LOCK_FILE: &str = ".migration.lock";
pub(super) const MIGRATION_SUFFIX: &str = ".legacy.bak";
pub(super) const MAX_MESSAGE_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub(super) static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Reference `SessionLoggingConfig.session_prefix`'s default.
pub const DEFAULT_SESSION_PREFIX: &str = "session";
/// Reference `vibe/utils/session_id.py` `_SHORT_LEN`: how much of an
/// identifier names its directory, and what a short selector spells.
pub const SHORT_SESSION_ID_LENGTH: usize = 8;

/// Reference `SessionLoggingConfig`, read from an effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLogging {
    pub enabled: bool,
    pub save_dir: PathBuf,
    pub session_prefix: String,
    pub generate_titles: bool,
}

impl SessionLogging {
    /// The `session_logging` table of `effective`, with the reference's
    /// defaults for what it leaves out. An unset directory is the vibe home's
    /// session log directory, which is where the configuration loader resolves
    /// it too.
    #[must_use]
    pub fn from_effective(effective: &toml::Table, vibe_home: &Path) -> Self {
        let table = effective
            .get("session_logging")
            .and_then(toml::Value::as_table);
        let text = |key: &str| {
            table
                .and_then(|table| table.get(key))
                .and_then(toml::Value::as_str)
                .filter(|value| !value.is_empty())
        };
        let flag = |key: &str, default: bool| {
            table
                .and_then(|table| table.get(key))
                .and_then(toml::Value::as_bool)
                .unwrap_or(default)
        };
        Self {
            enabled: flag("enabled", true),
            save_dir: text("save_dir").map_or_else(|| default_save_dir(vibe_home), PathBuf::from),
            session_prefix: text("session_prefix")
                .unwrap_or(DEFAULT_SESSION_PREFIX)
                .to_owned(),
            generate_titles: flag("generate_titles", true),
        }
    }

    /// The defaults, under `vibe_home`.
    #[must_use]
    pub fn defaults(vibe_home: &Path) -> Self {
        Self::from_effective(&toml::Table::new(), vibe_home)
    }

    /// A store over the save directory, named with the configured prefix,
    /// which it also registers for every other store opened over it.
    #[must_use]
    pub fn store(&self) -> SessionStore {
        register_session_prefix(&self.save_dir, &self.session_prefix);
        SessionStore::new(&self.save_dir).with_prefix(&self.session_prefix)
    }
}

/// Reference `SESSION_LOG_DIR`.
#[must_use]
pub fn default_save_dir(vibe_home: &Path) -> PathBuf {
    vibe_home.join("logs").join("session")
}

/// Reference `shorten_session_id`.
#[must_use]
pub fn short_session_id(session_id: &str) -> &str {
    session_id
        .char_indices()
        .nth(SHORT_SESSION_ID_LENGTH)
        .map_or(session_id, |(end, _)| &session_id[..end])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydratedSession {
    pub metadata: SessionMetadata,
    pub messages: Vec<ModelMessage>,
    pub current_config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    pub migrated: usize,
    pub skipped: usize,
    pub issues: Vec<MigrationIssue>,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    pointer_key: Option<String>,
    /// The prefix this store was given, or [`None`] to follow the one
    /// registered for its directory whenever it names or lists a session, so
    /// a store opened before the configuration was resolved agrees with every
    /// store opened after.
    prefix: Option<String>,
}

impl SessionStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            pointer_key: current_tty_key(),
            prefix: None,
        }
    }

    #[must_use]
    pub fn with_pointer_key(mut self, pointer_key: impl Into<String>) -> Self {
        self.pointer_key = Some(sanitize_pointer_key(&pointer_key.into()));
        self
    }

    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// The save directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The prefix a session directory is named with.
    #[must_use]
    pub fn prefix(&self) -> String {
        self.prefix
            .clone()
            .unwrap_or_else(|| registry::prefix(&self.root))
    }

    /// Names a new session without writing anything.
    ///
    /// The session reaches disk with its first message ([`Self::append_messages`]),
    /// which is when the reference's `save_interaction` first writes one.
    pub fn create(
        &self,
        id: &str,
        working_directory: &str,
        parent_session_id: Option<String>,
        now_ms: u64,
    ) -> Result<SessionMetadata, StorageError> {
        validate_session_id(id)?;
        let metadata = self.compose_session(id, working_directory, parent_session_id, now_ms);
        registry::remember_pending(&self.root, &metadata);
        Ok(metadata)
    }

    /// Forgets a session that was named and never written.
    pub fn discard_pending(&self, session_id: &str) {
        registry::discard_pending(&self.root, session_id);
    }

    /// The session recorded under exactly `session_id`, written or not: what a
    /// process holding the session open reads, where [`Self::load`] reads only
    /// what a later process could reopen.
    pub fn open(&self, session_id: &str) -> Result<HydratedSession, StorageError> {
        match self.load(session_id) {
            Ok(hydrated) if hydrated.metadata.id == session_id => return Ok(hydrated),
            Ok(_) | Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let metadata = registry::pending(&self.root, session_id)
            .ok_or_else(|| StorageError::SessionNotFound(session_id.to_owned()))?;
        Ok(HydratedSession {
            metadata,
            messages: Vec::new(),
            current_config: BTreeMap::new(),
        })
    }

    pub fn create_child(
        &self,
        id: &str,
        working_directory: &str,
        parent_session_id: String,
        now_ms: u64,
    ) -> Result<SessionMetadata, StorageError> {
        validate_session_id(id)?;
        if self
            .valid_metadata()?
            .iter()
            .any(|metadata| metadata.id == id)
        {
            return Err(StorageError::DuplicateSessionId(id.to_owned()));
        }
        Ok(self.compose_session(id, working_directory, Some(parent_session_id), now_ms))
    }

    /// The store a subagent of `parent` saves its session in: the parent's own
    /// `agents` directory, each child named after the agent it runs.
    /// Reference `create_child`, which roots the child's logger at
    /// `<parent session>/agents` under the agent's name as its prefix.
    #[must_use]
    pub fn child_store(&self, parent: &SessionMetadata, agent: &str) -> Self {
        Self {
            root: self.session_path(parent).join(CHILD_SESSIONS_DIRECTORY),
            pointer_key: None,
            prefix: Some(agent.to_owned()),
        }
    }

    /// The directory a session created at `now_ms` is written to.
    ///
    /// Two sessions sharing a short identifier and a second would share a
    /// name, so the stamp moves forward a second until the name is free: the
    /// name keeps the layout a lookup by short identifier reads.
    fn free_directory_name(&self, now_ms: u64, id: &str) -> String {
        let prefix = self.prefix();
        let mut stamp = now_ms;
        loop {
            let name = session_directory_name(&prefix, stamp, id);
            if !self.root.join(&name).exists() {
                return name;
            }
            stamp = stamp.saturating_add(1_000);
        }
    }

    pub fn load(&self, selector: &str) -> Result<HydratedSession, StorageError> {
        let mut metadata = self.resolve(selector)?;
        let messages = self.read_messages(&metadata)?;
        metadata.message_count = u64::try_from(messages.len()).unwrap_or(u64::MAX);
        Ok(HydratedSession {
            metadata,
            messages,
            current_config: BTreeMap::new(),
        })
    }

    pub fn resume(
        &self,
        selector: &str,
        current_system_prompt: impl Into<String>,
        current_config: BTreeMap<String, Value>,
    ) -> Result<HydratedSession, StorageError> {
        let mut hydrated = self.load(selector)?;
        hydrated
            .messages
            .retain(|message| !matches!(message, ModelMessage::System { .. }));
        hydrated.messages.insert(
            0,
            ModelMessage::System {
                content: current_system_prompt.into(),
            },
        );
        hydrated.current_config = current_config;
        Ok(hydrated)
    }

    /// Every listed session, most recently updated first, filtered to those
    /// begun in or moved to `cwd` when one is given.
    ///
    /// Reference `SessionLoader.list_sessions` over the listing index.
    pub fn sessions(&self, cwd: Option<&str>) -> Result<Vec<SessionInfo>, StorageError> {
        index::list(self, cwd)
    }

    /// Reference `SessionLoader.get_first_user_message`: the label an untitled
    /// session is shown by.
    #[must_use]
    pub fn first_user_message(&self, session_id: &str) -> String {
        index::first_user_message(self, session_id)
    }

    /// Reference `delete_saved_session`: removes the session this store holds
    /// under exactly `session_id`, and forgets it in every terminal's pointer.
    /// Answers whether one was there.
    pub fn delete(&self, session_id: &str) -> Result<bool, StorageError> {
        let metadata = match self.resolve_exact(session_id) {
            Ok(metadata) => metadata,
            Err(StorageError::SessionNotFound(_)) => {
                self.clear_pointer_if_matches(session_id)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let session_path = self.session_path(&metadata);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tombstone = self
            .root
            .join(format!(".deleting-{sequence}-{}", metadata.id));
        fs::rename(&session_path, &tombstone).map_err(|source| StorageError::Io {
            path: session_path.clone(),
            source,
        })?;
        sync_directory(&self.root)?;
        fs::remove_dir_all(&tombstone).map_err(|source| StorageError::InterruptedDelete {
            session_id: metadata.id.clone(),
            tombstone,
            source,
        })?;
        self.clear_pointer_if_matches(&metadata.id)?;
        sync_directory(&self.root)?;
        Ok(true)
    }

    pub fn recover_interrupted_deletes(&self) -> Result<usize, StorageError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(source) => {
                return Err(StorageError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };
        let mut recovered = 0_usize;
        for entry in entries {
            let entry = entry.map_err(|source| StorageError::Io {
                path: self.root.clone(),
                source,
            })?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".deleting-")
            {
                fs::remove_dir_all(entry.path()).map_err(|source| StorageError::Io {
                    path: entry.path(),
                    source,
                })?;
                recovered = recovered.saturating_add(1);
            }
        }
        if recovered > 0 {
            sync_directory(&self.root)?;
        }
        Ok(recovered)
    }

    /// The directory `metadata` is, or will be, written under.
    #[must_use]
    pub fn session_path(&self, metadata: &SessionMetadata) -> PathBuf {
        self.root.join(&metadata.directory)
    }
}

/// The advisory lock this store serializes its durable writes behind, with the
/// store's own error vocabulary in front of [`atomic_file::FileLock`].
pub(super) struct FileLock(
    /// Held for its `Drop`, which is what releases the lock.
    #[expect(dead_code, reason = "the guard's whole job is its drop")]
    atomic_file::FileLock,
);

impl FileLock {
    /// Blocks until the lock is available.
    fn acquire(path: &Path) -> Result<Self, StorageError> {
        atomic_file::FileLock::acquire(path)
            .map(Self)
            .map_err(|source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Answers `busy` rather than waiting when another holder owns the lock.
    fn try_acquire(path: &Path, busy: StorageError) -> Result<Self, StorageError> {
        atomic_file::FileLock::try_acquire(path)
            .map(Self)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::WouldBlock {
                    busy
                } else {
                    StorageError::Io {
                        path: path.to_path_buf(),
                        source,
                    }
                }
            })
    }
}

pub(super) fn ensure_private_directory(path: &Path) -> Result<(), StorageError> {
    atomic_file::ensure_private_directory(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn create_private_directory(path: &Path) -> Result<(), StorageError> {
    atomic_file::create_private_directory(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn sync_directory(path: &Path) -> Result<(), StorageError> {
    atomic_file::sync_directory(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn validate_session_id(id: &str) -> Result<(), StorageError> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(StorageError::InvalidSessionId(id.to_owned()))
    }
}

pub(super) fn is_internal_directory(directory: &str) -> bool {
    directory.starts_with('.') || directory == lease::ACTIVE_DIRECTORY
}

pub(super) fn is_safe_handoff_component(component: &str, prefix: &str) -> bool {
    component.starts_with(prefix)
        && !component.contains('/')
        && !component.contains('\\')
        && component != "."
        && component != ".."
}

/// Reference `SessionLogger.save_folder`.
pub(super) fn session_directory_name(prefix: &str, now_ms: u64, id: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        format_compact_timestamp(now_ms),
        short_session_id(id)
    )
}

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod lease_tests;
#[cfg(test)]
mod permissions_tests;
#[cfg(test)]
mod storage_tests;
