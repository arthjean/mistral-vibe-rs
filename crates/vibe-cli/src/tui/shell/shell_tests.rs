use serde_json::{Value, json};

use super::*;
use crate::tui::state::{EntryStatus, TranscriptKind, TuiSnapshot};
use crate::tui::transcript::{MANUAL_SHELL_TOOL_NAME, Region, region};

/// The `shell` entry a session publishes for `printf ok` in `state`
/// (reference `shell_effect_detail` and `shell_effect_state`).
pub(in crate::tui) fn manual_shell_entry(state: Value) -> PublicHistoryEntry {
    let running = state["status"] == "running";
    serde_json::from_value(json!({
        "type": "effect",
        "id": "operation",
        "sessionId": "session",
        "createdAt": 1,
        "updatedAt": 1,
        "generationStatus": if running { "in_progress" } else { "completed" },
        "title": MANUAL_SHELL_TOOL_NAME,
        "detail": {
            "kind": "shell",
            "toolName": MANUAL_SHELL_TOOL_NAME,
            "display": {
                "summary": "shell: printf ok",
                "verb": "Running",
                "message": "printf ok",
                "settledVerb": "Ran",
                "settledMessage": "printf ok",
                "statusText": "Running command",
            },
            "input": {"command": "printf ok"},
        },
        "state": state,
    }))
    .expect("a manual shell entry")
}

pub(in crate::tui) fn running(output: &str) -> PublicHistoryEntry {
    manual_shell_entry(json!({"status": "running", "outputText": output}))
}

pub(in crate::tui) fn cancelled(output: &str) -> PublicHistoryEntry {
    manual_shell_entry(json!({
        "status": "cancelled",
        "reason": "Command interrupted",
        "outputText": output,
        "durationMs": 0,
        "display": {"success": false, "message": "Command interrupted"},
    }))
}

/// Reference `set_stream_message`: the published entry is drawn once and
/// followed, showing what its output last appended.
#[test]
fn a_published_entry_is_drawn_once_and_followed() {
    let mut state = TuiState::new("session");
    let mut transcript_id = None;

    draw_entry(&mut state, &mut transcript_id, running("a logged line\n"));
    let id = transcript_id.clone().expect("the entry is drawn");
    // Output the command wrote before its entry was first drawn streams too.
    assert_eq!(
        state.effect_streams.get(&id).map(String::as_str),
        Some("a logged line\n")
    );
    draw_entry(
        &mut state,
        &mut transcript_id,
        running("a logged line\nanother\n"),
    );

    assert_eq!(state.entries.len(), 1);
    assert_eq!(state.entries[0].id, id);
    assert_eq!(state.entries[0].kind, TranscriptKind::Effect);
    assert_eq!(state.entries[0].status, EntryStatus::Streaming);
    assert_eq!(
        state.effect_streams.get(&id).map(String::as_str),
        Some("another\n")
    );
}

#[test]
fn a_completed_command_renders_its_settled_header() {
    let mut state = TuiState::new("session");
    let mut transcript_id = None;
    draw_entry(&mut state, &mut transcript_id, running(""));

    draw_entry(
        &mut state,
        &mut transcript_id,
        manual_shell_entry(json!({
            "status": "completed",
            "output": {"stdout": "ok", "stderr": "", "output": "ok"},
            "outputText": "ok",
            "durationMs": 0,
            "display": {"success": true, "message": "Ran printf ok"},
        })),
    );

    assert_eq!(state.entries[0].status, EntryStatus::Completed);
    assert!(state.effect_streams.is_empty());
    let Region::Effect(effect) = region(&state.entries[0]) else {
        panic!("the settled command renders as an effect");
    };
    assert_eq!(effect.header_text(), "Ran printf ok");
}

#[test]
fn late_output_cannot_revive_a_settled_command() {
    let mut state = TuiState::new("session");
    let mut transcript_id = None;
    draw_entry(&mut state, &mut transcript_id, running("partial"));
    draw_entry(&mut state, &mut transcript_id, cancelled("partial"));
    let settled = state.entries[0].clone();

    draw_entry(&mut state, &mut transcript_id, running("partial late"));

    assert_eq!(state.entries[0], settled);
    assert_eq!(state.entries[0].status, EntryStatus::Cancelled);
}

/// A resync that brings the canonical entry back keeps one copy of it.
#[test]
fn a_resync_holding_the_published_entry_shows_it_once() {
    let mut state = TuiState::new("session");
    let mut transcript_id = None;
    draw_entry(&mut state, &mut transcript_id, cancelled("ab"));

    state
        .resync(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 1,
            entries: vec![history_entry(cancelled("ab"))],
            cursor_before: None,
            cursor_after: None,
            waiting: false,
        })
        .expect("the snapshot applies");

    assert_eq!(state.entries.len(), 1);
}
