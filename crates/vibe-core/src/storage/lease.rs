//! Who holds a session open.
//!
//! Reference `SessionLease` (`vibe/core/session/session_lease.py`): a process
//! that opens a session takes an exclusive advisory lock on
//! `<save_dir>/active/<id>.lock` and keeps it for as long as the session is its
//! own, so a second process asked to open the same session is refused rather
//! than left to interleave writes into one log. Beside the lock sits a
//! diagnostic, `<id>.lock.json`, naming the holder; a `.registry` lock in the
//! same directory serializes acquiring and releasing, so a release that
//! unlinks the two files never races an acquire that is about to reuse them.
//!
//! The on-disk format is shared: a lease this port takes is one the reference
//! honors, and the other way around, which is what lets the two run over one
//! save directory.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use thiserror::Error;

use crate::atomic_file::{self, open_private_lock};

/// The directory under the save directory that holds the leases.
pub const ACTIVE_DIRECTORY: &str = "active";
const REGISTRY_FILE: &str = ".registry";
const LEASE_VERSION: u32 = 1;

/// Why a lease could not be taken.
#[derive(Debug, Error)]
pub enum LeaseError {
    /// Another process holds the session. The sentence is the one the
    /// reference's `SessionBusyError` carries, which a client reads.
    #[error("Session is already open: {0}")]
    Busy(String),
    #[error("invalid session ID: {0:?}")]
    InvalidSessionId(String),
    #[error("session lease path cannot contain a symbolic link")]
    SymbolicLink,
    #[error("I/O failure at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A held lease, released when dropped.
#[derive(Debug)]
pub struct SessionLease {
    session_id: String,
    path: PathBuf,
    diagnostic_path: PathBuf,
    file: Option<File>,
}

impl SessionLease {
    /// Takes the lease on `session_id` under `root`, the save directory.
    ///
    /// # Errors
    ///
    /// [`LeaseError::Busy`] when another holder owns the session, and an I/O
    /// error when the lock or its diagnostic cannot be written; a lease whose
    /// diagnostic could not be published is not held.
    pub fn acquire(root: &Path, session_id: &str) -> Result<Self, LeaseError> {
        if !is_lease_session_id(session_id) {
            return Err(LeaseError::InvalidSessionId(session_id.to_owned()));
        }
        let directory = root.join(ACTIVE_DIRECTORY);
        if is_symlink(root) || is_symlink(&directory) {
            return Err(LeaseError::SymbolicLink);
        }
        atomic_file::ensure_private_directory(&directory).map_err(|source| LeaseError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = directory.join(format!("{session_id}.lock"));
        let diagnostic_path = directory.join(format!("{session_id}.lock.json"));
        let _registry = registry_lock(&directory)?;
        let file = open_private_lock(&path).map_err(|source| LeaseError::Io {
            path: path.clone(),
            source,
        })?;
        if let Err(error) = fs2::FileExt::try_lock_exclusive(&file) {
            return Err(
                if error.kind() == fs2::lock_contended_error().kind()
                    || error.kind() == std::io::ErrorKind::WouldBlock
                {
                    LeaseError::Busy(session_id.to_owned())
                } else {
                    LeaseError::Io {
                        path,
                        source: error,
                    }
                },
            );
        }
        let mut lease = Self {
            session_id: session_id.to_owned(),
            path,
            diagnostic_path,
            file: Some(file),
        };
        if let Err(source) = lease.publish_diagnostic() {
            // A lease that cannot say who holds it is not held.
            let path = lease.diagnostic_path.clone();
            lease.unlock();
            let _ = fs::remove_file(&lease.path);
            return Err(LeaseError::Io { path, source });
        }
        Ok(lease)
    }

    /// The session this lease holds.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn publish_diagnostic(&self) -> std::io::Result<()> {
        // Compact, sorted, one line: what the reference writes, so a reader of
        // either implementation parses the other's.
        let diagnostic = json!({
            "acquired_at": crate::storage::time::format_lease_timestamp(crate::clock::now_millis()),
            "lease_version": LEASE_VERSION,
            "process_id": std::process::id(),
            "session_id": self.session_id,
        });
        let mut encoded = serde_json::to_vec(&diagnostic)?;
        encoded.push(b'\n');
        let mut file = open_private_lock(&self.diagnostic_path)?;
        file.set_len(0)?;
        file.write_all(&encoded)?;
        file.sync_all()
    }

    fn unlock(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = fs2::FileExt::unlock(&file);
        }
    }

    /// Lets the session go, removing the lock and its diagnostic. A file that
    /// cannot be removed is left behind: the next holder reuses the lock and
    /// overwrites the diagnostic.
    pub fn release(mut self) {
        self.release_in_place();
    }

    fn release_in_place(&mut self) {
        if self.file.is_none() {
            return;
        }
        let directory = self
            .path
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf);
        let registry = registry_lock(&directory).ok();
        self.unlock();
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.diagnostic_path);
        drop(registry);
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.release_in_place();
    }
}

/// The blocking lock that serializes every acquire and release in `directory`.
fn registry_lock(directory: &Path) -> Result<atomic_file::FileLock, LeaseError> {
    let path = directory.join(REGISTRY_FILE);
    atomic_file::FileLock::acquire(&path).map_err(|source| LeaseError::Io { path, source })
}

/// Reference `_SESSION_ID_PATTERN`: an alphanumeric first character, then up
/// to 127 alphanumerics, underscores or hyphens.
fn is_lease_session_id(session_id: &str) -> bool {
    let mut characters = session_id.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && session_id.len() <= 128
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}
