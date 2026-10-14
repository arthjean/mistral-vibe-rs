use serde_json::json;

use super::*;

fn item(id: &str, content: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        id: id.to_owned(),
        content: content.to_owned(),
        status,
    }
}

#[test]
fn the_summary_names_the_focus_and_leaves_cancellations_out_of_the_count() {
    let mut tracker = TodoTracker::default();
    assert_eq!(tracker.summary(), None);
    tracker.record(vec![
        item("1", "Parse", TodoStatus::Completed),
        item("2", "Build", TodoStatus::Pending),
        item("3", "Ship", TodoStatus::InProgress),
        item("4", "Drop", TodoStatus::Cancelled),
    ]);
    assert_eq!(tracker.summary().as_deref(), Some("▶ 1/3 · Ship"));
    tracker.record(vec![item("1", "Parse", TodoStatus::Completed)]);
    assert_eq!(
        tracker.summary().as_deref(),
        Some("☑ 1/1 · All todos complete")
    );
    tracker.record(vec![item("4", "Drop", TodoStatus::Cancelled)]);
    assert_eq!(
        tracker.summary().as_deref(),
        Some("☒ 0/0 · All todos cancelled")
    );
}

#[test]
fn a_change_counts_additions_removals_and_transitions_in_order() {
    let previous = vec![
        item("1", "a", TodoStatus::Pending),
        item("2", "b", TodoStatus::InProgress),
        item("3", "c", TodoStatus::Pending),
    ];
    let current = vec![
        item("1", "a", TodoStatus::InProgress),
        item("2", "b", TodoStatus::Completed),
        item("4", "d", TodoStatus::Pending),
    ];
    assert_eq!(
        summarize_change(&previous, &current),
        "+1 · \u{2212}1 · 1 started · 1 completed · 1/3 done"
    );
    assert_eq!(
        summarize_change(&current, &current),
        "no changes · 1/3 done"
    );
    assert_eq!(summarize_change(&current, &[]), "Cleared todos");
    assert_eq!(summarize_change(&[], &[]), "No todos");
}

#[test]
fn only_an_output_that_validates_yields_a_list() {
    assert_eq!(parse_output(&json!({})), Some(Vec::new()));
    assert_eq!(
        parse_output(&json!({"todos": [{"id": "1", "content": "a"}]})),
        Some(vec![item("1", "a", TodoStatus::Pending)])
    );
    assert_eq!(parse_output(&json!(null)), None);
    assert_eq!(parse_output(&json!({"todos": [], "verb": "Updated"})), None);
    assert_eq!(
        parse_output(&json!({"todos": [{"id": "1", "content": "a", "status": "done"}]})),
        None
    );
    assert_eq!(
        parse_output(&json!({"todos": [{"id": "1", "content": "a", "priority": "urgent"}]})),
        None
    );
}
