use std::path::PathBuf;

use vibe_app_server::client::PublicHistoryEntry;

use super::super::controls::ControlFocus;
use super::super::hydration::canonical_session_projection;
use super::super::runtime::interactive_test_runtime;
use super::super::state::PlanReviewState;
use super::*;

#[test]
fn plan_review_choices_and_callback_bounds_are_exact() {
    assert_eq!(
        plan_option_id("Yes, clear context and auto approve edits"),
        Some("clear_auto")
    );
    assert_eq!(
        plan_transition(&CallbackChoice::Option {
            id: "manual".to_owned()
        }),
        Some(false)
    );
    // The clearing choice differs from the plain one only in what the tool
    // raises on the turn; both switch the session to auto approval.
    assert_eq!(
        plan_transition(&CallbackChoice::Option {
            id: "clear_auto".to_owned()
        }),
        Some(true)
    );
    assert_eq!(
        plan_transition(&CallbackChoice::FreeText {
            value: "Revise it".to_owned()
        }),
        None
    );
    assert!(required_callback_text(&"x".repeat(MAX_CALLBACK_TEXT_BYTES + 1), "callback").is_err());
    assert_eq!(
        callback_effect_content("shell", &json!({"command": "cargo test"})),
        "$ cargo test"
    );
}

/// The reference publishes a plan review as its own notice, so the callback
/// detail carries no marker and the open notice is what turns the same
/// user-input question into a plan review.
#[test]
fn the_plan_review_notice_decides_what_a_user_input_callback_is() {
    let entry = serde_json::from_value::<PublicHistoryEntry>(json!({
        "type": "callback",
        "id": "callback-entry",
        "sessionId": "session",
        "turnId": "turn",
        "createdAt": 1,
        "updatedAt": 1,
        "generationStatus": "in_progress",
        "callbackId": "callback-1",
        "title": "Review plan",
        "detail": {
            "kind": "user_input",
            "request": {
                "questions": [{
                    "header": "Plan",
                    "question": "Approve?",
                    "options": [
                        {"label": "Yes", "description": "Approve plan"},
                        {"label": "No", "description": "Keep planning"}
                    ],
                    "multiSelect": false,
                    "hideOther": true
                }]
            }
        },
        "state": {"status": "open"}
    }))
    .expect("callback fixture");

    let plain = pending_callback_from_entry(&entry, None)
        .expect("callback is valid")
        .expect("callback is active");
    assert!(matches!(plain.request, CallbackRequest::UserInput { .. }));

    let reviewing = pending_callback_from_entry(&entry, Some(Path::new("/tmp/plan.md")))
        .expect("callback is valid")
        .expect("callback is active");
    let CallbackRequest::PlanReview { plan_path, .. } = &reviewing.request else {
        panic!("expected a plan-review callback");
    };
    assert_eq!(plan_path, Path::new("/tmp/plan.md"));
}

/// The relation is what stops a plan review from leaking onto the next
/// question the session asks.
#[test]
fn a_plan_review_notice_only_reaches_the_callback_it_relates_to() {
    let mut state = TuiState::new("session");
    assert!(plan_reviews(&state).is_empty());

    state.entries.push(crate::tui::hydration::history_entry(
        PublicHistoryEntry::Notice {
            metadata: vibe_app_server::client::PublicEntryMetadata {
                input_entry_id: None,
                id: "notice:callback-1:plan-review".to_owned(),
                session_id: "session".to_owned(),
                turn_id: None,
                created_at: 1,
                updated_at: 1,
                generation_status: vibe_app_server::client::PublicEntryGenerationStatus::Completed,
                related_entry_id: Some("callback:callback-1".to_owned()),
            },
            level: vibe_app_server::client::PublicNoticeLevel::Info,
            message: "Review plan".to_owned(),
            detail: NoticeDetail::PlanReviewStarted {
                file_path: "/tmp/plan.md".to_owned(),
            },
        },
    ));

    let reviews = plan_reviews(&state);
    assert_eq!(
        reviews.get("callback:callback-1"),
        Some(&PathBuf::from("/tmp/plan.md"))
    );
    assert_eq!(reviews.get("callback:callback-2"), None);
}

#[test]
fn callback_absence_fails_an_unsettled_notice_closed() {
    let mut state = TuiState::new("session");
    state.append_local(TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: "approval pending".to_owned(),
        status: EntryStatus::Streaming,
        source: EntrySource::tracking("callback"),
    });

    fail_inactive_callback_notices(&mut state, None);

    assert_eq!(state.entries[0].status, EntryStatus::Failed);
}

#[test]
fn canonical_multi_question_callbacks_preserve_structure() {
    let entry = serde_json::from_value::<PublicHistoryEntry>(json!({
        "type": "callback",
        "id": "callback-entry",
        "sessionId": "session",
        "turnId": "turn",
        "createdAt": 1,
        "updatedAt": 1,
        "generationStatus": "in_progress",
        "callbackId": "callback-1",
        "title": "Need input\u{1b}[31m",
        "detail": {
            "kind": "user_input",
            "request": {
                "questions": [
                    {
                        "header": "Runtime",
                        "question": "Choose runtimes",
                        "options": [
                            {"label": "Rust", "description": "Native"},
                            {"label": "Python", "description": "Portable"}
                        ],
                        "multiSelect": true,
                        "hideOther": true
                    },
                    {
                        "header": "Constraint",
                        "question": "Any constraints? \u{4e16}\u{754c}",
                        "options": [
                            {"label": "Fast", "description": ""},
                            {"label": "Small", "description": ""}
                        ],
                        "multiSelect": false,
                        "hideOther": false
                    }
                ],
                "footerNote": "All answers are submitted together."
            }
        },
        "state": {"status": "open"}
    }))
    .expect("callback fixture");
    let pending = pending_callback_from_entry(&entry, None)
        .expect("callback is valid")
        .expect("callback is active");
    let CallbackRequest::UserInput { questions, .. } = &pending.request else {
        panic!("expected user-input callback");
    };
    assert_eq!(questions.len(), 2);
    assert!(questions[0].multi_select);
    assert!(
        pending
            .prompt
            .contains("All answers are submitted together.")
    );
}

#[test]
fn plan_approval_fails_closed_until_the_live_file_is_readable_and_nonempty() {
    let mut state = TuiState::new("session");
    assert!(plan_approval_error(&state).is_some());
    state.plan_review = Some(PlanReviewState {
        path: PathBuf::from("plan.md"),
        content: String::new(),
        error: Some("missing".to_owned()),
    });
    assert!(plan_approval_error(&state).is_some());
    state.plan_review = Some(PlanReviewState {
        path: PathBuf::from("plan.md"),
        content: "# Ready".to_owned(),
        error: None,
    });
    assert_eq!(plan_approval_error(&state), None);
}

#[tokio::test]
async fn plan_callback_stays_open_when_its_answer_cannot_be_delivered() {
    let mut runtime = interactive_test_runtime("plan-settings-failure");
    runtime.mode = "plan".to_owned();
    let session_id = runtime.session_id.clone();
    runtime
        .service
        .close_session(&session_id)
        .await
        .expect("session closes");
    let mut state = TuiState::new(&session_id);
    state.plan_review = Some(PlanReviewState {
        path: PathBuf::from("plan.md"),
        content: "# Ready".to_owned(),
        error: None,
    });
    let mut controls = ControlState::new(&session_id);
    controls.begin_turn("turn").expect("turn begins");
    let pending = PendingCallback {
        callback_id: "plan-callback".to_owned(),
        session_id,
        turn_id: "turn".to_owned(),
        prompt: "Approve plan?".to_owned(),
        request: CallbackRequest::PlanReview {
            questions: vec![CallbackQuestion {
                header: "Plan".to_owned(),
                question: "Approve?".to_owned(),
                options: vec![CallbackOption {
                    id: "manual".to_owned(),
                    label: "Yes, and request approval for edits".to_owned(),
                    description: String::new(),
                }],
                allows_free_text: true,
                multi_select: false,
            }],
            footer_note: None,
            plan_path: PathBuf::from("plan.md"),
        },
    };
    controls
        .present_callback(pending.clone())
        .expect("plan callback presents");

    respond_to_pending_callback(
        &mut runtime,
        &mut controls,
        &pending,
        CallbackChoice::Option {
            id: "manual".to_owned(),
        },
        &mut state,
    );

    // The review tool switches the profile inside the turn, so an answer
    // that never reached it leaves the session in plan mode.
    assert_eq!(runtime.mode, "plan");
    assert!(controls.contains_callback("plan-callback"));
}

#[test]
fn a_callback_waits_for_the_typing_pause_before_it_shows() {
    let mut state = TuiState::new("session");
    let mut controls = ControlState::new("session");
    controls.begin_turn("turn").expect("turn begins");
    controls
        .present_callback(PendingCallback {
            callback_id: "typed-over".to_owned(),
            session_id: "session".to_owned(),
            turn_id: "turn".to_owned(),
            prompt: "Approve?".to_owned(),
            request: CallbackRequest::Approval {
                options: Vec::new(),
                effect: CallbackEffect {
                    tool_name: "test".to_owned(),
                    summary: String::new(),
                    content: String::new(),
                    permissions: Vec::new(),
                },
            },
        })
        .expect("callback presents");
    state.last_keystroke_ms = 10_000;

    hold_for_typing(&mut controls, &mut state, 10_400);
    sync_callback_presentation(&controls, &mut state, 10_400);
    assert!(state.typing_hold);
    assert!(state.callback.is_none());

    hold_for_typing(&mut controls, &mut state, 11_000);
    sync_callback_presentation(&controls, &mut state, 11_000);
    assert!(!state.typing_hold);
    assert!(state.callback.is_some());
    assert!(!controls.input_is_ready(11_000));

    state.last_keystroke_ms = 11_100;
    hold_for_typing(&mut controls, &mut state, 11_200);
    assert!(!state.typing_hold, "a revealed callback stays revealed");
}

#[test]
fn committed_callback_driver_error_reconciles_stale_controls() {
    let mut runtime = interactive_test_runtime("callback-race-session");
    let session_id = runtime.session_id.clone();
    let mut state =
        canonical_session_projection(&mut runtime, &session_id, false).expect("initial projection");
    let mut controls = ControlState::new(&session_id);
    controls.begin_turn("turn").expect("turn begins");
    controls
        .present_callback(PendingCallback {
            callback_id: "committed-callback".to_owned(),
            session_id,
            turn_id: "turn".to_owned(),
            prompt: "Approve?".to_owned(),
            request: CallbackRequest::Approval {
                options: Vec::new(),
                effect: CallbackEffect {
                    tool_name: "test".to_owned(),
                    summary: String::new(),
                    content: String::new(),
                    permissions: Vec::new(),
                },
            },
        })
        .expect("local callback presents");
    assert_eq!(controls.focus, ControlFocus::Callback);

    recover_from_callback_response_error(
        &mut runtime,
        &mut controls,
        &mut state,
        "committed-callback",
        "driver failed after commit",
    );

    assert!(controls.pending_callback().is_none());
    assert_eq!(controls.focus, ControlFocus::Prompt);
    assert!(
        state
            .diagnostics()
            .any(|diagnostic| diagnostic.contains("driver failed after commit"))
    );
}
