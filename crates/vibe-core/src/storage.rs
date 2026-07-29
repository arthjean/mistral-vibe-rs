use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::events::ModelMessage;

const METADATA_FILE: &str = "meta.json";
const MESSAGES_FILE: &str = "messages.jsonl";
const LAST_SESSION_DIRECTORY: &str = ".last_session";
const MAX_MESSAGE_RECORD_BYTES: usize = 8 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
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
        fs::create_dir_all(&self.root).map_err(|source| StorageError::Io {
            path: self.root.clone(),
            source,
        })?;
        let directory = session_directory_name(now_ms, id);
        let session_path = self.root.join(&directory);
        fs::create_dir(&session_path).map_err(|source| StorageError::Io {
            path: session_path.clone(),
            source,
        })?;
        File::create(session_path.join(MESSAGES_FILE)).map_err(|source| StorageError::Io {
            path: session_path.join(MESSAGES_FILE),
            source,
        })?;
        let start_time = format_iso_timestamp(now_ms);
        let mut environment = BTreeMap::new();
        environment.insert(
            "working_directory".to_owned(),
            Some(working_directory.to_owned()),
        );
        let metadata = SessionMetadata {
            id: id.to_owned(),
            directory: directory.clone(),
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
        self.write_pointer(id)?;
        Ok(metadata)
    }

    pub fn append_message(
        &self,
        metadata: &mut SessionMetadata,
        message: &ModelMessage,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        if matches!(message, ModelMessage::System { .. }) {
            metadata.system_prompt =
                Some(serde_json::to_value(message).map_err(StorageError::Json)?);
            metadata.updated_at_ms = now_ms;
            metadata.end_time = Some(format_iso_timestamp(now_ms));
            return self.write_metadata(metadata);
        }
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        let encoded = serde_json::to_vec(message).map_err(StorageError::Json)?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(&encoded)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_data())
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
        metadata.message_count = metadata.message_count.saturating_add(1);
        metadata.last_message_fingerprint = Some(message_fingerprint(message)?);
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_timestamp(now_ms));
        self.write_metadata(metadata)
    }

    pub fn replace_messages(
        &self,
        metadata: &mut SessionMetadata,
        messages: &[ModelMessage],
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let path = self.session_path(metadata).join(MESSAGES_FILE);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .session_path(metadata)
            .join(format!(".messages.{sequence}.tmp"));
        let non_system_messages = messages
            .iter()
            .filter(|message| !matches!(message, ModelMessage::System { .. }));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| StorageError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            for message in non_system_messages {
                let encoded = serde_json::to_vec(message).map_err(StorageError::Json)?;
                file.write_all(&encoded)
                    .and_then(|()| file.write_all(b"\n"))
                    .map_err(|source| StorageError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            }
            file.sync_all().map_err(|source| StorageError::Io {
                path: temporary.clone(),
                source,
            })?;
            fs::rename(&temporary, &path).map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
            sync_directory(&self.session_path(metadata))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            return result;
        }
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
        let prompt = current_system_prompt.into();
        if let Some(pointer) = self.read_pointer()? {
            if let Ok(metadata) = self.resolve(&pointer)
                && same_working_directory(&metadata.working_directory, working_directory)
            {
                return self.resume(&metadata.id, prompt, current_config);
            }
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
            let exact_path = directory == selector;
            let metadata = self.read_metadata_from_directory(&directory);
            let exact_metadata = metadata
                .as_ref()
                .is_ok_and(|metadata| metadata.id == selector);
            let candidate_by_metadata = metadata
                .as_ref()
                .is_ok_and(|metadata| exact_metadata || metadata.id.starts_with(selector));
            let candidate_by_path = directory.ends_with(&format!("_{}", short_id(selector)));
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
            if let Ok(item) =
                self.read_metadata_from_directory(&entry.file_name().to_string_lossy())
            {
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
        let session_path = self.session_path(metadata);
        let path = session_path.join(METADATA_FILE);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = session_path.join(format!(".meta.{sequence}.tmp"));
        let encoded = serde_json::to_vec_pretty(metadata).map_err(StorageError::Json)?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| StorageError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&encoded)
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|source| StorageError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, &path).map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
            sync_directory(&session_path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn write_pointer(&self, session_id: &str) -> Result<(), StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(());
        };
        let pointer_directory = self.root.join(LAST_SESSION_DIRECTORY);
        fs::create_dir_all(&pointer_directory).map_err(|source| StorageError::Io {
            path: pointer_directory.clone(),
            source,
        })?;
        let path = pointer_directory.join(pointer_key);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = pointer_directory.join(format!(".pointer.{sequence}.tmp"));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| StorageError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            writeln!(file, "{session_id}")
                .and_then(|()| file.sync_all())
                .map_err(|source| StorageError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, &path).map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
            sync_directory(&pointer_directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
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
}

fn sync_directory(path: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid session ID `{0}`")]
    InvalidSessionId(String),
    #[error("session `{0}` was not found")]
    SessionNotFound(String),
    #[error("session selector `{0}` is ambiguous")]
    AmbiguousSession(String),
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
    #[error("invalid message record {line} at `{path}`: {message}")]
    CorruptMessages {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("message count mismatch: metadata declares {expected}, log contains {actual}")]
    MessageCountMismatch { expected: u64, actual: u64 },
    #[error("JSON serialization failed: {0}")]
    Json(serde_json::Error),
}

fn validate_session_id(id: &str) -> Result<(), StorageError> {
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

fn session_directory_name(now_ms: u64, id: &str) -> String {
    format!(
        "session_{}_{}",
        format_compact_timestamp(now_ms),
        short_id(id)
    )
}

fn short_id(id: &str) -> &str {
    id.get(..id.len().min(8)).unwrap_or(id)
}

fn message_fingerprint(message: &ModelMessage) -> Result<String, StorageError> {
    let value = serde_json::to_value(message).map_err(StorageError::Json)?;
    let encoded = python_canonical_json(&value);
    let digest = Sha256::digest(encoded.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

fn python_canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => python_json_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_canonical_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        format!(
                            "{}: {}",
                            python_json_string(key),
                            python_canonical_json(value)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn python_json_string(value: &str) -> String {
    let mut encoded = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\u{0008}' => encoded.push_str("\\b"),
            '\u{000C}' => encoded.push_str("\\f"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            character if character <= '\u{001F}' => {
                use std::fmt::Write as _;
                let _ = write!(encoded, "\\u{:04x}", u32::from(character));
            }
            character if character.is_ascii() => encoded.push(character),
            character => {
                use std::fmt::Write as _;
                let mut units = [0_u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    let _ = write!(encoded, "\\u{unit:04x}");
                }
            }
        }
    }
    encoded.push('"');
    encoded
}

fn default_title_source() -> String {
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

fn format_iso_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = timestamp_parts(milliseconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}000+00:00")
}

fn format_compact_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, _) = timestamp_parts(milliseconds);
    format!("{year:04}{month:02}{day:02}_{hour:02}{minute:02}{second:02}")
}

fn timestamp_sort_key(value: &str) -> Option<u64> {
    let digits: String = value
        .chars()
        .filter(char::is_ascii_digit)
        .take(17)
        .collect();
    (digits.len() == 17)
        .then(|| digits.parse::<u64>().ok())
        .flatten()
}

fn timestamp_parts(milliseconds: u64) -> (i64, u64, u64, u64, u64, u64, u64) {
    let seconds = milliseconds / 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
        milliseconds % 1_000,
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let days = days_since_epoch.saturating_add(719_468);
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u64::try_from(month).unwrap_or_default(),
        u64::try_from(day).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> ModelMessage {
        ModelMessage::User {
            content: content.to_owned(),
        }
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
}
