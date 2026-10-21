use super::super::diagnostics;
use super::*;

fn entry(id: &str, revision: u64, text: &str, status: EntryStatus) -> TranscriptEntry {
    TranscriptEntry {
        id: id.to_owned(),
        revision,
        kind: TranscriptKind::AssistantMessage,
        text: text.to_owned(),
        status,
        source: EntrySource::Restored,
    }
}

fn history(count: usize) -> TuiState {
    let mut state = TuiState::new("session");
    state.entries = (0..count)
        .map(|index| entry(&format!("entry-{index}"), 1, "text", EntryStatus::Completed))
        .collect();
    state.reindex();
    state
}

#[test]
fn a_resumed_transcript_shows_its_tail_and_reveals_the_rest_ten_at_a_time() {
    let mut state = history(35);
    state.window_history_tail();
    assert_eq!(state.hidden_history(), 15);
    assert_eq!(state.older_history_remaining(), Some(15));

    assert!(state.reveal_older_history());
    assert_eq!(state.hidden_history(), 5);
    assert!(state.reveal_older_history());
    assert_eq!(state.hidden_history(), 0);
    assert!(!state.has_older_history());
    assert!(!state.reveal_older_history());

    // A short transcript holds nothing back.
    let mut short = history(20);
    short.window_history_tail();
    assert!(!short.has_older_history());
}

#[test]
fn the_button_names_no_count_while_the_server_holds_older_pages() {
    let mut state = history(25);
    state.window_history_tail();
    state.cursor_before = Some("5".to_owned());
    assert!(state.has_older_history());
    assert_eq!(state.older_history_remaining(), None);
    // A page fetched from the server is shown whole.
    state
        .prepend_history(
            vec![entry("older", 1, "text", EntryStatus::Completed)],
            None,
        )
        .expect("older page prepends");
    assert_eq!(state.hidden_history(), 0);
    assert!(!state.has_older_history());
}

#[test]
fn gaps_require_one_canonical_snapshot_without_partial_mutation() {
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: entry("one", 1, "a", EntryStatus::Streaming),
        })
        .expect("first event applies");
    assert_eq!(
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 3,
                entry: entry("three", 1, "c", EntryStatus::Completed),
            })
            .expect("gap is observable"),
        ApplyResult::ResyncRequired
    );
    assert_eq!(state.entries.len(), 1);
    state
        .apply(ServerEvent::Snapshot(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 3,
            entries: vec![entry("canonical", 1, "state", EntryStatus::Completed)],
            cursor_before: Some("older".to_owned()),
            cursor_after: None,
            waiting: false,
        }))
        .expect("snapshot replaces local state");
    assert_eq!(state.entries[0].id, "canonical");
    assert!(!state.resync_required);
}

#[test]
fn watermark_only_events_preserve_live_sequence_without_mutating_projection() {
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: entry("user", 1, "hello", EntryStatus::Completed),
        })
        .expect("user message applies");
    assert_eq!(
        state
            .apply(ServerEvent::Watermark { event_id: 2 })
            .expect("title-only projection advances the watermark"),
        ApplyResult::Applied
    );
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 3,
            entry: entry("assistant", 1, "answer", EntryStatus::Streaming),
        })
        .expect("model text applies after the title watermark");

    assert_eq!(state.watermark, 3);
    assert_eq!(state.entries.len(), 2);
    assert!(!state.resync_required);
}

/// Reference `_appended_text(update.patch, "/state/outputText")`: the call
/// shows what each update appended, and nothing once it settles.
#[test]
fn a_running_effect_streams_what_each_update_appended() {
    let effect = |revision: u64, state: serde_json::Value| {
        let mut entry = crate::tui::hydration::published_fixture(
            "call",
            serde_json::json!({
                "type": "effect",
                "title": "bash",
                "detail": vibe_app_server::client::EffectDetail::for_call(
                    "bash",
                    &serde_json::json!({"command": "make"}),
                ),
                "state": state,
                "generationStatus": "in_progress",
            }),
        );
        entry.revision = revision;
        entry
    };
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: effect(
                1,
                serde_json::json!({"status": "running", "outputText": "a\n"}),
            ),
        })
        .expect("the call starts");
    assert!(state.effect_streams.is_empty());
    state
        .apply(ServerEvent::EntryUpdated {
            event_id: 2,
            entry: effect(
                2,
                serde_json::json!({"status": "running", "outputText": "a\nb\nc\n"}),
            ),
        })
        .expect("output grows");
    assert_eq!(
        state.effect_streams.get("call").map(String::as_str),
        Some("b\nc\n")
    );
    state
        .apply(ServerEvent::EntryUpdated {
            event_id: 3,
            entry: effect(
                3,
                serde_json::json!({
                    "status": "failed",
                    "error": {"message": "exit 2"},
                    "outputText": "a\nb\nc\n",
                    "display": {"success": false, "message": "make"},
                }),
            ),
        })
        .expect("the call settles");
    assert!(state.effect_streams.is_empty());
}

#[test]
fn streamed_content_is_monotonic_and_completed_entries_are_immutable() {
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: entry("one", 1, "a", EntryStatus::Streaming),
        })
        .expect("stream starts");
    state
        .apply(ServerEvent::EntryUpdated {
            event_id: 2,
            entry: entry("one", 2, "answer", EntryStatus::Completed),
        })
        .expect("stream completes");
    assert!(matches!(
        state.apply(ServerEvent::EntryUpdated {
            event_id: 3,
            entry: entry("one", 3, "answer changed", EntryStatus::Completed),
        }),
        Err(StateError::CompletedEntryMutation(_))
    ));
    assert_eq!(state.watermark, 2);
    assert_eq!(
        state
            .apply(ServerEvent::Diagnostic {
                event_id: 3,
                message: "recovered".to_owned(),
            })
            .expect("invalid mutation did not consume the sequence"),
        ApplyResult::Applied
    );
}

#[test]
fn settling_replaces_streamed_output_while_live_growth_stays_monotonic() {
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: entry("effect", 1, "shell\npartial", EntryStatus::Streaming),
        })
        .expect("effect starts");
    assert!(matches!(
        state.apply(ServerEvent::EntryUpdated {
            event_id: 2,
            entry: entry("effect", 2, "shell\nrewritten", EntryStatus::Streaming),
        }),
        Err(StateError::NonMonotonicStream(_))
    ));
    state
        .apply(ServerEvent::EntryUpdated {
            event_id: 2,
            entry: entry("effect", 2, "shell\npermission denied", EntryStatus::Failed),
        })
        .expect("terminal projection replaces the stream");
    assert_eq!(state.entries[0].status, EntryStatus::Failed);
    assert_eq!(state.entries[0].text, "shell\npermission denied");
}

#[test]
fn canonical_replacement_preserves_diagnostics_but_not_stale_projection_data() {
    let mut state = TuiState::new("session");
    state.push_diagnostic("keep this diagnostic");
    state.resize(41, 9);
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: entry("stale", 1, "old", EntryStatus::Completed),
        })
        .expect("stale entry applies");

    state
        .resync(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 8,
            entries: vec![entry("canonical", 1, "new", EntryStatus::Completed)],
            cursor_before: None,
            cursor_after: None,
            waiting: false,
        })
        .expect("same-session replacement");

    assert_eq!(state.watermark, 8);
    assert_eq!(state.entries[0].id, "canonical");
    assert_eq!(state.viewport, (41, 9));
    assert_eq!(
        state.diagnostics().collect::<Vec<_>>(),
        vec!["keep this diagnostic"]
    );
}

#[test]
fn a_resync_replaces_history_without_disturbing_local_observability() {
    let mut state = TuiState::new("session");
    state.waiting = true;
    state.sync_activity(5_000);
    state.debug_console = Some(super::super::debug_console::DebugConsole::default());
    state
        .transcript_view
        .publish(3, vec!["See https://ratatui.rs".to_owned()]);
    state
        .transcript_view
        .begin_selection(super::super::transcript_view::Cell { line: 0, column: 0 });
    state
        .transcript_view
        .extend_selection(super::super::transcript_view::Cell { line: 0, column: 3 });
    assert!(state.transcript_view.has_selection());

    state
        .resync(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 2,
            entries: vec![entry("canonical", 1, "new", EntryStatus::Completed)],
            cursor_before: None,
            cursor_after: None,
            waiting: true,
        })
        .expect("same-session replacement");

    assert!(state.debug_console.is_some(), "the console stayed open");
    assert!(
        state.transcript_view.has_selection(),
        "the operator's selection survived the resync"
    );
    // The turn clock keeps running: the resync did not restart the turn.
    state.sync_activity(9_000);
    assert_eq!(
        state
            .activity
            .as_ref()
            .map(|activity| activity.elapsed_seconds),
        Some(4)
    );
}

#[test]
fn a_rejected_resync_leaves_the_state_as_it_was() {
    let mut state = TuiState::new("session");
    state.append_local(entry("ignored", 1, "local", EntryStatus::Completed));
    state.scroll_offset = 3;
    let before = state.clone();

    assert_eq!(
        state.resync(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 4,
            entries: vec![entry("local-1", 1, "collides", EntryStatus::Completed)],
            cursor_before: None,
            cursor_after: None,
            waiting: false,
        }),
        Err(StateError::DuplicateEntry("local-1".to_owned()))
    );
    assert_eq!(state, before);
}

#[test]
fn canonical_replacement_restores_local_entries_at_their_explicit_anchors() {
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 1,
            entry: entry("server-one", 1, "one", EntryStatus::Completed),
        })
        .expect("first server entry");
    state.append_local(entry("ignored", 1, "local one", EntryStatus::Completed));
    state.append_local(entry("ignored", 1, "local two", EntryStatus::Cancelled));
    state
        .apply(ServerEvent::EntryAdded {
            event_id: 2,
            entry: entry("server-two", 1, "two", EntryStatus::Completed),
        })
        .expect("second server entry");

    state
        .resync(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 3,
            entries: vec![
                entry("server-one", 1, "one", EntryStatus::Completed),
                entry("server-two", 1, "two", EntryStatus::Completed),
            ],
            cursor_before: None,
            cursor_after: None,
            waiting: false,
        })
        .expect("local positions restore");

    assert_eq!(
        state
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "local one", "local two", "two"]
    );
}

#[test]
fn paging_preserves_order_and_requests_older_history_at_the_top() {
    let mut state = TuiState::new("session");
    state
        .apply(ServerEvent::Snapshot(TuiSnapshot {
            session_id: "session".to_owned(),
            event_id: 4,
            entries: vec![
                entry("three", 1, "three", EntryStatus::Completed),
                entry("four", 1, "four", EntryStatus::Completed),
            ],
            cursor_before: Some("before-three".to_owned()),
            cursor_after: None,
            waiting: false,
        }))
        .expect("snapshot applies");
    state.set_scroll_line_limit(1);
    state.scroll_up(10);
    assert!(state.needs_older_history());
    state
        .prepend_history(
            vec![
                entry("one", 1, "one", EntryStatus::Completed),
                entry("two", 1, "two", EntryStatus::Completed),
            ],
            None,
        )
        .expect("older history prepends");
    assert_eq!(
        state
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["one", "two", "three", "four"]
    );
}

#[test]
fn activity_reports_the_running_effect_and_clears_the_moment_work_settles() {
    let mut state = TuiState::new("session");
    state.waiting = true;
    state.entries.push(crate::tui::hydration::published_fixture(
        "effect",
        serde_json::json!({
            "type": "effect",
            "title": "read",
            "detail": vibe_app_server::client::EffectDetail::for_call(
                "read_file",
                &serde_json::json!({"file_path": "a.rs"}),
            ),
            "state": {"status": "running", "outputText": ""},
        }),
    ));
    state.sync_activity(1_000);
    let activity = state.activity.clone().expect("an active turn reports work");
    assert_eq!(activity.status, "Reading file");
    assert_eq!(activity.elapsed_seconds, 0);

    state.sync_activity(4_000);
    let activity = state.activity.clone().expect("the turn is still active");
    assert_eq!(activity.elapsed_seconds, 3);
    assert_eq!(activity.hint(), "(3s Esc/Ctrl+C to interrupt)");

    state.waiting = false;
    state.sync_activity(9_000);
    assert_eq!(state.activity, None, "a settled turn leaves no indicator");

    // The next turn restarts its own clock instead of resuming the old one.
    state.waiting = true;
    state.entries.clear();
    state.sync_activity(20_000);
    let activity = state.activity.clone().expect("the next turn reports work");
    assert_eq!(activity.elapsed_seconds, 0);
    assert_eq!(activity.status, diagnostics::DEFAULT_ACTIVITY_STATUS);
}
