use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::atomic_file::{self, AtomicWriteError, create_private_file, write_atomically};
use crate::events::ModelMessage;
use crate::text::hex_encode;

mod canonical_json;
mod handoff;
mod migration;
mod time;

use canonical_json::python_canonical_json;
use time::{format_compact_timestamp, format_iso_timestamp, timestamp_sort_key};

pub(super) const METADATA_FILE: &str = "meta.json";
pub(super) const MESSAGES_FILE: &str = "messages.jsonl";
pub(super) const LAST_SESSION_DIRECTORY: &str = ".last_session";
pub(super) const HANDOFF_JOURNAL_PREFIX: &str = ".handoff-transaction-";
pub(super) const HANDOFF_LOCK_PREFIX: &str = ".handoff-lock-";
pub(super) const MIGRATION_LOCK_FILE: &str = ".migration.lock";
pub(super) const MIGRATION_SUFFIX: &str = ".legacy.bak";
pub(super) const CURRENT_FORMAT_VERSION: u32 = 2;
pub(super) const MAX_MESSAGE_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub(super) static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(default = "current_format_version")]
    pub format_version: u32,
    #[serde(rename = "session_id")]
    pub id: String,
    #[serde(skip)]
    pub directory: String,
    pub start_time: String,
    pub end_time: Option<String>,
    pub git_commit: Option<String>,
    pub git_branch: Option<String>,
    pub environment: BTreeMap<String, Option<String>>,
    pub username: String,
    #[serde(default)]
    pub child_sessions: Vec<Value>,
    #[serde(default)]
    pub loops: Vec<Value>,
    pub title: Option<String>,
    #[serde(default = "default_title_source")]
    pub title_source: String,
    #[serde(default, rename = "experiments")]
    pub experiment_state: Value,
    #[serde(default, rename = "total_messages")]
    pub message_count: u64,
    pub last_message_fingerprint: Option<String>,
    #[serde(default, rename = "stats")]
    pub statistics: BTreeMap<String, Value>,
    #[serde(default)]
    pub tools_available: Vec<Value>,
    #[serde(default)]
    pub config: BTreeMap<String, Value>,
    #[serde(default)]
    pub agent_profile: Option<Value>,
    #[serde(default)]
    pub system_prompt: Option<Value>,
    #[serde(skip)]
    pub created_at_ms: u64,
    #[serde(skip)]
    pub updated_at_ms: u64,
    #[serde(skip)]
    pub working_directory: String,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydratedSession {
    pub metadata: SessionMetadata,
    pub messages: Vec<ModelMessage>,
    pub current_config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub start_time: String,
    pub end_time: Option<String>,
    pub working_directory: String,
    pub parent_session_id: Option<String>,
    pub message_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next_offset: Option<usize>,
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
}

impl SessionStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            pointer_key: current_tty_key(),
        }
    }

    #[must_use]
    pub fn with_pointer_key(mut self, pointer_key: impl Into<String>) -> Self {
        self.pointer_key = Some(sanitize_pointer_key(&pointer_key.into()));
        self
    }

    pub fn create(
        &self,
        id: &str,
        working_directory: &str,
        parent_session_id: Option<String>,
        now_ms: u64,
    ) -> Result<SessionMetadata, StorageError> {
        validate_session_id(id)?;
        ensure_private_directory(&self.root)?;
        let directory = session_directory_name(now_ms, id);
        let metadata =
            self.initialize_session(&directory, id, working_directory, parent_session_id, now_ms)?;
        self.write_pointer(id)?;
        Ok(metadata)
    }

    pub fn create_child(
        &self,
        id: &str,
        working_directory: &str,
        parent_session_id: String,
        now_ms: u64,
    ) -> Result<SessionMetadata, StorageError> {
        validate_session_id(id)?;
        ensure_private_directory(&self.root)?;
        if self
            .valid_metadata()?
            .iter()
            .any(|metadata| metadata.id == id)
        {
            return Err(StorageError::DuplicateSessionId(id.to_owned()));
        }
        let directory = session_directory_name(now_ms, id);
        self.initialize_session(
            &directory,
            id,
            working_directory,
            Some(parent_session_id),
            now_ms,
        )
    }

    fn initialize_session(
        &self,
        directory: &str,
        id: &str,
        working_directory: &str,
        parent_session_id: Option<String>,
        now_ms: u64,
    ) -> Result<SessionMetadata, StorageError> {
        let session_path = self.root.join(directory);
        create_private_directory(&session_path)?;
        create_private_file(&session_path.join(MESSAGES_FILE)).map_err(|source| {
            StorageError::Io {
                path: session_path.join(MESSAGES_FILE),
                source,
            }
        })?;
        let start_time = format_iso_timestamp(now_ms);
        let mut environment = BTreeMap::new();
        environment.insert(
            "working_directory".to_owned(),
            Some(working_directory.to_owned()),
        );
        let metadata = SessionMetadata {
            format_version: CURRENT_FORMAT_VERSION,
            id: id.to_owned(),
            directory: directory.to_owned(),
            start_time,
            end_time: None,
            git_commit: None,
            git_branch: None,
            environment,
            username: std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
            child_sessions: Vec::new(),
            loops: Vec::new(),
            title: None,
            title_source: default_title_source(),
            experiment_state: Value::Null,
            message_count: 0,
            last_message_fingerprint: None,
            statistics: BTreeMap::new(),
            tools_available: Vec::new(),
            config: BTreeMap::new(),
            agent_profile: None,
            system_prompt: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            working_directory: working_directory.to_owned(),
            parent_session_id,
        };
        self.write_metadata(&metadata)?;
        Ok(metadata)
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
    /// metadata, because only the current one is ever replayed.
    pub fn append_messages(
        &self,
        metadata: &mut SessionMetadata,
        messages: &[ModelMessage],
        now_ms: u64,
    ) -> Result<(), StorageError> {
        if messages.is_empty() {
            return Ok(());
        }
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        let mut encoded = Vec::new();
        let mut appended = 0_u64;
        let mut last = None;
        for message in messages {
            if matches!(message, ModelMessage::System { .. }) {
                metadata.system_prompt =
                    Some(serde_json::to_value(message).map_err(StorageError::Json)?);
                continue;
            }
            serde_json::to_writer(&mut encoded, message).map_err(StorageError::Json)?;
            encoded.push(b'\n');
            appended = appended.saturating_add(1);
            last = Some(message);
        }
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

    pub fn replace_messages(
        &self,
        metadata: &mut SessionMetadata,
        messages: &[ModelMessage],
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        let mut encoded = Vec::new();
        for message in messages
            .iter()
            .filter(|message| !matches!(message, ModelMessage::System { .. }))
        {
            serde_json::to_writer(&mut encoded, message).map_err(StorageError::Json)?;
            encoded.push(b'\n');
        }
        write_atomically(&path, "messages", &encoded)?;
        metadata.system_prompt = messages
            .iter()
            .find(|message| matches!(message, ModelMessage::System { .. }))
            .map(serde_json::to_value)
            .transpose()
            .map_err(StorageError::Json)?;
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
        let prompt = current_system_prompt.into();
        if let Some(pointer) = self.read_pointer()?
            && let Ok(metadata) = self.resolve(&pointer)
            && same_working_directory(&metadata.working_directory, working_directory)
        {
            return self.resume(&metadata.id, prompt, current_config);
        }
        let latest = self
            .valid_metadata()?
            .into_iter()
            .filter(|metadata| {
                same_working_directory(&metadata.working_directory, working_directory)
            })
            .max_by_key(|metadata| (metadata.updated_at_ms, metadata.created_at_ms))
            .ok_or(StorageError::NoSessions)?;
        self.resume(&latest.id, prompt, current_config)
    }

    pub fn list(
        &self,
        working_directory: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SessionPage, StorageError> {
        if !(1..=500).contains(&limit) {
            return Err(StorageError::InvalidPaginationLimit(limit));
        }
        let mut metadata = self.valid_metadata()?;
        if let Some(working_directory) = working_directory {
            metadata
                .retain(|item| same_working_directory(&item.working_directory, working_directory));
        }
        metadata.sort_by(|left, right| {
            (right.updated_at_ms, right.created_at_ms, &right.id).cmp(&(
                left.updated_at_ms,
                left.created_at_ms,
                &left.id,
            ))
        });
        let total = metadata.len();
        let sessions = metadata
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|metadata| SessionSummary {
                id: metadata.id,
                title: metadata.title,
                start_time: metadata.start_time,
                end_time: metadata.end_time,
                working_directory: metadata.working_directory,
                parent_session_id: metadata.parent_session_id,
                message_count: metadata.message_count,
            })
            .collect::<Vec<_>>();
        let consumed = offset.saturating_add(sessions.len());
        Ok(SessionPage {
            sessions,
            next_offset: (consumed < total).then_some(consumed),
        })
    }

    pub fn history(
        &self,
        selector: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ModelMessage>, StorageError> {
        if !(1..=500).contains(&limit) {
            return Err(StorageError::InvalidPaginationLimit(limit));
        }
        Ok(self
            .load(selector)?
            .messages
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect())
    }

    pub fn update_title(
        &self,
        selector: &str,
        title: &str,
        now_ms: u64,
    ) -> Result<SessionMetadata, StorageError> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 200 {
            return Err(StorageError::InvalidTitle);
        }
        let mut metadata = self.resolve(selector)?;
        metadata.title = Some(title.to_owned());
        metadata.title_source = "user".to_owned();
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_timestamp(now_ms));
        self.write_metadata(&metadata)?;
        Ok(metadata)
    }

    pub fn close(&self, selector: &str, now_ms: u64) -> Result<SessionMetadata, StorageError> {
        let mut metadata = self.resolve(selector)?;
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_timestamp(now_ms));
        self.write_metadata(&metadata)?;
        Ok(metadata)
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

    pub fn delete(&self, selector: &str) -> Result<(), StorageError> {
        let metadata = self.resolve(selector)?;
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
        sync_directory(&self.root)
    }

    pub fn select_for_continue(&self, selector: &str) -> Result<(), StorageError> {
        let metadata = self.resolve(selector)?;
        self.write_pointer(&metadata.id)
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

    pub fn migrate_legacy(&self) -> Result<MigrationReport, StorageError> {
        ensure_private_directory(&self.root)?;
        let lock_path = self.root.join(MIGRATION_LOCK_FILE);
        let _lock = FileLock::try_acquire(&lock_path, StorageError::MigrationInProgress)?;
        self.recover_migration_directories()?;
        let entries = fs::read_dir(&self.root).map_err(|source| StorageError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut candidates = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path.extension().and_then(|extension| extension.to_str()) == Some("json")
            })
            .collect::<Vec<_>>();
        candidates.sort();
        let mut report = MigrationReport {
            migrated: 0,
            skipped: 0,
            issues: Vec::new(),
        };
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

    fn resolve(&self, selector: &str) -> Result<SessionMetadata, StorageError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::SessionNotFound(selector.to_owned()));
            }
            Err(source) => {
                return Err(StorageError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };
        let mut matches = Vec::new();
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
            let exact_path = directory == selector;
            let metadata = self.read_metadata_from_directory(&directory);
            let exact_metadata = metadata
                .as_ref()
                .is_ok_and(|metadata| metadata.id == selector);
            let candidate_by_metadata = metadata
                .as_ref()
                .is_ok_and(|metadata| exact_metadata || metadata.id.starts_with(selector));
            let candidate_by_path = directory.ends_with(&format!("_{}", directory_id(selector)));
            if exact_path || candidate_by_path || candidate_by_metadata {
                matches.push((exact_path || exact_metadata, metadata));
            }
        }
        let exact_matches = matches.iter().filter(|(exact, _)| *exact).count();
        if exact_matches == 1 {
            let position = matches
                .iter()
                .position(|(exact, _)| *exact)
                .ok_or_else(|| StorageError::SessionNotFound(selector.to_owned()))?;
            return matches.swap_remove(position).1;
        }
        if exact_matches > 1 {
            return Err(StorageError::AmbiguousSession(selector.to_owned()));
        }
        match matches.len() {
            0 => Err(StorageError::SessionNotFound(selector.to_owned())),
            1 => matches
                .pop()
                .map(|(_, metadata)| metadata)
                .ok_or_else(|| StorageError::SessionNotFound(selector.to_owned()))?,
            _ => Err(StorageError::AmbiguousSession(selector.to_owned())),
        }
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
        if metadata.format_version > CURRENT_FORMAT_VERSION {
            return Err(StorageError::UnsupportedFormat {
                path,
                version: metadata.format_version,
            });
        }
        metadata.directory = directory.to_owned();
        metadata.working_directory = metadata
            .environment
            .get("working_directory")
            .and_then(Clone::clone)
            .unwrap_or_default();
        let modified_key = fs::metadata(self.root.join(directory).join(MESSAGES_FILE))
            .and_then(|item| item.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
            .unwrap_or_default();
        metadata.created_at_ms = timestamp_sort_key(&metadata.start_time).unwrap_or(modified_key);
        metadata.updated_at_ms = metadata
            .end_time
            .as_deref()
            .and_then(timestamp_sort_key)
            .unwrap_or(modified_key);
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
            let message: ModelMessage =
                serde_json::from_str(&line).map_err(|error| StorageError::CorruptMessages {
                    path: path.clone(),
                    line: index,
                    message: error.to_string(),
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

    fn write_metadata(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        let path = self.session_path(metadata).join(METADATA_FILE);
        let mut encoded = serde_json::to_vec_pretty(metadata).map_err(StorageError::Json)?;
        encoded.push(b'\n');
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

    fn session_path(&self, metadata: &SessionMetadata) -> PathBuf {
        self.root.join(&metadata.directory)
    }

    fn clear_pointer_if_matches(&self, session_id: &str) -> Result<(), StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(());
        };
        let path = self.root.join(LAST_SESSION_DIRECTORY).join(pointer_key);
        match fs::read_to_string(&path) {
            Ok(pointer) if pointer.trim() == session_id => {
                fs::remove_file(&path).map_err(|source| StorageError::Io {
                    path: path.clone(),
                    source,
                })?;
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StorageError::Io { path, source }),
        }
    }

    fn append_message_to_path(
        &self,
        path: &Path,
        metadata: &mut SessionMetadata,
        message: &ModelMessage,
    ) -> Result<(), StorageError> {
        if matches!(message, ModelMessage::System { .. }) {
            metadata.system_prompt =
                Some(serde_json::to_value(message).map_err(StorageError::Json)?);
            return Ok(());
        }
        let encoded = serde_json::to_vec(message).map_err(StorageError::Json)?;
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
    #[error("unsupported session format {version} at `{path}`")]
    UnsupportedFormat { path: PathBuf, version: u32 },
    #[error("pagination limit must be between 1 and 500, got {0}")]
    InvalidPaginationLimit(usize),
    #[error("session title must contain 1 to 200 characters")]
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

pub(super) const fn current_format_version() -> u32 {
    CURRENT_FORMAT_VERSION
}

pub(super) fn is_internal_directory(directory: &str) -> bool {
    directory.starts_with(".deleting-")
        || directory.starts_with(".migrating-")
        || directory.starts_with(".handoff-")
}

pub(super) fn is_safe_handoff_component(component: &str, prefix: &str) -> bool {
    component.starts_with(prefix)
        && !component.contains('/')
        && !component.contains('\\')
        && component != "."
        && component != ".."
}

pub(super) fn session_directory_name(now_ms: u64, id: &str) -> String {
    format!(
        "session_{}_{}",
        format_compact_timestamp(now_ms),
        directory_id(id)
    )
}

pub(super) fn directory_id(id: &str) -> String {
    let digest = Sha256::digest(id.as_bytes());
    hex_encode(digest.get(..16).unwrap_or(&digest))
}

pub(super) fn message_fingerprint(message: &ModelMessage) -> Result<String, StorageError> {
    let value = serde_json::to_value(message).map_err(StorageError::Json)?;
    Ok(hex_encode(&Sha256::digest(
        python_canonical_json(&value).as_bytes(),
    )))
}

pub(super) fn default_title_source() -> String {
    "auto".to_owned()
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
                && let Some(name) = target.file_name().and_then(|name| name.to_str())
            {
                return Some(sanitize_pointer_key(name));
            }
        }
    }
    std::env::var("WT_SESSION")
        .ok()
        .map(|value| sanitize_pointer_key(&format!("wt-{value}")))
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
mod tests {
    use super::*;

    fn user(content: &str) -> ModelMessage {
        ModelMessage::user(content.to_owned())
    }

    /// US-172: the synthetic pair a slash invocation appends is made of the
    /// message shapes the store already persists, so it round-trips through a
    /// session unchanged.
    #[test]
    fn the_invoked_skill_pair_round_trips_through_the_store() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let mut metadata = store
            .create("session-skill", "/workspace", None, 10)
            .expect("session creates");
        let pair = [
            user("/probe"),
            ModelMessage::Assistant {
                content: String::new(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: vec![crate::events::ModelToolCall {
                    id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                    name: "skill".to_owned(),
                    arguments: "{\"name\":\"probe\"}".to_owned(),
                }],
            },
            ModelMessage::Tool {
                call_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                content: format!(
                    "name: probe\ncontent: {}\nBody\n</skill_content>\nskill_dir: None",
                    crate::skills::skill_content_marker("probe")
                ),
                is_error: false,
            },
        ];
        store
            .append_messages(&mut metadata, &pair, 11)
            .expect("the pair persists");

        let hydrated = store.load("session-skill").expect("session loads");
        assert_eq!(hydrated.messages, pair.to_vec());
    }

    #[test]
    fn sessions_append_atomically_and_resume_with_current_system_context() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let mut metadata = store
            .create("session-alpha", "/workspace", None, 10)
            .expect("session creates");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::System {
                    content: "old system".to_owned(),
                },
                11,
            )
            .expect("system appends");
        store
            .append_message(&mut metadata, &user("hello"), 12)
            .expect("user appends");
        metadata
            .statistics
            .insert("tokens".to_owned(), Value::from(4));
        metadata.experiment_state = serde_json::json!({"variant": "b"});
        store
            .update_metadata(&metadata)
            .expect("metadata updates atomically");

        let hydrated = store
            .resume(
                "session-alpha",
                "current system",
                BTreeMap::from([("model".to_owned(), Value::String("new".to_owned()))]),
            )
            .expect("session resumes");
        assert_eq!(
            hydrated.messages,
            vec![
                ModelMessage::System {
                    content: "current system".to_owned()
                },
                user("hello")
            ]
        );
        assert_eq!(hydrated.metadata.statistics["tokens"], 4);
        assert_eq!(hydrated.current_config["model"], "new");
    }

    #[test]
    fn continue_prefers_valid_pointer_then_latest_valid_session() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("test-tty");
        store
            .create("older", "/workspace", None, 10)
            .expect("older session");
        store
            .create("newer", "/workspace", None, 20)
            .expect("newer session");
        let continued = store
            .continue_session("/workspace", "system", BTreeMap::new())
            .expect("valid pointer");
        assert_eq!(continued.metadata.id, "newer");

        fs::write(
            temporary
                .path()
                .join(LAST_SESSION_DIRECTORY)
                .join("test-tty"),
            "stale\n",
        )
        .expect("stale pointer fixture");
        let fallback = store
            .continue_session("/workspace", "system", BTreeMap::new())
            .expect("latest fallback");
        assert_eq!(fallback.metadata.id, "newer");
    }

    #[test]
    fn exact_session_ids_win_over_longer_prefix_matches() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        store
            .create("session", "/workspace", None, 10)
            .expect("exact session");
        store
            .create("session-longer", "/workspace", None, 1_020)
            .expect("prefixed session");
        assert_eq!(
            store.load("session").expect("exact match").metadata.id,
            "session"
        );
    }

    #[test]
    fn a_durable_log_record_ahead_of_metadata_is_recovered_in_memory() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let mut metadata = store
            .create("session-recover", "/workspace", None, 10)
            .expect("session creates");
        let log = store.session_path(&metadata).join(MESSAGES_FILE);
        let encoded = serde_json::to_string(&user("durable")).expect("message serializes");
        fs::write(&log, format!("{encoded}\n")).expect("simulated durable append");

        let recovered = store.load("session-recover").expect("record recovers");
        assert_eq!(recovered.metadata.message_count, 1);
        metadata = recovered.metadata;
        store
            .append_message(&mut metadata, &user("next"), 11)
            .expect("metadata catches up");
        assert_eq!(
            store
                .load("session-recover")
                .expect("session remains loadable")
                .messages
                .len(),
            2
        );
    }

    #[test]
    fn corruption_and_ambiguous_short_ids_never_overwrite_evidence() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let first = store
            .create("prefix-one", "/workspace", None, 10)
            .expect("first session");
        store
            .create("prefix-two", "/workspace", None, 20)
            .expect("second session");
        assert!(matches!(
            store.load("prefix"),
            Err(StorageError::AmbiguousSession(_))
        ));

        let log = store.session_path(&first).join(MESSAGES_FILE);
        fs::write(&log, b"{\"role\":\"user\"").expect("truncate fixture log");
        let before = fs::read(&log).expect("fixture remains readable");
        assert!(matches!(
            store.load("prefix-one"),
            Err(StorageError::CorruptMessages { .. })
        ));
        assert_eq!(fs::read(log).expect("evidence preserved"), before);
    }

    #[test]
    fn path_traversal_session_ids_are_rejected() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        assert!(matches!(
            store.create("../escape", "/workspace", None, 10),
            Err(StorageError::InvalidSessionId(_))
        ));
        assert!(!temporary.path().join("escape").exists());
    }

    #[test]
    fn lifecycle_operations_are_durable_paginated_and_parent_linked() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("test");
        let mut parent = store
            .create("parent-session", "/workspace", None, 10)
            .expect("parent session");
        store
            .append_message(&mut parent, &user("one"), 11)
            .expect("first message");
        store
            .append_message(&mut parent, &user("two"), 12)
            .expect("second message");
        store
            .update_title("parent-session", "Named session", 13)
            .expect("title updates");

        let child = store
            .fork(
                "parent-session",
                "child-session",
                "current prompt",
                BTreeMap::from([("model".to_owned(), Value::String("current".to_owned()))]),
                20,
            )
            .expect("session forks");
        assert_eq!(
            child.metadata.parent_session_id.as_deref(),
            Some("parent-session")
        );
        assert_eq!(child.messages.len(), 3);
        assert_eq!(child.current_config["model"], "current");

        let page = store.list(Some("/workspace"), 0, 1).expect("list page");
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.next_offset, Some(1));
        assert_eq!(
            store.history("parent-session", 1, 1).expect("history page"),
            [user("two")]
        );

        let rewind = store
            .rewind("parent-session", 1, BTreeMap::new(), 30)
            .expect("session rewinds");
        assert_eq!(rewind.messages, [user("one")]);
        assert_eq!(
            store
                .load("parent-session")
                .expect("rewind survives restart")
                .messages,
            [user("one")]
        );
        store
            .close("parent-session", 31)
            .expect("session closes durably");
        store.delete("child-session").expect("child deletes");
        assert!(matches!(
            store.load("child-session"),
            Err(StorageError::SessionNotFound(_))
        ));
    }

    #[test]
    fn rewound_fork_is_published_once_without_mutating_its_parent() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("rewind-fork");
        let mut parent = store
            .create("parent", "/workspace", None, 10)
            .expect("parent session");
        parent
            .config
            .insert("model".to_owned(), Value::from("parent-model"));
        store.update_metadata(&parent).expect("parent metadata");
        for (index, message) in [user("one"), user("two"), user("three")]
            .into_iter()
            .enumerate()
        {
            store
                .append_message(&mut parent, &message, 11 + index as u64)
                .expect("parent message");
        }

        let child = store
            .fork_rewound(
                "parent",
                "child",
                2,
                BTreeMap::from([("tokens".to_owned(), Value::from(42))]),
                20,
            )
            .expect("rewound fork");

        assert_eq!(child.metadata.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(child.messages, [user("one"), user("two")]);
        assert_eq!(child.current_config["model"], "parent-model");
        assert_eq!(child.metadata.statistics["tokens"], 42);
        assert_eq!(
            store
                .load("parent")
                .expect("parent remains intact")
                .messages,
            [user("one"), user("two"), user("three")]
        );
    }

    #[test]
    fn legacy_migration_is_versioned_retryable_and_isolates_bad_entries() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        fs::write(
            temporary.path().join("valid.json"),
            serde_json::to_vec(&serde_json::json!({
                "sessionId": "legacy-session",
                "workingDirectory": "/workspace",
                "messages": [
                    {"role": "user", "content": "legacy"}
                ],
                "statistics": {"tokens": 2},
                "experiments": {"variant": "a"},
                "createdAtMs": 10,
                "updatedAtMs": 11
            }))
            .expect("legacy serializes"),
        )
        .expect("legacy fixture");
        fs::write(temporary.path().join("broken.json"), b"{").expect("broken legacy fixture");

        let report = store.migrate_legacy().expect("migration completes");
        assert_eq!(report.migrated, 1);
        assert_eq!(report.issues.len(), 1);
        let migrated = store
            .load("legacy-session")
            .expect("migrated session loads");
        assert_eq!(migrated.metadata.format_version, CURRENT_FORMAT_VERSION);
        assert_eq!(migrated.messages, [user("legacy")]);
        assert!(temporary.path().join("valid.json.legacy.bak").is_file());

        let retry = store.migrate_legacy().expect("migration retry completes");
        assert_eq!(retry.migrated, 0);
        assert_eq!(retry.issues.len(), 1);
    }

    #[test]
    fn migration_lock_and_interrupted_delete_artifacts_fail_safe() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        fs::create_dir_all(temporary.path()).expect("root exists");
        let lock = FileLock::try_acquire(
            &temporary.path().join(MIGRATION_LOCK_FILE),
            StorageError::MigrationInProgress,
        )
        .expect("migration lock");
        assert!(matches!(
            store.migrate_legacy(),
            Err(StorageError::MigrationInProgress)
        ));
        drop(lock);
        fs::write(
            temporary.path().join(MIGRATION_LOCK_FILE),
            b"stale process marker",
        )
        .expect("stale lock fixture");
        let recovered = FileLock::try_acquire(
            &temporary.path().join(MIGRATION_LOCK_FILE),
            StorageError::MigrationInProgress,
        )
        .expect("OS lock ignores stale file contents");
        drop(recovered);

        let tombstone = temporary.path().join(".deleting-1-stale");
        fs::create_dir(&tombstone).expect("tombstone fixture");
        fs::write(tombstone.join(METADATA_FILE), b"not a session").expect("tombstone content");
        assert!(
            store
                .list(None, 0, 10)
                .expect("listing")
                .sessions
                .is_empty()
        );
        assert_eq!(
            store
                .recover_interrupted_deletes()
                .expect("delete recovers"),
            1
        );
        assert!(!tombstone.exists());
    }

    #[test]
    fn handoff_publishes_complete_hydration_before_pointer_switch() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("handoff");
        let mut parent = store
            .create("parent", "/workspace", None, 1)
            .expect("parent session");
        parent
            .config
            .insert("model".to_owned(), Value::from("parent"));
        parent.agent_profile = Some(serde_json::json!({"name": "reviewer"}));
        parent.tools_available = vec![serde_json::json!({"name": "read_file"})];
        store
            .update_metadata(&parent)
            .expect("parent hydration metadata");

        let child = store
            .handoff_messages(
                &parent,
                "child",
                &[
                    ModelMessage::System {
                        content: "system".to_owned(),
                    },
                    user("complete"),
                ],
                2,
                true,
            )
            .expect("handoff");
        let hydrated = store.load("child").expect("published child loads");
        assert_eq!(hydrated.messages, [user("complete")]);
        assert_eq!(hydrated.metadata.config["model"], "parent");
        assert_eq!(hydrated.metadata.agent_profile, parent.agent_profile);
        assert_eq!(hydrated.metadata.tools_available, parent.tools_available);
        assert_eq!(child.id, "child");
        assert!(
            fs::read_dir(temporary.path())
                .expect("session root")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".handoff-"))
        );
    }

    #[test]
    fn handoff_journal_rolls_forward_after_publication_before_pointer_switch() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("recovery");
        let mut parent = store
            .create("parent", "/workspace", None, 1)
            .expect("parent session");
        parent
            .config
            .insert("model".to_owned(), serde_json::json!("parent"));
        store.update_metadata(&parent).expect("parent metadata");

        let staging_directory = ".handoff-crash-recovered";
        let destination_directory = session_directory_name(2, "recovered");
        let mut staged = store
            .initialize_session(
                staging_directory,
                "recovered",
                "/workspace",
                Some("parent".to_owned()),
                2,
            )
            .expect("staged child");
        staged.config = parent.config.clone();
        store
            .replace_messages(&mut staged, &[user("durable")], 2)
            .expect("complete staged transcript");
        let journal_path = store.handoff_journal_path().expect("journal path");
        store
            .write_handoff_journal(
                &journal_path,
                &HandoffJournal {
                    session_id: "recovered".to_owned(),
                    staging_directory: staging_directory.to_owned(),
                    destination_directory: destination_directory.clone(),
                },
            )
            .expect("durable handoff intent");
        fs::rename(
            temporary.path().join(staging_directory),
            temporary.path().join(&destination_directory),
        )
        .expect("directory was published before simulated crash");
        sync_directory(temporary.path()).expect("published directory");
        assert_eq!(
            store.read_pointer().expect("stale pointer").as_deref(),
            Some("parent")
        );

        let recovered = store
            .handoff_messages(&parent, "recovered", &[user("replacement")], 2, true)
            .expect("same handoff retry rolls forward");
        assert_eq!(recovered.id, "recovered");
        assert_eq!(
            store.load("recovered").expect("recovered child").messages,
            [user("durable")]
        );
        assert_eq!(
            store.read_pointer().expect("recovered pointer").as_deref(),
            Some("recovered")
        );
        assert!(!journal_path.exists());
    }

    #[test]
    fn child_creation_rejects_an_id_left_by_a_previous_process() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let first_process = SessionStore::new(temporary.path());
        first_process
            .create_child("child-restart", "/workspace", "parent".to_owned(), 1)
            .expect("first process child");
        let restarted_process = SessionStore::new(temporary.path());
        assert!(matches!(
            restarted_process.create_child(
                "child-restart",
                "/workspace",
                "parent".to_owned(),
                2,
            ),
            Err(StorageError::DuplicateSessionId(id)) if id == "child-restart"
        ));
        assert_eq!(
            restarted_process
                .list(None, 0, 10)
                .expect("unambiguous sessions")
                .sessions
                .len(),
            1
        );
    }
}
