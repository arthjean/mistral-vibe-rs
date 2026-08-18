//! Committing a batch of configuration files, all of them or none.
//!
//! A configuration write can span the user file and a project file at once, and
//! a process that dies between the two must not leave the pair disagreeing. So
//! every write is staged beside its destination, a journal records the staged
//! set, the swaps happen only once every stage succeeded, and a journal found
//! at startup is rolled forward or rolled back before anything is read.
//!
//! Nothing here knows what a configuration document means. It moves bytes into
//! place atomically and answers for the crash windows between them, which is
//! why it is separable from the layering that produces those bytes.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ConfigError, ConfigPaths, LOCK_FILE, TRANSACTION_FILE};
use crate::atomic_file::{self, create_private_file, write_atomically};

pub(super) struct PreparedWrite {
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
    had_original: bool,
    pub(super) cleanup_on_drop: bool,
}

impl PreparedWrite {
    pub(super) fn new(destination: PathBuf, bytes: Vec<u8>) -> Result<Self, ConfigError> {
        let parent = destination
            .parent()
            .ok_or_else(|| ConfigError::InvalidPath(destination.clone()))?;
        ensure_private_directory(parent)?;
        let token = random_sidecar_token()?;
        let temporary = parent.join(format!(".config.{token}.tmp"));
        let backup = parent.join(format!(".config.{token}.bak"));
        let mut file = create_private_file(&temporary).map_err(|source| ConfigError::Io {
            path: temporary.clone(),
            source,
        })?;
        let prepared = Self {
            had_original: destination.is_file(),
            destination,
            temporary,
            backup,
            cleanup_on_drop: true,
        };
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Io {
                path: prepared.temporary.clone(),
                source,
            })?;
        Ok(prepared)
    }

    pub(super) fn journal_entry(&self) -> JournalEntry {
        JournalEntry {
            destination: self.destination.clone(),
            temporary: self.temporary.clone(),
            backup: self.backup.clone(),
            had_original: self.had_original,
        }
    }
}

impl Drop for PreparedWrite {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum JournalState {
    Prepared,
    Committed,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ConfigJournal {
    pub(super) state: JournalState,
    pub(super) entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct JournalEntry {
    pub(super) destination: PathBuf,
    pub(super) temporary: PathBuf,
    pub(super) backup: PathBuf,
    pub(super) had_original: bool,
}

pub(super) fn commit_prepared(prepared: &[PreparedWrite]) -> Result<(), ConfigError> {
    for item in prepared {
        if item.had_original {
            fs::rename(&item.destination, &item.backup).map_err(|source| ConfigError::Io {
                path: item.destination.clone(),
                source,
            })?;
        }
        fs::rename(&item.temporary, &item.destination).map_err(|source| ConfigError::Io {
            path: item.destination.clone(),
            source,
        })?;
        if let Some(parent) = item.destination.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

pub(super) fn rollback_prepared(prepared: &[PreparedWrite]) -> Result<(), ConfigError> {
    for item in prepared.iter().rev() {
        if item.backup.exists() {
            if item.destination.exists() {
                fs::remove_file(&item.destination).map_err(|source| ConfigError::Io {
                    path: item.destination.clone(),
                    source,
                })?;
            }
            fs::rename(&item.backup, &item.destination).map_err(|source| ConfigError::Io {
                path: item.destination.clone(),
                source,
            })?;
        } else if !item.had_original && item.destination.exists() {
            fs::remove_file(&item.destination).map_err(|source| ConfigError::Io {
                path: item.destination.clone(),
                source,
            })?;
        }
        if item.temporary.exists() {
            fs::remove_file(&item.temporary).map_err(|source| ConfigError::Io {
                path: item.temporary.clone(),
                source,
            })?;
        }
        if let Some(parent) = item.destination.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

pub(super) fn cleanup_prepared(prepared: &[PreparedWrite]) -> Result<(), ConfigError> {
    for item in prepared {
        if item.backup.exists() {
            fs::remove_file(&item.backup).map_err(|source| ConfigError::Io {
                path: item.backup.clone(),
                source,
            })?;
        }
        if item.temporary.exists() {
            fs::remove_file(&item.temporary).map_err(|source| ConfigError::Io {
                path: item.temporary.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

pub(super) fn recover_transaction(paths: &ConfigPaths) -> Result<(), ConfigError> {
    let path = paths.vibe_home.join(TRANSACTION_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.clone(),
                source,
            });
        }
    };
    let journal: ConfigJournal =
        serde_json::from_slice(&bytes).map_err(|source| ConfigError::CorruptJournal {
            path: path.clone(),
            source,
        })?;
    validate_journal(&journal, paths)?;
    match journal.state {
        JournalState::Prepared => {
            let recovered = journal
                .entries
                .into_iter()
                .map(|entry| PreparedWrite {
                    destination: entry.destination,
                    temporary: entry.temporary,
                    backup: entry.backup,
                    had_original: entry.had_original,
                    cleanup_on_drop: false,
                })
                .collect::<Vec<_>>();
            rollback_prepared(&recovered)?;
        }
        JournalState::Committed => {
            for entry in journal.entries {
                if entry.backup.exists() {
                    fs::remove_file(&entry.backup).map_err(|source| ConfigError::Io {
                        path: entry.backup.clone(),
                        source,
                    })?;
                }
                if entry.temporary.exists() {
                    fs::remove_file(&entry.temporary).map_err(|source| ConfigError::Io {
                        path: entry.temporary.clone(),
                        source,
                    })?;
                }
            }
        }
    }
    fs::remove_file(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    sync_directory(&paths.vibe_home)
}

pub(super) fn cleanup_orphan_sidecars(paths: &ConfigPaths) -> Result<(), ConfigError> {
    let parents = [paths.user_config(), paths.project_config()]
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>();
    for parent in parents {
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ConfigError::Io {
                    path: parent,
                    source,
                });
            }
        };
        let mut removed = false;
        for entry in entries {
            let entry = entry.map_err(|source| ConfigError::Io {
                path: parent.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let token = name.strip_prefix(".config.").and_then(|name| {
                name.strip_suffix(".tmp")
                    .or_else(|| name.strip_suffix(".bak"))
            });
            if token.is_none_or(|token| {
                token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|source| ConfigError::Io {
                path: entry.path(),
                source,
            })?;
            removed = true;
        }
        if removed {
            sync_directory(&parent)?;
        }
    }
    Ok(())
}

pub(super) fn random_sidecar_token() -> Result<String, ConfigError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ConfigError::RandomUnavailable)?;
    Ok(format!("{:032x}", u128::from_ne_bytes(bytes)))
}

fn validate_journal(journal: &ConfigJournal, paths: &ConfigPaths) -> Result<(), ConfigError> {
    let allowed = [paths.user_config(), paths.project_config()];
    for entry in &journal.entries {
        if !allowed.contains(&entry.destination)
            || entry.temporary == entry.backup
            || !transaction_sidecar(&entry.temporary, &entry.destination, "tmp")
            || !transaction_sidecar(&entry.backup, &entry.destination, "bak")
        {
            return Err(ConfigError::UnsafeJournal(path_for_journal(paths)));
        }
    }
    Ok(())
}

fn path_for_journal(paths: &ConfigPaths) -> PathBuf {
    paths.vibe_home.join(TRANSACTION_FILE)
}

fn transaction_sidecar(path: &Path, destination: &Path, suffix: &str) -> bool {
    if path.parent() != destination.parent() {
        return false;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".config.") && name.ends_with(&format!(".{suffix}")))
}

pub(super) fn write_journal(path: &Path, journal: &ConfigJournal) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::InvalidPath(path.to_path_buf()))?;
    ensure_private_directory(parent)?;
    let mut encoded = serde_json::to_vec(journal).map_err(ConfigError::Json)?;
    encoded.push(b'\n');
    write_atomically(path, "journal", &encoded).map_err(ConfigError::from)
}

/// The advisory lock every configuration read and write is serialized behind.
///
/// The guard itself is [`atomic_file::FileLock`]; what this adds is the
/// configuration's own error vocabulary and the knowledge of where the lock
/// file lives.
pub(super) struct ConfigFileLock(
    /// Held for its `Drop`, which is what releases the lock.
    #[expect(dead_code, reason = "the guard's whole job is its drop")]
    atomic_file::FileLock,
);

impl ConfigFileLock {
    pub(super) fn acquire(vibe_home: &Path) -> Result<Self, ConfigError> {
        let path = vibe_home.join(LOCK_FILE);
        atomic_file::FileLock::acquire(&path)
            .map(Self)
            .map_err(|source| ConfigError::Io { path, source })
    }
}

pub(super) fn ensure_private_directory(path: &Path) -> Result<(), ConfigError> {
    atomic_file::ensure_private_directory(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn sync_directory(path: &Path) -> Result<(), ConfigError> {
    atomic_file::sync_directory(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}
