use serde_json::json;

use super::*;

fn state(active: Option<&str>, history: &[Value]) -> Value {
    let status = active.map_or_else(
        || json!({"type": "idle"}),
        |turn| json!({"type": "running", "activeTurnId": turn}),
    );
    json!({"session": {"status": status}, "history": history})
}

fn user(id: &str, turn: &str) -> Value {
    json!({"type": "message", "role": "user", "id": id, "turnId": turn})
}

fn write(id: &str, turn: &str, path: &str, previous: Option<&str>) -> Value {
    json!({
        "type": "effect",
        "id": id,
        "turnId": turn,
        "detail": {"kind": "file_write", "input": {"filePath": path}},
        "state": {"status": "completed", "output": {
            "filePath": path,
            "content": "",
            "fileExisted": previous.is_some(),
            "previousContent": previous,
        }},
    })
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows: the rewind file list publishes verbatim paths (US-010)"
)]
fn a_rewind_restores_every_file_its_turns_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let mut snapshots = RewindSnapshots::default();
    snapshots.observe(&state(None, &[]), &cwd);

    let one = [user("u1", "t1")];
    snapshots.observe(&state(Some("t1"), &one), &cwd);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let one = [user("u1", "t1"), write("e1", "t1", "a.txt", None)];
    snapshots.observe(&state(Some("t1"), &one), &cwd);
    snapshots.observe(&state(None, &one), &cwd);

    let mut two = one.to_vec();
    two.push(user("u2", "t2"));
    snapshots.observe(&state(Some("t2"), &two), &cwd);
    std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
    two.push(write("e2", "t2", "a.txt", Some("one\n")));
    two.push(write("e3", "t2", "b.txt", None));
    snapshots.observe(&state(Some("t2"), &two), &cwd);
    snapshots.observe(&state(None, &two), &cwd);

    let a = dir.path().join("a.txt").to_string_lossy().into_owned();
    let b = dir.path().join("b.txt").to_string_lossy().into_owned();
    assert_eq!(snapshots.restorable_paths("u2"), vec![a.clone(), b.clone()]);
    assert_eq!(snapshots.restorable_paths("u1"), vec![a.clone(), b.clone()]);
    assert!(snapshots.restorable_paths("missing").is_empty());

    let restored = snapshots.restore("u2");
    assert_eq!(restored.paths, vec![a, b]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!dir.path().join("b.txt").exists());

    snapshots.drop_from("u2");
    assert!(snapshots.restorable_paths("u2").is_empty());
    assert_eq!(snapshots.restorable_paths("u1").len(), 1);
}
