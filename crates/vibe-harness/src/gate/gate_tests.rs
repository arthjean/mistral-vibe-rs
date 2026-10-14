use super::*;

fn action() -> ToolAction {
    ToolAction {
        action_id: "a1".to_owned(),
        turn_id: "turn-1".to_owned(),
        tool_name: "file_system.bash".to_owned(),
        arguments: json!({"command": "touch x"}),
    }
}

#[test]
fn an_approval_callback_gates_the_effect_of_its_action() {
    let entry = approval_callback(
        "s",
        "approval-c1",
        &action(),
        5,
        Some("user-1"),
        None,
        Vec::new(),
    );
    assert_eq!(entry["relatedEntryId"], "effect-a1");
    assert_eq!(entry["detail"]["relatedEntryId"], "effect-a1");
    assert_eq!(
        entry["detail"]["effect"]["input"],
        json!({"command": "touch x"})
    );
    assert_eq!(entry["state"], json!({"status": "open"}));
}

#[test]
fn the_latest_open_callback_blocks_the_session() {
    let mut book = CallbackBook::default();
    let _first = book.open(approval_callback(
        "s",
        "approval-c1",
        &action(),
        1,
        None,
        None,
        Vec::new(),
    ));
    let state = HarnessState::new("s", None, "s", 0);
    let shown = CallbackBook::overlay(&book.open, &state);
    assert_eq!(
        shown.session.status,
        SessionStatus::Blocked {
            active_turn_id: "turn-1".to_owned(),
            callback_id: "approval-c1".to_owned(),
            callback_kind: "approval".to_owned(),
        }
    );
    assert_eq!(shown.active_callbacks.len(), 1);
}

#[test]
fn closing_a_callback_answers_its_waiter() {
    let mut book = CallbackBook::default();
    let mut waiter = book.open(approval_callback(
        "s",
        "approval-c1",
        &action(),
        1,
        None,
        None,
        Vec::new(),
    ));
    let settled = book.close_all("Turn interrupted", 9);
    assert_eq!(
        settled[0]["state"],
        json!({"status": "cancelled", "reason": "Turn interrupted"})
    );
    assert_eq!(settled[0]["generationStatus"], "completed");
    assert_eq!(
        waiter.try_recv().ok(),
        Some(CallbackAnswer::Closed("Turn interrupted".to_owned()))
    );
    assert!(!book.is_open("approval-c1"));
}

#[test]
fn a_taken_identifier_gains_a_suffix() {
    let mut book = CallbackBook::default();
    let _waiter = book.open(approval_callback(
        "s",
        "approval-c1",
        &action(),
        1,
        None,
        None,
        Vec::new(),
    ));
    assert_ne!(book.fresh_id("approval-c1"), "approval-c1");
    assert_eq!(book.fresh_id("approval-c2"), "approval-c2");
}

#[test]
fn a_settled_answer_waits_for_its_release() {
    let mut book = CallbackBook::default();
    let mut waiter = book.open(approval_callback(
        "s",
        "approval-c1",
        &action(),
        1,
        None,
        None,
        Vec::new(),
    ));
    let output = json!({"type": "approval", "decision": {"type": "approve"}});
    let entry = book
        .settle("approval-c1", CallbackAnswer::Output(output.clone()), 2)
        .unwrap();
    assert_eq!(
        entry["state"],
        json!({"status": "answered", "output": output})
    );
    assert!(waiter.try_recv().is_err());
    book.release("approval-c1");
    assert_eq!(waiter.try_recv().ok(), Some(CallbackAnswer::Output(output)));
}
