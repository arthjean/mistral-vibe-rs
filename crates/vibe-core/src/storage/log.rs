//! `messages.jsonl`: a session's append-only message log.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use serde_json::Value;

use super::{
    HydratedSession, MAX_MESSAGE_RECORD_BYTES, MESSAGES_FILE, SessionMetadata, SessionStore,
    StorageError, create_private_directory, ensure_private_directory, record,
};
use crate::atomic_file::{FileMode, create_private_file, write_atomically};
use crate::events::ModelMessage;
use crate::pycompat::{self, Dumps};
use crate::text::sha256_hex;
use crate::time::format_iso_millis;

impl SessionStore {
    /// Writes the session with an empty log, which is how a subagent's session
    /// exists before its first message. Reference `persist_empty_session`.
    pub fn persist_empty(
        &self,
        metadata: &mut SessionMetadata,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        self.replace_messages(metadata, &[], now_ms)
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
    /// A session with nothing but a system message is not written at all.
    pub fn append_messages(
        &self,
        metadata: &mut SessionMetadata,
        messages: &[ModelMessage],
        now_ms: u64,
    ) -> Result<(), StorageError> {
        if messages.is_empty() {
            return Ok(());
        }
        let appended = LogAppend::encode(messages, metadata)?;
        if appended.encoded.is_empty() && !metadata.is_persisted(self) {
            return Ok(());
        }
        self.materialize(metadata)?;
        appended.write(&self.session_path(metadata).join(MESSAGES_FILE), metadata)?;
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_millis(now_ms));
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
        write_atomically(&path, "messages", &encoded, FileMode::Private)?;
        // The reference's message list always opens with the system prompt; a
        // list stored here without one keeps the prompt already recorded.
        if let Some(record) = messages
            .iter()
            .find(|message| matches!(message, ModelMessage::System { .. }))
            .map(record::system_prompt)
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
        metadata.end_time = Some(format_iso_millis(now_ms));
        self.write_metadata(metadata)
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

    pub(super) fn read_messages(
        &self,
        metadata: &SessionMetadata,
    ) -> Result<Vec<ModelMessage>, StorageError> {
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
                record::from_line(&line).map_err(|message| StorageError::CorruptMessages {
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
}

/// What one batch of messages appends to a log: the records of every message
/// but the system ones, how many there are, and the last of them.
///
/// A system message never reaches the log: it replaces the prompt `meta.json`
/// records, because only the current one is ever replayed.
pub(super) struct LogAppend<'a> {
    encoded: Vec<u8>,
    count: u64,
    last: Option<&'a ModelMessage>,
}

impl<'a> LogAppend<'a> {
    /// Encodes `messages`, recording the system prompt among them on
    /// `metadata`.
    pub(super) fn encode(
        messages: &'a [ModelMessage],
        metadata: &mut SessionMetadata,
    ) -> Result<Self, StorageError> {
        let mut appended = Self {
            encoded: Vec::new(),
            count: 0,
            last: None,
        };
        for message in messages {
            if matches!(message, ModelMessage::System { .. }) {
                metadata.system_prompt = Some(record::system_prompt(message));
                continue;
            }
            appended.encoded.extend(encode_message(message)?);
            appended.encoded.push(b'\n');
            appended.count = appended.count.saturating_add(1);
            appended.last = Some(message);
        }
        Ok(appended)
    }

    /// Appends the records to the log at `path` in one synced write, and
    /// counts them and fingerprints the last on `metadata`.
    pub(super) fn write(
        self,
        path: &Path,
        metadata: &mut SessionMetadata,
    ) -> Result<(), StorageError> {
        if !self.encoded.is_empty() {
            let io_error = |source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            };
            let mut file = OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(io_error)?;
            file.write_all(&self.encoded)
                .and_then(|()| file.sync_data())
                .map_err(io_error)?;
        }
        metadata.message_count = metadata.message_count.saturating_add(self.count);
        if let Some(last) = self.last {
            metadata.last_message_fingerprint = Some(message_fingerprint(last)?);
        }
        Ok(())
    }
}

pub(super) fn message_fingerprint(message: &ModelMessage) -> Result<String, StorageError> {
    let value = message_record(message)?;
    let payload = pycompat::dumps(
        &value,
        Dumps {
            sort_keys: true,
            ..Dumps::DEFAULT
        },
    );
    Ok(sha256_hex(payload.as_bytes()))
}

/// The record a message is written to the log as.
fn message_record(message: &ModelMessage) -> Result<Value, StorageError> {
    record::to_value(message).map_err(StorageError::Json)
}

fn encode_message(message: &ModelMessage) -> Result<Vec<u8>, StorageError> {
    record::to_line(message).map_err(StorageError::Json)
}
