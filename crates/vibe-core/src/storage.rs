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
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::atomic_file::{self, AtomicWriteError, create_private_file, write_atomically};
use crate::events::ModelMessage;
use crate::text::hex_encode;

mod canonical_json;
mod handoff;
pub mod index;
pub mod lease;
mod migration;
pub mod permissions;
pub(crate) mod time;

use canonical_json::python_canonical_json;
pub use index::SessionInfo;
use time::{format_compact_timestamp, format_iso_timestamp};
pub use time::{normalize_iso_utc, parse_iso_millis};

pub(super) const METADATA_FILE: &str = "meta.json";
pub(super) const MESSAGES_FILE: &str = "messages.jsonl";
pub(super) const LAST_SESSION_DIRECTORY: &str = ".last_session";
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

/// The prefix each save directory was configured with.
///
/// A store is opened from a directory in many places, most of which never see
/// the configuration; the prefix is registered once where the configuration is
/// resolved, the way the reference keys its listing index by save directory
/// (`session_index.py` `session_index_for`).
fn prefixes() -> &'static Mutex<BTreeMap<PathBuf, String>> {
    static PREFIXES: OnceLock<Mutex<BTreeMap<PathBuf, String>>> = OnceLock::new();
    PREFIXES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Records the `session_prefix` sessions under `root` are named with.
pub fn register_session_prefix(root: &Path, prefix: &str) {
    if let Ok(mut prefixes) = prefixes().lock() {
        if prefix == DEFAULT_SESSION_PREFIX {
            prefixes.remove(root);
        } else {
            prefixes.insert(root.to_path_buf(), prefix.to_owned());
        }
    }
}

fn registered_prefix(root: &Path) -> String {
    prefixes()
        .lock()
        .ok()
        .and_then(|prefixes| prefixes.get(root).cloned())
        .unwrap_or_else(|| DEFAULT_SESSION_PREFIX.to_owned())
}

/// Sessions named but not yet written, keyed by save directory and identifier.
///
/// The reference keeps a new session's metadata on its logger until the first
/// save; here the app server that opens a session and the driver that runs its
/// turns hold separate stores, so what one records before the first save has to
/// be where the other finds it.
fn pending() -> &'static Mutex<BTreeMap<(PathBuf, String), SessionMetadata>> {
    static PENDING: OnceLock<Mutex<BTreeMap<(PathBuf, String), SessionMetadata>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(BTreeMap::new()))
}

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
            generate_titles: flag("generate_titles", false),
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
        SessionStore::new(&self.save_dir)
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

/// Reference `SessionMetadata` (`vibe/core/types.py`), in its field order, plus
/// the keys a full save appends and every key neither side models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(rename = "session_id")]
    pub id: String,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    pub start_time: String,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub git_commit: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub environment: BTreeMap<String, Option<String>>,
    /// Where the session began. `environment.working_directory` follows the
    /// session when it moves; this stays, so the listing still offers it from
    /// where the user started.
    #[serde(default)]
    pub origin_directory: Option<String>,
    #[serde(default = "unknown_username")]
    pub username: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub child_sessions: Vec<Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub loops: Vec<Value>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_title_source")]
    pub title_source: String,
    /// The latest accepted user interaction, as a UTC instant.
    #[serde(default)]
    pub bumped_at: Option<String>,
    /// When the session was pinned, or `None` while it is not.
    #[serde(default)]
    pub pinned_at: Option<String>,
    #[serde(default, rename = "experiments")]
    pub experiment_state: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    pub config: BTreeMap<String, Value>,
    #[serde(default)]
    pub import_provenance: Option<Value>,
    #[serde(default)]
    pub created_worktree: Option<Value>,
    #[serde(default, rename = "stats", deserialize_with = "null_as_default")]
    pub statistics: BTreeMap<String, Value>,
    #[serde(
        default,
        rename = "total_messages",
        deserialize_with = "null_as_default"
    )]
    pub message_count: u64,
    #[serde(default)]
    pub last_message_fingerprint: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub tools_available: Vec<Value>,
    #[serde(default)]
    pub agent_profile: Option<Value>,
    #[serde(default)]
    pub system_prompt: Option<Value>,
    /// Keys neither implementation's model names, kept so a rewrite here never
    /// drops what a newer writer added.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
    #[serde(skip)]
    pub directory: String,
    #[serde(skip)]
    pub created_at_ms: u64,
    #[serde(skip)]
    pub updated_at_ms: u64,
    #[serde(skip)]
    pub working_directory: String,
}

impl SessionMetadata {
    /// Whether this session has been written to disk.
    #[must_use]
    pub fn is_persisted(&self, store: &SessionStore) -> bool {
        store.session_path(self).join(METADATA_FILE).is_file()
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HandoffJournal {
    session_id: String,
    staging_directory: String,
    destination_directory: String,
}

/// What a handoff writes onto the session it publishes, beyond the messages.
pub(super) struct HandoffPlan {
    current_config: BTreeMap<String, Value>,
    config_overlay: BTreeMap<String, Value>,
    /// The reference's `keep_parent`: whether the session being replaced is
    /// recorded as the parent of the one being published.
    retain_parent: bool,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    pointer_key: Option<String>,
    prefix: String,
}

impl SessionStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let prefix = registered_prefix(&root);
        Self {
            root,
            pointer_key: current_tty_key(),
            prefix,
        }
    }

    #[must_use]
    pub fn with_pointer_key(mut self, pointer_key: impl Into<String>) -> Self {
        self.pointer_key = Some(sanitize_pointer_key(&pointer_key.into()));
        self
    }

    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The save directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The prefix a session directory is named with.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
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
        self.remember_pending(&metadata);
        Ok(metadata)
    }

    fn remember_pending(&self, metadata: &SessionMetadata) {
        if let Ok(mut pending) = pending().lock() {
            pending.insert((self.root.clone(), metadata.id.clone()), metadata.clone());
        }
    }

    /// Forgets a session that was named and never written.
    pub fn discard_pending(&self, session_id: &str) {
        if let Ok(mut pending) = pending().lock() {
            pending.remove(&(self.root.clone(), session_id.to_owned()));
        }
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
        let metadata = pending()
            .lock()
            .ok()
            .and_then(|pending| {
                pending
                    .get(&(self.root.clone(), session_id.to_owned()))
                    .cloned()
            })
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

    /// Reference `SessionLogger._initialize_session_metadata`: what a new
    /// session records before anything happens in it.
    fn compose_session(
        &self,
        id: &str,
        working_directory: &str,
        parent_session_id: Option<String>,
        now_ms: u64,
    ) -> SessionMetadata {
        let mut environment = BTreeMap::new();
        environment.insert(
            "working_directory".to_owned(),
            Some(working_directory.to_owned()),
        );
        let (git_commit, git_branch) = git_metadata(Path::new(working_directory));
        SessionMetadata {
            id: id.to_owned(),
            parent_session_id,
            start_time: format_iso_timestamp(now_ms),
            end_time: None,
            git_commit,
            git_branch,
            environment,
            origin_directory: Some(working_directory.to_owned()),
            username: current_username(),
            child_sessions: Vec::new(),
            loops: Vec::new(),
            title: None,
            title_source: default_title_source(),
            bumped_at: None,
            pinned_at: None,
            experiment_state: Value::Null,
            config: BTreeMap::new(),
            import_provenance: None,
            created_worktree: None,
            statistics: BTreeMap::new(),
            message_count: 0,
            last_message_fingerprint: None,
            tools_available: Vec::new(),
            agent_profile: None,
            system_prompt: None,
            extra: serde_json::Map::new(),
            directory: self.free_directory_name(now_ms, id),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            working_directory: working_directory.to_owned(),
        }
    }

    /// The directory a session created at `now_ms` is written to.
    ///
    /// Two sessions sharing a short identifier and a second would share a
    /// name, so the stamp moves forward a second until the name is free: the
    /// name keeps the layout a lookup by short identifier reads.
    fn free_directory_name(&self, now_ms: u64, id: &str) -> String {
        let mut stamp = now_ms;
        loop {
            let name = session_directory_name(&self.prefix, stamp, id);
            if !self.root.join(&name).exists() {
                return name;
            }
            stamp = stamp.saturating_add(1_000);
        }
    }

    /// Creates the session's directory and its empty log, once.
    fn materialize(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        let session_path = self.session_path(metadata);
        if session_path.join(MESSAGES_FILE).is_file() {
            return Ok(());
        }
        ensure_private_directory(&self.root)?;
        if !session_path.is_dir() {
            create_private_directory(&session_path)?;
        }
        match create_private_file(&session_path.join(MESSAGES_FILE)) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(source) => Err(StorageError::Io {
                path: session_path.join(MESSAGES_FILE),
                source,
            }),
        }
    }

    pub fn append_message(
        &self,
        metadata: &mut SessionMetadata,
        message: &ModelMessage,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        self.append_messages(metadata, std::slice::from_ref(message), now_ms)
    }

    /// Appends `messages` to the log, then writes metadata once.
    ///
    /// A system message never reaches the log: it is replaced wholesale in the
    /// metadata, because only the current one is ever replayed. A session with
    /// nothing but a system message is not written at all.
    pub fn append_messages(
        &self,
        metadata: &mut SessionMetadata,
        messages: &[ModelMessage],
        now_ms: u64,
    ) -> Result<(), StorageError> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::new();
        let mut appended = 0_u64;
        let mut last = None;
        for message in messages {
            if matches!(message, ModelMessage::System { .. }) {
                metadata.system_prompt = Some(system_prompt_record(message)?);
                continue;
            }
            encoded.extend(encode_message(message)?);
            encoded.push(b'\n');
            appended = appended.saturating_add(1);
            last = Some(message);
        }
        if encoded.is_empty() && !metadata.is_persisted(self) {
            return Ok(());
        }
        self.materialize(metadata)?;
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        if !encoded.is_empty() {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|source| StorageError::Io {
                    path: path.clone(),
                    source,
                })?;
            file.write_all(&encoded)
                .and_then(|()| file.sync_data())
                .map_err(|source| StorageError::Io {
                    path: path.clone(),
                    source,
                })?;
        }
        metadata.message_count = metadata.message_count.saturating_add(appended);
        if let Some(last) = last {
            metadata.last_message_fingerprint = Some(message_fingerprint(last)?);
        }
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_timestamp(now_ms));
        self.write_metadata(metadata)
    }

    /// Reports whether `messages` still extends what the log already holds.
    ///
    /// A shorter history, or a different message at the persisted boundary,
    /// means the transcript was rewound or compacted and must be rewritten.
    pub fn extends_persisted_log(
        metadata: &SessionMetadata,
        messages: &[&ModelMessage],
    ) -> Result<bool, StorageError> {
        let persisted = usize::try_from(metadata.message_count).unwrap_or(usize::MAX);
        if messages.len() < persisted {
            return Ok(false);
        }
        let Some(boundary) = persisted
            .checked_sub(1)
            .and_then(|index| messages.get(index))
        else {
            return Ok(persisted == 0);
        };
        Ok(metadata.last_message_fingerprint.as_deref() == Some(&message_fingerprint(boundary)?))
    }

    /// Rewrites the whole log, writing the session even when nothing is left
    /// in it: the reference's `allow_empty` save, which a rewind to the first
    /// message makes.
    pub fn replace_messages(
        &self,
        metadata: &mut SessionMetadata,
        messages: &[ModelMessage],
        now_ms: u64,
    ) -> Result<(), StorageError> {
        self.materialize(metadata)?;
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        let mut encoded = Vec::new();
        for message in messages
            .iter()
            .filter(|message| !matches!(message, ModelMessage::System { .. }))
        {
            encoded.extend(encode_message(message)?);
            encoded.push(b'\n');
        }
        write_atomically(&path, "messages", &encoded)?;
        // The reference's message list always opens with the system prompt; a
        // list stored here without one keeps the prompt already recorded.
        if let Some(record) = messages
            .iter()
            .find(|message| matches!(message, ModelMessage::System { .. }))
            .map(system_prompt_record)
            .transpose()?
        {
            metadata.system_prompt = Some(record);
        }
        let last = messages
            .iter()
            .rev()
            .find(|message| !matches!(message, ModelMessage::System { .. }));
        metadata.message_count = u64::try_from(
            messages
                .iter()
                .filter(|message| !matches!(message, ModelMessage::System { .. }))
                .count(),
        )
        .unwrap_or(u64::MAX);
        metadata.last_message_fingerprint = last.map(message_fingerprint).transpose()?;
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_timestamp(now_ms));
        self.write_metadata(metadata)
    }

    /// Writes `metadata` over the session's record. A session not yet written
    /// keeps the change in memory, where its first save picks it up.
    pub fn update_metadata(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        self.write_metadata(metadata)
    }

    /// One session's metadata, without reading its messages.
    ///
    /// What a caller updating a single metadata field reads first, so a rollout
    /// persisted into `experiments` never pays for a transcript it does not
    /// look at.
    pub fn metadata(&self, selector: &str) -> Result<SessionMetadata, StorageError> {
        self.resolve(selector)
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

    /// Reference `update_saved_session_title`: a rename is manual, and it is
    /// what a later automatic title never overrides.
    pub fn update_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<SessionMetadata, StorageError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StorageError::InvalidTitle);
        }
        let mut metadata = self.resolve_exact(session_id)?;
        metadata.title = Some(title.to_owned());
        "manual".clone_into(&mut metadata.title_source);
        self.write_metadata(&metadata)?;
        Ok(metadata)
    }

    /// Reference `refresh_auto_title`: a generated title replaces an earlier
    /// generated one and never a manual rename. Answers whether it changed.
    pub fn refresh_auto_title(
        &self,
        metadata: &mut SessionMetadata,
        title: &str,
    ) -> Result<bool, StorageError> {
        let title = title.trim();
        if metadata.title_source == "manual"
            || title.is_empty()
            || metadata.title.as_deref() == Some(title)
        {
            return Ok(false);
        }
        if let Ok(current) = self.resolve_exact(&metadata.id)
            && current.title_source == "manual"
        {
            return Ok(false);
        }
        metadata.title = Some(title.to_owned());
        "auto".clone_into(&mut metadata.title_source);
        self.write_metadata(metadata)?;
        Ok(true)
    }

    /// Reference `persist_bumped_at`: the latest accepted user interaction,
    /// kept monotonic. Answers the instant the session now records, in
    /// milliseconds.
    pub fn persist_bumped_at(
        &self,
        metadata: &mut SessionMetadata,
        now_ms: u64,
    ) -> Result<u64, StorageError> {
        if let Some(current) = metadata.bumped_at.as_deref().and_then(parse_iso_millis)
            && current >= now_ms
        {
            return Ok(current);
        }
        metadata.bumped_at = Some(format_iso_timestamp(now_ms));
        self.write_metadata(metadata)?;
        Ok(now_ms)
    }

    /// Reference `relocate_saved_session`: the session now works in `cwd`, and
    /// the place it began is kept, promoted from the environment entry when the
    /// record predates the field.
    pub fn relocate(&self, session_id: &str, cwd: &str) -> Result<SessionMetadata, StorageError> {
        let mut metadata = self.resolve_exact(session_id)?;
        relocate_metadata(&mut metadata, cwd);
        self.write_metadata(&metadata)?;
        Ok(metadata)
    }

    /// Stamps a delegated session's end. One that never wrote anything has no
    /// record to stamp.
    pub fn close(&self, selector: &str, now_ms: u64) -> Result<(), StorageError> {
        let mut metadata = match self.resolve(selector) {
            Ok(metadata) => metadata,
            Err(StorageError::SessionNotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_timestamp(now_ms));
        self.write_metadata(&metadata)
    }

    pub fn fork(
        &self,
        selector: &str,
        new_id: &str,
        current_system_prompt: &str,
        current_config: BTreeMap<String, Value>,
        now_ms: u64,
    ) -> Result<HydratedSession, StorageError> {
        self.fork_with_config_overlay(
            selector,
            new_id,
            current_system_prompt,
            current_config,
            BTreeMap::new(),
            now_ms,
        )
    }

    pub fn fork_with_config_overlay(
        &self,
        selector: &str,
        new_id: &str,
        current_system_prompt: &str,
        current_config: BTreeMap<String, Value>,
        config_overlay: BTreeMap<String, Value>,
        now_ms: u64,
    ) -> Result<HydratedSession, StorageError> {
        let parent = self.load(selector)?;
        let mut messages = Vec::with_capacity(parent.messages.len().saturating_add(1));
        messages.push(ModelMessage::System {
            content: current_system_prompt.to_owned(),
        });
        messages.extend(parent.messages);
        self.publish_handoff(
            &parent.metadata,
            new_id,
            messages,
            HandoffPlan {
                current_config,
                config_overlay,
                retain_parent: true,
            },
            now_ms,
        )
    }

    pub fn fork_rewound(
        &self,
        selector: &str,
        new_id: &str,
        keep_messages: usize,
        statistics: BTreeMap<String, Value>,
        now_ms: u64,
    ) -> Result<HydratedSession, StorageError> {
        let mut parent = self.load(selector)?;
        if keep_messages > parent.messages.len() {
            return Err(StorageError::InvalidRewind {
                requested: keep_messages,
                available: parent.messages.len(),
            });
        }
        let mut messages = parent.messages.clone();
        messages.truncate(keep_messages);
        parent.metadata.statistics = statistics;
        self.publish_handoff(
            &parent.metadata,
            new_id,
            messages,
            HandoffPlan {
                current_config: parent.metadata.config.clone(),
                config_overlay: BTreeMap::new(),
                retain_parent: true,
            },
            now_ms,
        )
    }

    pub fn rewind(
        &self,
        selector: &str,
        keep_messages: usize,
        statistics: BTreeMap<String, Value>,
        now_ms: u64,
    ) -> Result<HydratedSession, StorageError> {
        let mut hydrated = self.load(selector)?;
        if keep_messages > hydrated.messages.len() {
            return Err(StorageError::InvalidRewind {
                requested: keep_messages,
                available: hydrated.messages.len(),
            });
        }
        hydrated.messages.truncate(keep_messages);
        hydrated.metadata.statistics = statistics;
        self.replace_messages(&mut hydrated.metadata, &hydrated.messages, now_ms)?;
        Ok(hydrated)
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

    pub fn select_for_continue(&self, selector: &str) -> Result<(), StorageError> {
        let metadata = self.resolve(selector)?;
        self.write_pointer(&metadata.id)
    }

    /// Records `session_id` as the one this terminal last used, whether or not
    /// it has been written yet (reference `last_session_pointer.record`).
    pub fn record_pointer(&self, session_id: &str) -> Result<(), StorageError> {
        self.write_pointer(session_id)
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

    /// Brings sessions saved by an earlier format into directories: the
    /// reference's single-file `<prefix>_*.json` sessions
    /// (`session_migration.py`), and this port's own earlier files.
    pub fn migrate_legacy(&self) -> Result<MigrationReport, StorageError> {
        let mut report = MigrationReport {
            migrated: 0,
            skipped: 0,
            issues: Vec::new(),
        };
        if !self.root.is_dir() {
            return Ok(report);
        }
        let entries = fs::read_dir(&self.root).map_err(|source| StorageError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut interrupted = false;
        let mut candidates = Vec::new();
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(".migrating-") {
                interrupted = true;
            } else if path.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("json")
                && !name.starts_with('.')
            {
                candidates.push(path);
            }
        }
        // A store with nothing to move is left exactly as it is: the reference
        // writes no lock file into a save directory it only reads.
        if candidates.is_empty() && !interrupted {
            return Ok(report);
        }
        let lock_path = self.root.join(MIGRATION_LOCK_FILE);
        let _lock = FileLock::try_acquire(&lock_path, StorageError::MigrationInProgress)?;
        self.recover_migration_directories()?;
        candidates.retain(|path| path.is_file());
        candidates.sort();
        for path in candidates {
            match self.migrate_legacy_file(&path) {
                Ok(MigrationOutcome::Migrated) => {
                    report.migrated = report.migrated.saturating_add(1);
                }
                Ok(MigrationOutcome::Skipped) => {
                    report.skipped = report.skipped.saturating_add(1);
                }
                Err(error) => report.issues.push(MigrationIssue {
                    path,
                    message: error.to_string(),
                }),
            }
        }
        Ok(report)
    }

    /// The session `selector` names for reopening it.
    ///
    /// Reference `resolve_legacy_session_reference` then `find_session_by_id`:
    /// the full identifier, or its first eight characters when exactly one
    /// session shortens to them; among directories carrying that short
    /// identifier, the one whose log was written last.
    fn resolve(&self, selector: &str) -> Result<SessionMetadata, StorageError> {
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
    fn resolve_exact(&self, session_id: &str) -> Result<SessionMetadata, StorageError> {
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
        let prefix = format!("{}_", self.prefix);
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

    fn valid_metadata(&self) -> Result<Vec<SessionMetadata>, StorageError> {
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

    fn read_metadata_from_directory(
        &self,
        directory: &str,
    ) -> Result<SessionMetadata, StorageError> {
        let path = self.root.join(directory).join(METADATA_FILE);
        let bytes = fs::read(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let mut metadata: SessionMetadata =
            serde_json::from_slice(&bytes).map_err(|source| StorageError::CorruptMetadata {
                path: path.clone(),
                source,
            })?;
        validate_session_id(&metadata.id)?;
        // This port's first layout versioned its records; the key means
        // nothing to either implementation now.
        metadata.extra.remove("format_version");
        metadata.directory = directory.to_owned();
        metadata.working_directory = metadata
            .environment
            .get("working_directory")
            .and_then(Clone::clone)
            .unwrap_or_default();
        let modified = fs::metadata(self.root.join(directory).join(MESSAGES_FILE))
            .and_then(|item| item.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or_default();
        metadata.created_at_ms = parse_iso_millis(&metadata.start_time).unwrap_or(modified);
        metadata.updated_at_ms = metadata
            .end_time
            .as_deref()
            .and_then(parse_iso_millis)
            .unwrap_or(modified);
        Ok(metadata)
    }

    fn read_messages(&self, metadata: &SessionMetadata) -> Result<Vec<ModelMessage>, StorageError> {
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        let file = File::open(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let mut messages = Vec::new();
        let mut reader = BufReader::new(file);
        let mut index = 0_usize;
        loop {
            let mut line = String::new();
            let read = (&mut reader)
                .take(
                    u64::try_from(MAX_MESSAGE_RECORD_BYTES)
                        .unwrap_or(u64::MAX)
                        .saturating_add(1),
                )
                .read_line(&mut line)
                .map_err(|source| StorageError::Io {
                    path: path.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            index = index.saturating_add(1);
            if read > MAX_MESSAGE_RECORD_BYTES {
                return Err(StorageError::CorruptMessages {
                    path,
                    line: index,
                    message: format!(
                        "JSONL record exceeds the {MAX_MESSAGE_RECORD_BYTES}-byte limit"
                    ),
                });
            }
            if line.trim().is_empty() {
                return Err(StorageError::CorruptMessages {
                    path,
                    line: index,
                    message: "empty JSONL record".to_owned(),
                });
            }
            let message =
                decode_message(&line).map_err(|message| StorageError::CorruptMessages {
                    path: path.clone(),
                    line: index,
                    message,
                })?;
            if !matches!(message, ModelMessage::System { .. }) {
                messages.push(message);
            }
        }
        let actual = u64::try_from(messages.len()).unwrap_or(u64::MAX);
        if actual == 0 && metadata.message_count != 0 {
            return Err(StorageError::MessageCountMismatch {
                expected: metadata.message_count,
                actual,
            });
        }
        Ok(messages)
    }

    /// Writes `metadata` over its record: every key the record already holds
    /// that the model does not name is kept, which is what the reference's
    /// field patches do. A session not yet written stays unwritten.
    fn write_metadata(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        let session_path = self.session_path(metadata);
        if !session_path.join(MESSAGES_FILE).is_file() {
            self.remember_pending(metadata);
            return Ok(());
        }
        self.discard_pending(&metadata.id);
        let path = session_path.join(METADATA_FILE);
        let mut record = metadata.clone();
        if let Ok(bytes) = fs::read(&path)
            && let Ok(Value::Object(existing)) = serde_json::from_slice::<Value>(&bytes)
        {
            let known = serde_json::to_value(metadata)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            for (key, value) in existing {
                if key != "format_version" && !known.contains_key(&key) {
                    record.extra.entry(key).or_insert(value);
                }
            }
        }
        let encoded = serde_json::to_vec_pretty(&record).map_err(StorageError::Json)?;
        write_atomically(&path, "meta", &encoded).map_err(StorageError::from)
    }

    fn write_pointer(&self, session_id: &str) -> Result<(), StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(());
        };
        let pointer_directory = self.root.join(LAST_SESSION_DIRECTORY);
        ensure_private_directory(&pointer_directory)?;
        let path = pointer_directory.join(pointer_key);
        write_atomically(&path, "pointer", format!("{session_id}\n").as_bytes())
            .map_err(StorageError::from)
    }

    fn read_pointer(&self) -> Result<Option<String>, StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(None);
        };
        let path = self.root.join(LAST_SESSION_DIRECTORY).join(pointer_key);
        match fs::read_to_string(&path) {
            Ok(pointer) => {
                let pointer = pointer.trim();
                if pointer.is_empty()
                    || pointer.contains('/')
                    || pointer.contains('\\')
                    || pointer == "."
                    || pointer == ".."
                {
                    Ok(None)
                } else {
                    Ok(Some(pointer.to_owned()))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StorageError::Io { path, source }),
        }
    }

    /// The session the terminal's pointer names, if any.
    pub fn pointer(&self) -> Option<String> {
        self.read_pointer().ok().flatten()
    }

    /// The directory a session is written under.
    pub fn session_directory(&self, selector: &str) -> Result<PathBuf, StorageError> {
        let metadata = self.resolve(selector)?;
        Ok(self.session_path(&metadata))
    }

    /// The directory `metadata` is, or will be, written under.
    #[must_use]
    pub fn session_path(&self, metadata: &SessionMetadata) -> PathBuf {
        self.root.join(&metadata.directory)
    }

    /// Reference `last_session_pointer.clear_matching`: every terminal's
    /// pointer that names the session is removed, not only this one's.
    fn clear_pointer_if_matches(&self, session_id: &str) -> Result<(), StorageError> {
        let pointer_directory = self.root.join(LAST_SESSION_DIRECTORY);
        let Ok(entries) = fs::read_dir(&pointer_directory) else {
            return Ok(());
        };
        let mut removed = false;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !path.is_file()
                || name.starts_with(HANDOFF_JOURNAL_PREFIX)
                || name.starts_with(HANDOFF_LOCK_PREFIX)
            {
                continue;
            }
            if fs::read_to_string(&path).is_ok_and(|pointer| pointer.trim() == session_id)
                && fs::remove_file(&path).is_ok()
            {
                removed = true;
            }
        }
        if removed {
            sync_directory(&pointer_directory)?;
        }
        Ok(())
    }

    fn append_message_to_path(
        &self,
        path: &Path,
        metadata: &mut SessionMetadata,
        message: &ModelMessage,
    ) -> Result<(), StorageError> {
        if matches!(message, ModelMessage::System { .. }) {
            metadata.system_prompt = Some(system_prompt_record(message)?);
            return Ok(());
        }
        let encoded = encode_message(message)?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        file.write_all(&encoded)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_data())
            .map_err(|source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        metadata.message_count = metadata.message_count.saturating_add(1);
        metadata.last_message_fingerprint = Some(message_fingerprint(message)?);
        Ok(())
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

/// Reference `SessionLogger.relocated_to` and `relocate_saved_session_at_path`.
pub fn relocate_metadata(metadata: &mut SessionMetadata, cwd: &str) {
    if metadata.origin_directory.is_none() {
        metadata.origin_directory = metadata
            .environment
            .get("working_directory")
            .cloned()
            .flatten();
    }
    metadata
        .environment
        .insert("working_directory".to_owned(), Some(cwd.to_owned()));
    cwd.clone_into(&mut metadata.working_directory);
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LegacySession {
    session_id: String,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    messages: Vec<ModelMessage>,
    #[serde(default)]
    statistics: BTreeMap<String, Value>,
    #[serde(default)]
    experiments: Value,
    #[serde(default)]
    config: BTreeMap<String, Value>,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    updated_at_ms: u64,
}

pub(super) enum MigrationOutcome {
    Migrated,
    Skipped,
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

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid session ID `{0}`")]
    InvalidSessionId(String),
    #[error("session `{0}` was not found")]
    SessionNotFound(String),
    #[error("session selector `{0}` is ambiguous")]
    AmbiguousSession(String),
    #[error("session ID `{0}` already exists")]
    DuplicateSessionId(String),
    #[error("no valid sessions exist")]
    NoSessions,
    #[error("I/O failure at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid metadata at `{path}`: {source}")]
    CorruptMetadata {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("Session title cannot be empty.")]
    InvalidTitle,
    #[error("cannot rewind to {requested} messages; only {available} are available")]
    InvalidRewind { requested: usize, available: usize },
    #[error("session migration is already in progress")]
    MigrationInProgress,
    #[error("invalid legacy session at `{path}`: {source}")]
    CorruptLegacy {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("delete of session `{session_id}` was interrupted at `{tombstone}`: {source}")]
    InterruptedDelete {
        session_id: String,
        tombstone: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid message record {line} at `{path}`: {message}")]
    CorruptMessages {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("message count mismatch: metadata declares {expected}, log contains {actual}")]
    MessageCountMismatch { expected: u64, actual: u64 },
    #[error("invalid or conflicting handoff transaction journal at `{0}`")]
    InvalidHandoffJournal(PathBuf),
    #[error("JSON serialization failed: {0}")]
    Json(serde_json::Error),
}

impl From<AtomicWriteError> for StorageError {
    fn from(error: AtomicWriteError) -> Self {
        Self::Io {
            path: error.path,
            source: error.source,
        }
    }
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

pub(super) fn message_fingerprint(message: &ModelMessage) -> Result<String, StorageError> {
    let value = message_record(message)?;
    Ok(hex_encode(&Sha256::digest(
        python_canonical_json(&value).as_bytes(),
    )))
}

pub(super) fn default_title_source() -> String {
    "auto".to_owned()
}

fn unknown_username() -> String {
    "unknown".to_owned()
}

/// Reads JSON `null` as the type's default, which is how a record written with
/// an explicit `null` for an optional group loads.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The record a message is written to the log as.
fn message_record(message: &ModelMessage) -> Result<Value, StorageError> {
    serde_json::to_value(message).map_err(StorageError::Json)
}

fn encode_message(message: &ModelMessage) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(&message_record(message)?).map_err(StorageError::Json)
}

fn decode_message(line: &str) -> Result<ModelMessage, String> {
    serde_json::from_str(line).map_err(|error| error.to_string())
}

/// What `meta.json` records as the system prompt.
fn system_prompt_record(message: &ModelMessage) -> Result<Value, StorageError> {
    message_record(message)
}

/// Reference `getpass.getuser`: the first of `LOGNAME`, `USER`, `LNAME` and
/// `USERNAME` that is set.
fn current_username() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(unknown_username)
}

/// Reference `SessionLogger._fetch_git_metadata`: the commit and branch the
/// working directory is on, when it is a git checkout.
fn git_metadata(working_directory: &Path) -> (Option<String>, Option<String>) {
    if !working_directory.is_dir() {
        return (None, None);
    }
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD", "--abbrev-ref", "HEAD"])
        .current_dir(working_directory)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return (None, None);
    };
    if !output.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.trim().lines();
    (
        lines.next().map(ToOwned::to_owned),
        lines.next().map(ToOwned::to_owned),
    )
}

fn same_working_directory(stored: &str, current: &str) -> bool {
    if stored == current {
        return true;
    }
    let stored = fs::canonicalize(stored);
    let current = fs::canonicalize(current);
    matches!((stored, current), (Ok(stored), Ok(current)) if stored == current)
}

fn current_tty_key() -> Option<String> {
    #[cfg(unix)]
    {
        for descriptor in ["0", "1", "2"] {
            let path = PathBuf::from("/proc/self/fd").join(descriptor);
            if let Ok(target) = fs::read_link(path)
                && target.starts_with("/dev/")
                && target != Path::new("/dev/null")
                && let Some(name) = target.file_name().and_then(|name| name.to_str())
            {
                return Some(sanitize_pointer_key(name));
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        // Reference `_windows_tty_key` without the console window handle,
        // which needs a Win32 call this crate does not make: the Windows
        // Terminal session, and otherwise the parent process.
        Some(match std::env::var("WT_SESSION") {
            Ok(value) => sanitize_pointer_key(&format!("wt-{value}")),
            Err(_) => sanitize_pointer_key(&format!("ppid-{}", parent_process_id())),
        })
    }
}

#[cfg(not(unix))]
fn parent_process_id() -> u32 {
    std::process::id()
}

fn sanitize_pointer_key(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized.to_owned()
    }
}

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod lease_tests;
#[cfg(test)]
mod permissions_tests;
#[cfg(test)]
mod storage_tests;
