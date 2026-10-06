//! The per-repository record of the versions each one keeps in the shared
//! store.
//!
//! Reference `vibe/core/skills/registry/_ledger.py`. The store under the Vibe
//! home is shared by every repository on the machine, but one sync only reads
//! its own manifests; pruning against those alone would evict a version a
//! sibling repository still pins. Each repository, keyed by its resolved
//! project roots or `global` when it has none, records the versions it keeps
//! active in a file of its own, and a prune keeps the union of every record.
//! A repository rewrites its own record on every sync, so a dropped pin leaves
//! the union unless another repository still holds it.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use super::manifest::resolve_lenient;
use super::store;

/// The key the global manifest's pins are recorded under.
pub const GLOBAL_KEY: &str = "global";

/// A stable key for the repository `roots` open: `global` with no roots, else
/// the SHA-1 of their resolved, sorted spellings joined by newlines (reference
/// `repo_key`).
#[must_use]
pub fn repo_key(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return GLOBAL_KEY.to_owned();
    }
    let mut parts = roots
        .iter()
        .map(|root| resolve_lenient(root).to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    parts.sort();
    crate::text::hex_encode(&Sha1::digest(parts.join("\n").as_bytes()))
}

/// Where the records live under a Vibe home.
#[must_use]
pub fn ledger_root(vibe_home: &Path) -> PathBuf {
    store::cache_dir(vibe_home).join("ledger")
}

/// Replaces `key`'s record with `active`; an empty set removes the record, so
/// a repository that pins nothing stops holding versions in the union.
///
/// # Errors
///
/// A record that cannot be written. The write goes through a temporary file
/// renamed into place, so a reader never sees half a record.
pub fn record(vibe_home: &Path, key: &str, active: &BTreeSet<(String, i64)>) -> io::Result<()> {
    let root = ledger_root(vibe_home);
    let target = root.join(format!("{key}.txt"));
    if active.is_empty() {
        return match fs::remove_file(&target) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    fs::create_dir_all(&root)?;
    let mut lines = active
        .iter()
        .map(|(skill_id, version)| format!("{skill_id}@{version}"))
        .collect::<Vec<_>>();
    lines.sort();
    let staged = root.join(format!(
        "{key}.{}.tmp",
        crate::session_id::generate_session_id(None)
    ));
    if let Err(error) = fs::write(&staged, lines.join("\n")) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    fs::rename(&staged, &target).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })
}

/// The union of every recorded repository's active set (reference `union`).
/// An unreadable record and a malformed line are skipped.
#[must_use]
pub fn union(vibe_home: &Path) -> BTreeSet<(String, i64)> {
    let mut out = BTreeSet::new();
    let Ok(entries) = fs::read_dir(ledger_root(vibe_home)) else {
        return out;
    };
    let mut records = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .collect::<Vec<_>>();
    records.sort();
    for path in records {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for line in body.lines() {
            let Some((skill_id, version)) = line.trim().rsplit_once('@') else {
                continue;
            };
            if skill_id.is_empty()
                || version.is_empty()
                || !version.chars().all(|character| character.is_ascii_digit())
            {
                continue;
            }
            if let Ok(version) = version.parse::<i64>() {
                out.insert((skill_id.to_owned(), version));
            }
        }
    }
    out
}
