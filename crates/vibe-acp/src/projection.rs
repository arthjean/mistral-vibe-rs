//! Public history entries as ACP session updates.
//!
//! Reference `vibe/acp/session_updates.py`: an entry the app server adds is
//! announced in full, a revision of one is projected as the delta between the
//! two versions, and a saved transcript is replayed through the same
//! announcement path. The projection reads the entries in their wire form, which
//! is the shape both implementations publish, so every rule here names the same
//! camelCase field the reference model aliases.

mod effect;
mod message;

use serde_json::{Map, Value, json};

use effect::{effect_progress_updates, effect_start, plan_update};
use message::{message_text, message_updates, reasoning_updates};

/// The updates a saved transcript replays as: the display title first, then
/// every entry except the title notices the title already stands for.
pub(crate) fn replay_session_updates(
    title: Option<&str>,
    updated_at: u64,
    history: &[Value],
) -> Vec<Value> {
    let mut updates = Vec::new();
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        updates.push(session_info_update(Some(title), updated_at));
    }
    for entry in history {
        if is_title_notice(entry) {
            continue;
        }
        updates.extend(replay_history_entry(entry));
    }
    updates
}

/// The updates one entry is announced with.
pub(crate) fn replay_history_entry(entry: &Value) -> Vec<Value> {
    match kind(entry) {
        "message" => message_updates(entry, None),
        "reasoning" => reasoning_updates(entry, None),
        "effect" => {
            let mut updates = vec![effect_start(entry)];
            updates.extend(plan_update(entry));
            updates
        }
        "checkpoint" => vec![checkpoint_update(entry, false)],
        "notice" if is_title_notice(entry) => {
            let title = entry.pointer("/detail/title").and_then(Value::as_str);
            vec![session_info_update(title, updated_at(entry))]
        }
        _ => Vec::new(),
    }
}

/// The updates an entry the app server just added is announced with.
pub(crate) fn added_entry_updates(entry: &Value) -> Vec<Value> {
    if is_title_notice(entry) {
        return Vec::new();
    }
    replay_history_entry(entry)
}

/// The updates a revision of an entry is projected as.
pub(crate) fn updated_entry_updates(previous: &Value, entry: &Value) -> Vec<Value> {
    match (kind(previous), kind(entry)) {
        ("message", "message") => {
            let delta = text_delta(&message_text(previous), &message_text(entry));
            if delta.is_empty() {
                Vec::new()
            } else {
                message_updates(entry, Some(&delta))
            }
        }
        ("reasoning", "reasoning") => {
            let delta = text_delta(text_field(previous, "text"), text_field(entry, "text"));
            if delta.is_empty() {
                Vec::new()
            } else {
                reasoning_updates(entry, Some(&delta))
            }
        }
        ("effect", "effect") => effect_progress_updates(previous, entry),
        ("checkpoint", "checkpoint") => vec![checkpoint_update(entry, true)],
        _ => Vec::new(),
    }
}

/// A session title as the client renders it, stamped with the session's
/// update time.
pub(crate) fn session_info_update(title: Option<&str>, updated_at_ms: u64) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".to_owned(), json!("session_info_update"));
    if let Some(title) = title {
        update.insert("title".to_owned(), json!(title));
    }
    update.insert(
        "updatedAt".to_owned(),
        json!(vibe_core::time::format_iso_millis(updated_at_ms)),
    );
    Value::Object(update)
}

fn kind(entry: &Value) -> &str {
    entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn text_field<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn updated_at(entry: &Value) -> u64 {
    entry
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn is_title_notice(entry: &Value) -> bool {
    kind(entry) == "notice"
        && entry.pointer("/detail/kind").and_then(Value::as_str) == Some("session_title_updated")
}

fn is_completed(entry: &Value) -> bool {
    entry.get("generationStatus").and_then(Value::as_str) == Some("completed")
}

/// What `current` adds to `previous`, or all of it when it is not an
/// extension.
fn text_delta(previous: &str, current: &str) -> String {
    current.strip_prefix(previous).unwrap_or(current).to_owned()
}

/// An object without its `null` members, which is what a model serialized
/// without its unset fields publishes.
pub(crate) fn object<'a>(fields: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// A text block as ACP tool call content.
pub(crate) fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

fn checkpoint_update(entry: &Value, progress: bool) -> Value {
    let message = entry
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty());
    let kind_name = entry
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = message.map_or_else(|| checkpoint_title(kind_name), ToOwned::to_owned);
    let content = entry
        .get("message")
        .and_then(Value::as_str)
        .map_or(Value::Null, |message| json!([text_content(message)]));
    let details = entry.get("details").cloned().unwrap_or(Value::Null);
    let status = if is_completed(entry) {
        "completed"
    } else {
        "in_progress"
    };
    object(vec![
        (
            "sessionUpdate",
            json!(if progress {
                "tool_call_update"
            } else {
                "tool_call"
            }),
        ),
        (
            "toolCallId",
            entry.get("id").cloned().unwrap_or(Value::Null),
        ),
        ("title", json!(title)),
        ("kind", json!("think")),
        ("status", json!(status)),
        ("content", content),
        (if progress { "rawOutput" } else { "rawInput" }, details),
        ("_meta", json!({"checkpoint_kind": kind_name})),
    ])
}

/// The label a checkpoint without a message is shown under: its kind, spaced
/// and capitalized the way Python's `str.capitalize` does it.
fn checkpoint_title(kind: &str) -> String {
    let spaced = kind.replace('_', " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first
            .to_uppercase()
            .chain(characters.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}
