//! The listing cache.
//!
//! Reference `SessionIndex` (`vibe/core/session/session_index.py`): listing
//! every saved session would read every `meta.json`, so a summary of each is
//! kept in `<save_dir>/.session_index.json`, keyed by directory name and
//! stamped with the metadata file's modification time. A listing stats each
//! session directory and re-reads only those whose metadata changed; a cache it
//! cannot parse is rebuilt from the directories, which stay the source of truth.
//! The file's layout is the reference's, so either implementation reuses the
//! other's cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::time::format_iso_micros;
use super::{MESSAGES_FILE, METADATA_FILE, SessionStore, StorageError, normalize_iso_utc};
use crate::atomic_file::write_atomically;

const INDEX_FILE: &str = ".session_index.json";
/// Reference `_PREVIEW_MAX_CHARS`.
const PREVIEW_MAX_CHARS: usize = 200;

/// Reference `SessionInfo`: what a listing knows of one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// Where the session works now.
    pub cwd: String,
    /// Where it began, when the record names it.
    pub origin_directory: Option<String>,
    pub parent_session_id: Option<String>,
    pub title: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub bumped_at: Option<String>,
    pub pinned_at: Option<String>,
    /// The end time, else the start time, else the metadata's modification
    /// time, always in UTC: what a listing sorts by.
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    mtime_ns: u64,
    #[serde(flatten)]
    info: SessionInfo,
}

/// Reference `SessionLoader.list_sessions`.
pub(super) fn list(
    store: &SessionStore,
    cwd: Option<&str>,
) -> Result<Vec<SessionInfo>, StorageError> {
    let mut sessions = reconcile(store)?
        .into_values()
        .map(|entry| entry.info)
        .filter(|info| {
            cwd.is_none_or(|cwd| info.cwd == cwd || info.origin_directory.as_deref() == Some(cwd))
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(sessions)
}

/// Reference `SessionIndex._reconcile`: the cache brought in line with the
/// directories, and rewritten when that changed anything.
fn reconcile(store: &SessionStore) -> Result<BTreeMap<String, Entry>, StorageError> {
    let root = store.root();
    if !root.is_dir() {
        return Ok(BTreeMap::new());
    }
    let index_path = root.join(INDEX_FILE);
    let mut entries = load_persisted(&index_path);
    let mut dirty = false;
    let mut seen = Vec::new();
    for directory in store.session_directories()? {
        let session_path = root.join(&directory);
        let Some(mtime_ns) = modified_ns(&session_path.join(METADATA_FILE)) else {
            continue;
        };
        seen.push(directory.clone());
        if entries
            .get(&directory)
            .is_some_and(|entry| entry.mtime_ns == mtime_ns)
        {
            continue;
        }
        match read_entry(&session_path, mtime_ns) {
            Some(entry) => {
                entries.insert(directory, entry);
                dirty = true;
            }
            None => {
                if entries.remove(&directory).is_some() {
                    dirty = true;
                }
            }
        }
    }
    let before = entries.len();
    entries.retain(|name, _| seen.contains(name));
    dirty |= entries.len() != before;
    if dirty {
        // The cache is advisory: a listing that cannot rewrite it still lists.
        if let Ok(encoded) = serde_json::to_vec(&entries) {
            let _ = write_atomically(&index_path, "session-index", &encoded);
        }
    }
    Ok(entries)
}

/// Reference `SessionIndex._load_persisted`: one malformed record discards
/// the whole cache.
fn load_persisted(path: &Path) -> BTreeMap<String, Entry> {
    let Ok(bytes) = fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(raw)) = serde_json::from_slice::<Value>(&bytes) else {
        return BTreeMap::new();
    };
    let mut entries = BTreeMap::new();
    for (name, payload) in raw {
        let Some(entry) = entry_from_payload(&payload) else {
            return BTreeMap::new();
        };
        entries.insert(name, entry);
    }
    entries
}

/// Reference `_entry_from_payload`.
fn entry_from_payload(payload: &Value) -> Option<Entry> {
    let object = payload.as_object()?;
    let mtime_ns = object.get("mtime_ns")?.as_u64()?;
    let session_id = object.get("session_id")?.as_str()?.to_owned();
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    Some(Entry {
        mtime_ns,
        info: SessionInfo {
            session_id,
            cwd: text("cwd").unwrap_or_default(),
            origin_directory: text("origin_directory"),
            parent_session_id: text("parent_session_id"),
            title: text("title"),
            start_time: text("start_time"),
            end_time: text("end_time"),
            bumped_at: text("bumped_at"),
            pinned_at: text("pinned_at"),
            updated_at: text("updated_at")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| mtime_to_utc_iso(mtime_ns)),
        },
    })
}

/// Reference `SessionIndex._read_entry` and `_build_info`.
fn read_entry(session_path: &Path, mtime_ns: u64) -> Option<Entry> {
    let log_size = fs::metadata(session_path.join(MESSAGES_FILE)).ok()?.len();
    let bytes = fs::read(session_path.join(METADATA_FILE)).ok()?;
    let Value::Object(metadata) = serde_json::from_slice::<Value>(&bytes).ok()? else {
        return None;
    };
    // An empty log lists only when the session recorded no messages; any other
    // empty log is an interrupted write a resume would refuse.
    if log_size == 0 && metadata.get("total_messages") != Some(&Value::from(0)) {
        return None;
    }
    let session_id = metadata
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_owned();
    let normalized = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .and_then(normalize_iso_utc)
    };
    let start_time = normalized("start_time");
    let end_time = normalized("end_time");
    let updated_at = end_time
        .clone()
        .or_else(|| start_time.clone())
        .unwrap_or_else(|| mtime_to_utc_iso(mtime_ns));
    Some(Entry {
        mtime_ns,
        info: SessionInfo {
            session_id,
            cwd: metadata
                .get("environment")
                .and_then(|environment| environment.get("working_directory"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            origin_directory: metadata
                .get("origin_directory")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            parent_session_id: metadata
                .get("parent_session_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            title: metadata
                .get("title")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            start_time,
            end_time,
            bumped_at: normalized("bumped_at"),
            pinned_at: normalized("pinned_at"),
            updated_at,
        },
    })
}

/// Reference `SessionLoader.get_first_user_message`.
pub(super) fn first_user_message(store: &SessionStore, session_id: &str) -> String {
    let suffix = format!("_{}", super::short_session_id(session_id));
    let latest = store
        .session_directories()
        .unwrap_or_default()
        .into_iter()
        .filter(|directory| directory.ends_with(&suffix))
        .filter_map(|directory| {
            let path = store.root().join(directory).join(MESSAGES_FILE);
            let modified = fs::metadata(&path).and_then(|item| item.modified()).ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified);
    let Some((_, path)) = latest else {
        return "(session not found)".to_owned();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return "(error reading session)".to_owned();
    };
    for line in content.split('\n').filter(|line| !line.is_empty()) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return "(corrupted session)".to_owned();
        };
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(text) = message.get("content").and_then(content_text) {
            return preview_snippet(&text);
        }
    }
    "(no user messages)".to_owned()
}

/// Reference `SessionLoader._extract_text_from_content`.
fn content_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    let cleaned = text.trim().replace('\n', " ");
    Some(if cleaned.is_empty() {
        "(empty message)".to_owned()
    } else {
        cleaned
    })
}

/// Reference `_preview_snippet`.
fn preview_snippet(text: &str) -> String {
    if text.chars().count() > PREVIEW_MAX_CHARS {
        let head: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
        format!("{}…", head.trim_end())
    } else {
        text.to_owned()
    }
}

fn modified_ns(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()
}

/// Reference `_mtime_to_utc_iso`.
fn mtime_to_utc_iso(mtime_ns: u64) -> String {
    format_iso_micros(mtime_ns / 1_000)
}
