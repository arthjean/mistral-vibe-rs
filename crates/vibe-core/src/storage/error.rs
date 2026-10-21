use std::path::PathBuf;

use thiserror::Error;

use crate::atomic_file::AtomicWriteError;

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
