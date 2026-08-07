//! Building the local transcript projection from canonical server state.
//!
//! Every path that replaces or extends the projection lives here: the first
//! hydration, adopting a new session id, paging older history, and the saved
//! history fallback used when the canonical read comes back empty.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use vibe_app_server::client::{PublicContentBlock, PublicHistoryEntry, PublicMessageRole};

use super::callback::sync_active_callbacks;
use super::controls::ControlState;
use super::runtime::InteractiveRuntime;
use super::state::{
    EntryStatus, ServerEvent, TranscriptEntry, TranscriptKind, TuiSnapshot, TuiState,
};
use super::{Arguments, CliError, INITIAL_HISTORY_LIMIT, sync_runtime_intent};

#[derive(Debug, Deserialize)]
pub(super) struct PublicHistoryList {
    pub(super) history: Vec<PersistedMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(super) enum PersistedMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
    },
    Tool {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

pub(super) fn adopt_hydrated_session(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    controls: &mut ControlState,
    session_id: String,
) -> bool {
    let mut replacement = match canonical_session_projection(runtime, &session_id, true) {
        Ok(replacement) => replacement,
        Err(error) => {
            state.push_diagnostic(format!(
                "Canonical session `{session_id}` is unavailable: {error}"
            ));
            return false;
        }
    };
    replacement.resize(state.viewport.0, state.viewport.1);
    runtime.session_id.clone_from(&session_id);
    sync_runtime_intent(runtime, None);
    *state = replacement;
    *controls = ControlState::new(session_id);
    sync_active_callbacks(runtime, state, controls);
    true
}

pub(super) fn metadata_session_id(result: &BTreeMap<String, Value>) -> Option<String> {
    result
        .get("metadata")
        .and_then(|metadata| {
            metadata
                .get("session_id")
                .or_else(|| metadata.get("sessionId"))
                .or_else(|| metadata.get("id"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub(super) fn hydrate_initial_state(
    runtime: &mut InteractiveRuntime,
    _arguments: &Arguments,
    _working_directory: &Path,
) -> Result<TuiState, CliError> {
    let session_id = runtime.session_id.clone();
    match canonical_session_projection(runtime, &session_id, true) {
        Ok(state) => Ok(state),
        Err(error) => Ok(recoverable_initial_state(
            &session_id,
            format!("Initial session state is unavailable: {error}"),
        )),
    }
}

pub(super) fn canonical_session_projection(
    runtime: &mut InteractiveRuntime,
    session_id: &str,
    include_saved_history: bool,
) -> Result<TuiState, CliError> {
    let result = runtime
        .service
        .public_call("session/read", json!({"sessionId": session_id}))?;
    let Some(public_state) = result.get("state") else {
        return Err(CliError::Terminal(
            "session/read omitted public state".to_owned(),
        ));
    };
    let mut state = tui_state_from_public_session(session_id, public_state)?;
    if include_saved_history {
        overlay_latest_saved_history(runtime, &mut state)?;
    }
    Ok(state)
}

fn overlay_latest_saved_history(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) -> Result<(), CliError> {
    // How long the stored transcript is decides which page of it to ask for,
    // and the session record is what carries both that length and the context
    // accounting. `session/rewind/read` used to answer them, which it no longer
    // does: it answers what a rewind to one entry would restore.
    let count = match runtime
        .service
        .public_call("session/log/read", json!({"sessionId": state.session_id}))
    {
        Ok(result) => {
            runtime.context_tokens = result
                .get("metadata")
                .and_then(|metadata| metadata.get("statistics"))
                .and_then(|statistics| statistics.get("context_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_default();
            result
                .get("messages")
                .and_then(Value::as_array)
                .and_then(|messages| u64::try_from(messages.len()).ok())
        }
        Err(_) => None,
    };
    let Some(message_count) = count.and_then(|count| usize::try_from(count).ok()) else {
        return Ok(());
    };
    let offset = message_count.saturating_sub(INITIAL_HISTORY_LIMIT);
    let result = runtime.service.public_call(
        "history/list",
        json!({
            "sessionId": state.session_id,
            "offset": offset,
            "limit": INITIAL_HISTORY_LIMIT,
        }),
    )?;
    let history = decode_public_result::<PublicHistoryList>(result)?;
    let mut entries = transcript_entries_from_history(&state.session_id, offset, &history.history);
    for entry in state
        .entries
        .iter()
        .filter(|entry| {
            entry.status == EntryStatus::Streaming || entry.kind == TranscriptKind::Callback
        })
        .cloned()
    {
        if !entries.iter().any(|current| current.id == entry.id) {
            entries.push(entry);
        }
    }
    state
        .apply(ServerEvent::Snapshot(TuiSnapshot {
            session_id: state.session_id.clone(),
            event_id: state.watermark,
            entries,
            cursor_before: (offset > 0).then(|| offset.to_string()),
            cursor_after: None,
            waiting: state.waiting,
        }))
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    Ok(())
}

/// Reveals the page above the oldest entry once the operator scrolls past it,
/// leaving the projection ordered oldest-first.
pub(super) fn page_older_history(runtime: Option<&mut InteractiveRuntime>, state: &mut TuiState) {
    if !state.needs_older_history() {
        return;
    }
    let Some(runtime) = runtime else {
        state.push_diagnostic("Saved history is unavailable until setup completes");
        return;
    };
    let Some(before) = state.cursor_before.clone() else {
        return;
    };
    let Ok(before) = before.parse::<usize>() else {
        state.cursor_before = None;
        state.push_diagnostic("Saved-history cursor is invalid");
        return;
    };
    let limit = before.min(INITIAL_HISTORY_LIMIT);
    let offset = before.saturating_sub(limit);
    let result = runtime.service.public_call(
        "history/list",
        json!({
            "sessionId": state.session_id,
            "offset": offset,
            "limit": limit,
        }),
    );
    let history = match result
        .map_err(CliError::from)
        .and_then(decode_public_result::<PublicHistoryList>)
    {
        Ok(history) => history,
        Err(error) => {
            state.push_diagnostic(format!("Older history is unavailable: {error}"));
            return;
        }
    };
    let entries = transcript_entries_from_history(&state.session_id, offset, &history.history);
    if let Err(error) = state.prepend_history(entries, (offset > 0).then(|| offset.to_string())) {
        state.push_diagnostic(format!("Older history is invalid: {error}"));
        return;
    }
    state.scroll_to_oldest();
}

pub(super) fn transcript_entries_from_history(
    session_id: &str,
    offset: usize,
    history: &[PersistedMessage],
) -> Vec<TranscriptEntry> {
    history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let (kind, text, status) = match message {
                PersistedMessage::User { content } => (
                    TranscriptKind::UserMessage,
                    content.clone(),
                    EntryStatus::Completed,
                ),
                PersistedMessage::Assistant { content } => (
                    TranscriptKind::AssistantMessage,
                    content.clone(),
                    EntryStatus::Completed,
                ),
                PersistedMessage::Tool {
                    call_id,
                    content,
                    is_error,
                } => (
                    TranscriptKind::Effect,
                    format!("Tool {call_id}\n{content}"),
                    if *is_error {
                        EntryStatus::Failed
                    } else {
                        EntryStatus::Completed
                    },
                ),
                PersistedMessage::System { content } => {
                    let _ = content;
                    return None;
                }
            };
            Some(TranscriptEntry {
                id: format!("persisted:{session_id}:{}", offset.saturating_add(index)),
                revision: 1,
                kind,
                text,
                status,
                details: json!({"source": "history/list"}),
            })
        })
        .collect()
}

pub(super) fn decode_public_result<T: DeserializeOwned>(
    result: BTreeMap<String, Value>,
) -> Result<T, CliError> {
    serde_json::from_value(Value::Object(result.into_iter().collect())).map_err(CliError::Json)
}

pub(super) fn recoverable_initial_state(
    session_id: &str,
    diagnostic: impl Into<String>,
) -> TuiState {
    let mut state = TuiState::new(session_id);
    state.ready = true;
    state.push_diagnostic(diagnostic);
    state
}

pub(super) fn tui_state_from_public_session(
    session_id: &str,
    public_state: &Value,
) -> Result<TuiState, CliError> {
    let reported_session_id = public_state
        .pointer("/session/id")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Terminal("session/read omitted session id".to_owned()))?;
    if reported_session_id != session_id {
        return Err(CliError::Terminal(format!(
            "session/read returned foreign session `{reported_session_id}`"
        )));
    }
    let mut entries = serde_json::from_value::<Vec<PublicHistoryEntry>>(
        public_state
            .pointer("/history/entries")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(CliError::Json)?
    .into_iter()
    .map(history_entry)
    .collect::<Vec<_>>();
    let active_callbacks = serde_json::from_value::<Vec<PublicHistoryEntry>>(
        public_state
            .get("activeCallbacks")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(CliError::Json)?;
    if active_callbacks.len() > 1 {
        return Err(CliError::Terminal(
            "session/read projected more than one active callback".to_owned(),
        ));
    }
    for callback in active_callbacks {
        let callback = history_entry(callback);
        if !entries.iter().any(|entry| entry.id == callback.id) {
            entries.push(callback);
        }
    }
    let cursor_before = public_state
        .pointer("/history/cursor/before")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cursor_after = public_state
        .pointer("/history/cursor/after")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let waiting = matches!(
        public_state
            .pointer("/session/status/type")
            .and_then(Value::as_str),
        Some("running" | "blocked")
    );
    let mut state = TuiState::new(session_id);
    state
        .apply(ServerEvent::Snapshot(TuiSnapshot {
            session_id: session_id.to_owned(),
            event_id: public_state
                .get("eventId")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            entries,
            cursor_before,
            cursor_after,
            waiting,
        }))
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    state
        .apply(ServerEvent::Ready)
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    Ok(state)
}

pub(super) fn resync_current_projection(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    match canonical_session_projection(runtime, &runtime.session_id.clone(), false) {
        Ok(replacement) => {
            if let Err(error) = state.replace_projection_preserving_diagnostics(replacement) {
                state.push_diagnostic(format!("Canonical resync was rejected: {error}"));
            }
        }
        Err(error) => {
            state.push_diagnostic(format!("Canonical resync failed: {error}"));
        }
    }
}

pub(super) fn history_entry(entry: PublicHistoryEntry) -> TranscriptEntry {
    let metadata = entry.metadata().clone();
    let completed = entry.is_completed();
    let details = serde_json::to_value(&entry).unwrap_or(Value::Null);
    let (kind, text) = match entry {
        PublicHistoryEntry::Message { role, content, .. } => {
            let kind = match role {
                PublicMessageRole::User => TranscriptKind::UserMessage,
                PublicMessageRole::Assistant => TranscriptKind::AssistantMessage,
                PublicMessageRole::System => TranscriptKind::Notice,
            };
            (kind, content_text(&content))
        }
        PublicHistoryEntry::Reasoning { text, summary, .. } => {
            let text = if text.is_empty() {
                summary.join("\n")
            } else {
                text
            };
            (TranscriptKind::Reasoning, text)
        }
        // `details` keeps the canonical entry verbatim: the semantic
        // presentation is derived from it by `tui::transcript`, so nothing is
        // flattened or duplicated here.
        PublicHistoryEntry::Effect { title, state, .. } => {
            let output = serde_json::to_value(state)
                .ok()
                .and_then(|encoded| {
                    encoded
                        .get("outputText")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            (
                TranscriptKind::Effect,
                if output.is_empty() {
                    title
                } else {
                    format!("{title}\n{output}")
                },
            )
        }
        PublicHistoryEntry::Callback { title, detail, .. } => (
            TranscriptKind::Callback,
            format!(
                "{title}\n{}",
                serde_json::to_value(&detail).unwrap_or(Value::Null)
            ),
        ),
        PublicHistoryEntry::Checkpoint { kind, message, .. } => (
            TranscriptKind::Checkpoint,
            message.unwrap_or_else(|| format!("Checkpoint: {kind}")),
        ),
        PublicHistoryEntry::Notice { message, .. } => (TranscriptKind::Notice, message),
    };
    // The nested effect or callback state is authoritative: a completed
    // generation whose effect failed, was cancelled, or was skipped must never
    // settle as a success.
    let nested_status = details.pointer("/state/status").and_then(Value::as_str);
    let status = match nested_status {
        Some("failed") => EntryStatus::Failed,
        Some("cancelled") | Some("expired") => EntryStatus::Cancelled,
        Some("skipped") => EntryStatus::Skipped,
        Some("pending") => EntryStatus::Pending,
        Some("blocked") => EntryStatus::Blocked,
        Some("running") => EntryStatus::Streaming,
        _ if completed => EntryStatus::Completed,
        _ => EntryStatus::Streaming,
    };
    TranscriptEntry {
        id: metadata.id,
        revision: metadata.updated_at.max(1),
        kind,
        text,
        status,
        details,
    }
}

pub(super) fn content_text(content: &[PublicContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            PublicContentBlock::Text { text } => text.clone(),
            PublicContentBlock::Image { attachment } => {
                format!(
                    "[image: {}]",
                    attachment
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unsupported terminal image")
                )
            }
            PublicContentBlock::Resource { resource } => format!(
                "[resource: {}]",
                resource
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("resource")
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_session_history_hydrates_the_initial_projection() {
        let state = tui_state_from_public_session(
            "session",
            &json!({
                "eventId": 9,
                "session": {
                    "id": "session",
                    "status": {"type": "running"}
                },
                "history": {
                    "entries": [{
                        "type": "message",
                        "id": "restored-user",
                        "sessionId": "session",
                        "createdAt": 1,
                        "updatedAt": 2,
                        "generationStatus": "completed",
                        "role": "user",
                        "content": [{"type": "text", "text": "restored prompt"}]
                    }],
                    "cursor": {"before": "older", "after": null}
                }
            }),
        )
        .expect("public session state hydrates");

        assert_eq!(state.watermark, 9);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].text, "restored prompt");
        assert_eq!(state.cursor_before.as_deref(), Some("older"));
        assert!(state.waiting);
        assert!(state.ready);
    }

    #[test]
    fn persisted_history_fallback_is_bounded_and_semantic() {
        let messages = json!([
            {"role": "system", "content": "internal"},
            {"role": "user", "content": "restored question"},
            {
                "role": "assistant",
                "content": "restored answer",
                "reasoning": null,
                "reasoning_state": [],
                "tool_calls": []
            },
            {"role": "tool", "call_id": "shell-1", "content": "failed", "is_error": true}
        ]);
        let messages =
            serde_json::from_value::<Vec<PersistedMessage>>(messages).expect("typed history");
        let entries = transcript_entries_from_history("saved", 40, &messages);

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "persisted:saved:41");
        assert_eq!(entries[0].kind, TranscriptKind::UserMessage);
        assert_eq!(entries[1].kind, TranscriptKind::AssistantMessage);
        assert_eq!(entries[2].kind, TranscriptKind::Effect);
        assert_eq!(entries[2].status, EntryStatus::Failed);
    }

    #[test]
    fn active_callbacks_are_included_when_the_history_window_omits_them() {
        let state = tui_state_from_public_session(
            "session",
            &json!({
                "eventId": 4,
                "session": {
                    "id": "session",
                    "status": {"type": "blocked"}
                },
                "history": {"entries": [], "cursor": {"before": null, "after": null}},
                "activeCallbacks": [{
                    "type": "callback",
                    "id": "callback-entry",
                    "sessionId": "session",
                    "turnId": "turn",
                    "createdAt": 1,
                    "updatedAt": 1,
                    "generationStatus": "in_progress",
                    "callbackId": "callback-1",
                    "title": "Approve?",
                    "detail": {
                        "kind": "approval",
                        "effect": {
                            "kind": "shell",
                            "toolName": "bash",
                            "display": {"summary": "bash: ls", "statusText": "Running command"},
                            "input": {"command": "ls"}
                        },
                        "choices": ["approve", "deny", "cancel_turn"]
                    },
                    "state": {"status": "open"}
                }]
            }),
        )
        .expect("active callback hydrates");
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].kind, TranscriptKind::Callback);
        assert_eq!(state.entries[0].status, EntryStatus::Streaming);
    }

    #[test]
    fn malformed_public_history_is_not_mistaken_for_empty_history() {
        let malformed = BTreeMap::from([("history".to_owned(), json!({"role": "user"}))]);
        assert!(decode_public_result::<PublicHistoryList>(malformed).is_err());
    }

    #[test]
    fn rich_content_has_safe_terminal_fallbacks() {
        let content = vec![
            PublicContentBlock::Text {
                text: "hello".to_owned(),
            },
            PublicContentBlock::Image {
                attachment: json!({"name": "diagram.png"}),
            },
            PublicContentBlock::Resource {
                resource: json!({"name": "README.md"}),
            },
        ];
        assert_eq!(
            content_text(&content),
            "hello\n[image: diagram.png]\n[resource: README.md]"
        );
    }
}
