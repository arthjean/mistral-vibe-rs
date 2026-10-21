//! The JSON files the caches keyed by a credential's digest live in.
//!
//! The eval cache, the account cache and the fast-model verdicts each keep one
//! JSON object under the vibe home, keyed by an anonymous digest of the
//! credential, and the reference reads and writes the three alike: a file that
//! does not read as an object holds no entry, and a write replaces the whole
//! document with its compact `json.dump` through a sibling file. The files are
//! shared with every other client running under the same vibe home, the Python
//! reference included, so the text is the reference's to the byte.
//!
//! The eval and account caches also share the entry layout, which
//! [`TimedEntries`] reads and writes: the second an entry was stored at and the
//! payload it records, applied while it is recent enough.
//!
//! Reference `_read_entries` and `_write_entries` in
//! `vibe/core/experiments/cache.py` and `vibe/core/llm/model_probe.py`, and
//! `_read_whoami_entries` and `_write_whoami_entries` in
//! `vibe/setup/auth/whoami.py`, at the pinned commit.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::atomic_file::{FileMode, write_atomically};
use crate::pycompat::{self, Dumps};

/// The key an entry records the second it was stored at.
const STORED_AT_KEY: &str = "stored_at_timestamp";
/// The key an entry records its payload under.
const PAYLOAD_KEY: &str = "payload";

/// Every entry the file at `path` holds, in the order it holds them, or none
/// when it does not read as a JSON object.
pub(crate) fn read_entries(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(entries) => Some(entries),
            _ => None,
        })
        .unwrap_or_default()
}

/// Replaces the file at `path` with `entries`, written as
/// `json.dump(entries, separators=(",", ":"))` writes them, staged beside it
/// under `staging` and moved into place, so a reader never sees half a
/// document. The entries are keyed by credential, so the file is owner-only.
///
/// Answers whether the file was replaced; a write that fails leaves the
/// previous file in place.
pub(crate) fn write_entries(path: &Path, staging: &str, entries: Map<String, Value>) -> bool {
    let text = pycompat::dumps(
        &Value::Object(entries),
        Dumps {
            separators: Some((",", ":")),
            ..Dumps::DEFAULT
        },
    );
    path.parent()
        .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        && write_atomically(path, staging, text.as_bytes(), FileMode::Private).is_ok()
}

/// A cache file whose entries each record a payload and the second it was
/// stored at, and are applied for `ttl_seconds` after that second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimedEntries {
    path: PathBuf,
    /// The prefix of the staging file a write goes through.
    staging: &'static str,
    ttl_seconds: i64,
}

impl TimedEntries {
    pub(crate) fn new(path: PathBuf, staging: &'static str, ttl_seconds: i64) -> Self {
        Self {
            path,
            staging,
            ttl_seconds,
        }
    }

    /// Where the entries are read from and written to.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The payload stored under `key`, while it is recent as of `now`, in
    /// seconds since the epoch.
    pub(crate) fn payload(&self, key: &str, now: i64) -> Option<Map<String, Value>> {
        let entry = read_entries(&self.path).shift_remove(key)?;
        self.fresh_payload(entry, now)
    }

    /// Every payload recent as of `now`, in the order the file holds them,
    /// whichever credential stored it.
    pub(crate) fn payloads(&self, now: i64) -> impl Iterator<Item = Map<String, Value>> + '_ {
        read_entries(&self.path)
            .into_iter()
            .filter_map(move |(_, entry)| self.fresh_payload(entry, now))
    }

    /// Records `payload` under `key` as of `now`, keeping every other entry
    /// the file holds in its place, and an existing `key` in its own.
    pub(crate) fn store(&self, key: String, payload: Value, now: i64) {
        let mut entries = read_entries(&self.path);
        let mut entry = Map::new();
        entry.insert(STORED_AT_KEY.to_owned(), Value::from(now));
        entry.insert(PAYLOAD_KEY.to_owned(), payload);
        entries.insert(key, Value::Object(entry));
        write_entries(&self.path, self.staging, entries);
    }

    /// Drops the entry stored under `key`, rewriting the file only when it
    /// held one; the others keep their order, as `dict.pop` keeps it.
    pub(crate) fn remove(&self, key: &str) {
        let mut entries = read_entries(&self.path);
        if entries.shift_remove(key).is_some() {
            write_entries(&self.path, self.staging, entries);
        }
    }

    /// An entry's payload, when the entry is an object whose stored second is
    /// an integer within the window and whose payload is an object.
    ///
    /// The stored second reads as Python's `isinstance(value, int)` reads
    /// one: an integer, or a boolean, which Python counts as one, and never a
    /// float.
    fn fresh_payload(&self, entry: Value, now: i64) -> Option<Map<String, Value>> {
        let Value::Object(mut entry) = entry else {
            return None;
        };
        let stored_at = match entry.get(STORED_AT_KEY)? {
            Value::Bool(flag) => i64::from(*flag),
            Value::Number(number) => number.as_i64()?,
            _ => return None,
        };
        let Value::Object(payload) = entry.shift_remove(PAYLOAD_KEY)? else {
            return None;
        };
        let stale = i128::from(stored_at) <= i128::from(now) - i128::from(self.ttl_seconds);
        (!stale).then_some(payload)
    }
}
