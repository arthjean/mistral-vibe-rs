use super::*;

#[test]
fn a_child_is_named_after_its_parent_and_the_spawning_action() {
    let id = child_id("session", "action");
    assert!(id.starts_with("child-"));
    assert_eq!(id.len(), "child-".len() + 24);
    assert_eq!(id, child_id("session", "action"));
    assert_ne!(id, child_id("session", "other"));
}

fn spawn_entry(child: &str, name: &str, created_at: u64) -> Value {
    json!({
        "id": format!("effect-{name}"),
        "type": "effect",
        "createdAt": created_at,
        "updatedAt": created_at,
        "state": {"status": "completed"},
        "detail": {
            "kind": "subagent",
            "toolName": "subagent.spawn",
            "childSessionId": child,
            "input": {"task": "Work", "agent": "generic"},
            "display": {"message": name},
        },
    })
}

fn session(created_at: u64, updated_at: u64) -> Value {
    json!({
        "createdAt": created_at,
        "updatedAt": updated_at,
        "status": {"type": "idle"},
        "tokenUsage": null,
        "contextUsage": null,
        "title": null,
    })
}

#[test]
fn a_summary_takes_its_name_from_the_spawn_that_links_it() {
    let mut directory = ChildDirectory::default();
    directory
        .sessions
        .insert("child-aabbccdd".to_owned(), session(5, 6));
    let unlinked = directory.summaries();
    assert_eq!(unlinked[0]["name"], "child-aa");
    assert_eq!(unlinked[0]["agentType"], "subagent");
    assert_eq!(unlinked[0]["tokenUsage"]["totalTokens"], 0);
    directory.observe(&[spawn_entry("child-aabbccdd", "helper", 4)]);
    let linked = directory.summaries();
    assert_eq!(linked[0]["name"], "helper");
    assert_eq!(linked[0]["agentType"], "generic");
}

#[test]
fn a_completed_stop_archives_the_child_it_names() {
    let mut directory = ChildDirectory::default();
    directory
        .sessions
        .insert("child-aabbccdd".to_owned(), session(5, 6));
    let stop = json!({
        "id": "effect-stop",
        "type": "effect",
        "createdAt": 10,
        "updatedAt": 12,
        "state": {"status": "completed"},
        "detail": {"kind": "tool", "toolName": "subagent.stop", "input": {"agentName": "helper"}},
    });
    directory.observe(&[spawn_entry("child-aabbccdd", "helper", 4), stop]);
    let summary = &directory.summaries()[0];
    assert_eq!(summary["status"], json!({"type": "archived"}));
    assert_eq!(summary["updatedAt"], 12);
}
