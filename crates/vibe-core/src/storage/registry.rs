//! What every store opened over one save directory shares within a process.
//!
//! A store is opened from a directory in many places, most of which never see
//! the configuration, and the app server that opens a session and the driver
//! that runs its turns hold separate stores over the same directory. Two things
//! have to agree across all of them: the prefix the directory's sessions are
//! named with, registered once where the configuration is resolved, the way
//! the reference keys its listing index by save directory (`session_index.py`
//! `session_index_for`); and the sessions named there but not written yet,
//! which the reference keeps on a session's logger until its first save.
//!
//! Both live here, keyed by directory behind one lock. A pending session
//! leaves when its first write lands or when its owner discards it. A lock a
//! panicking thread poisoned answers every lookup as for a directory nothing
//! was registered for, and drops every update: the registry is a convenience
//! over what is on disk, never the record of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::{DEFAULT_SESSION_PREFIX, SessionMetadata};

#[derive(Default)]
struct SaveDirectory {
    /// The configured prefix, or [`None`] for the default one.
    prefix: Option<String>,
    /// Sessions named and not yet written, by identifier.
    pending: BTreeMap<String, SessionMetadata>,
}

fn directories() -> Option<MutexGuard<'static, BTreeMap<PathBuf, SaveDirectory>>> {
    static DIRECTORIES: OnceLock<Mutex<BTreeMap<PathBuf, SaveDirectory>>> = OnceLock::new();
    DIRECTORIES.get_or_init(Mutex::default).lock().ok()
}

/// Records the `session_prefix` sessions under `root` are named with.
pub fn register_session_prefix(root: &Path, prefix: &str) {
    let Some(mut directories) = directories() else {
        return;
    };
    let prefix = (prefix != DEFAULT_SESSION_PREFIX).then(|| prefix.to_owned());
    if prefix.is_some() || directories.contains_key(root) {
        directories.entry(root.to_path_buf()).or_default().prefix = prefix;
    }
}

/// The prefix sessions under `root` are named with.
pub(super) fn prefix(root: &Path) -> String {
    directories()
        .and_then(|directories| directories.get(root)?.prefix.clone())
        .unwrap_or_else(|| DEFAULT_SESSION_PREFIX.to_owned())
}

/// Keeps `metadata` where every store over `root` finds it until it is first
/// written.
pub(super) fn remember_pending(root: &Path, metadata: &SessionMetadata) {
    if let Some(mut directories) = directories() {
        directories
            .entry(root.to_path_buf())
            .or_default()
            .pending
            .insert(metadata.id.clone(), metadata.clone());
    }
}

/// The session named under `root` as `session_id` and not written yet.
pub(super) fn pending(root: &Path, session_id: &str) -> Option<SessionMetadata> {
    directories()?.get(root)?.pending.get(session_id).cloned()
}

/// Forgets the unwritten session `session_id` under `root`.
pub(super) fn discard_pending(root: &Path, session_id: &str) {
    if let Some(mut directories) = directories()
        && let Some(directory) = directories.get_mut(root)
    {
        directory.pending.remove(session_id);
    }
}
