use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use super::*;

fn child(id: &str, status: &str, created_at: i64) -> Value {
    json!({
        "id": id,
        "name": format!("{id}_worker"),
        "agentType": "codeReviewer",
        "status": {"type": status, "activeTurnId": "turn"},
        "tokenUsage": {},
        "contextUsage": {"totalTokens": 12_345},
        "createdAt": created_at,
        "updatedAt": created_at,
    })
}

fn children(values: &[Value]) -> Vec<ChildSession> {
    children_of(&json!({"childSessions": values}))
}

fn press(subagents: &mut Subagents, code: KeyCode) -> ListKey {
    subagents.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn names_read_as_capitalized_words() {
    assert_eq!(display_name("codeReviewer"), "Code Reviewer");
    assert_eq!(display_name("explore_agent-two"), "Explore Agent Two");
    assert_eq!(display_name("HTTPServer"), "HTTP Server");
    assert_eq!(display_name("already spaced"), "already spaced");
}

#[test]
fn children_are_ordered_by_creation_then_identifier() {
    let ordered = children(&[
        child("b", "idle", 2),
        child("c", "idle", 1),
        child("a", "idle", 2),
    ]);
    let ids = ordered
        .iter()
        .map(|child| child.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["c", "a", "b"]);
    assert_eq!(ordered[0].context_tokens, 12_345);
}

#[test]
fn a_finished_batch_keeps_only_its_ready_children() {
    let mut subagents = Subagents::default();
    subagents.sync(children(&[
        child("a", "running", 1),
        child("b", "failed", 2),
    ]));
    assert_eq!(
        subagents.shown().len(),
        2,
        "a working batch shows every child"
    );
    subagents.sync(children(&[child("a", "idle", 1), child("b", "failed", 2)]));
    let shown = subagents
        .shown()
        .iter()
        .map(|child| child.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        shown,
        ["a"],
        "once the batch is done, only a ready child stays"
    );
}

#[test]
fn the_list_takes_the_keyboard_and_opens_a_child_read_only() {
    let mut subagents = Subagents::default();
    subagents.sync(children(&[child("a", "running", 1)]));
    assert_eq!(press(&mut subagents, KeyCode::Down), ListKey::Ignored);
    assert!(subagents.focus_first());
    assert_eq!(press(&mut subagents, KeyCode::Char('x')), ListKey::Handled);
    assert_eq!(press(&mut subagents, KeyCode::Down), ListKey::Handled);
    assert_eq!(press(&mut subagents, KeyCode::Enter), ListKey::Handled);
    assert_eq!(subagents.viewed().map(|child| child.id.as_str()), Some("a"));
    assert_eq!(subagents.take_refresh().as_deref(), Some("a"));
    assert_eq!(
        subagents.view_entries().map(|entries| entries.len()),
        Some(1),
        "the transcript reads as loading until its history lands"
    );
    // Up stops at the main row while a child is viewed.
    press(&mut subagents, KeyCode::Up);
    assert_eq!(press(&mut subagents, KeyCode::Up), ListKey::Handled);
    assert!(subagents.focused());
    subagents.show_main(true);
    assert!(!subagents.is_viewing());
    assert!(!subagents.focused());
}

#[test]
fn a_child_that_finishes_says_it_is_ready_once() {
    let mut subagents = Subagents::default();
    subagents.sync(children(&[child("a", "running", 1)]));
    subagents.focus_first();
    subagents.select_row(1);
    subagents.replace_history("a", Vec::new(), true);
    subagents.sync(children(&[child("a", "idle", 1)]));
    subagents.sync(children(&[child("a", "idle", 1)]));
    let entries = subagents.view_entries().unwrap_or_default();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, READY_MESSAGE);
    subagents.refuse_input();
    assert_eq!(
        subagents.view_entries().map(|entries| entries.len()),
        Some(2)
    );
}

#[test]
fn a_viewed_child_that_disappears_returns_to_the_conversation() {
    let mut subagents = Subagents::default();
    subagents.sync(children(&[child("a", "running", 1)]));
    subagents.focus_first();
    subagents.select_row(1);
    assert!(subagents.is_viewing());
    subagents.sync(Vec::new());
    assert!(!subagents.is_viewing());
    assert!(!subagents.visible());
}

#[test]
fn a_tail_that_meets_the_shown_history_is_merged_onto_it() {
    let entry = |id: &str| TranscriptEntry {
        id: id.to_owned(),
        revision: 1,
        kind: TranscriptKind::AssistantMessage,
        text: id.to_owned(),
        status: EntryStatus::Completed,
        source: EntrySource::Restored,
    };
    let merged = merge_tail(
        &[entry("1"), entry("2"), entry("3")],
        vec![entry("3"), entry("4")],
    );
    let ids = merged
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["1", "2", "3", "4"]);
    assert!(merge_tail(&[entry("1")], Vec::new()).is_empty());
}
