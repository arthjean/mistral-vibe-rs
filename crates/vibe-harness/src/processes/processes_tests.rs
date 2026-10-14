use std::time::Duration;

use serde_json::json;

use super::*;

fn structured(result: ToolResult) -> Value {
    match result {
        ToolResult::Success {
            structured_content: Some(value),
            ..
        } => value,
        other => unreachable!("expected a success, got {other:?}"),
    }
}

#[test]
fn a_process_id_hashes_the_session_and_the_call() {
    let id = process_id("session", "action", "call");
    assert!(id.starts_with(&format!("process-{}-", &hex_digest(b"session")[..12])));
    assert_eq!(id.len(), "process-".len() + 12 + 1 + 24);
    assert_eq!(id, process_id("session", "action", "call"));
}

#[cfg(unix)]
#[test]
fn a_process_runs_on_a_terminal_and_pages_its_output() {
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let started = structured(manager.start(
        "process-a".to_owned(),
        &json!({"command": "printf 'one\\ntwo\\n'"}),
        workspace.path(),
        None,
    ));
    assert_eq!(started["status"], "running");
    let deadline = Instant::now() + Duration::from_secs(10);
    while manager.public()[0]["status"] == "running" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let all = structured(manager.output(&json!({"processId": "process-a"})));
    assert_eq!(all["output"], "one\r\ntwo\r\n");
    assert_eq!(all["status"], "completed");
    assert_eq!(all["exitCode"], 0);
    let tail = structured(
        manager.output(&json!({"processId": "process-a", "from": "end", "maxBytes": 4})),
    );
    assert_eq!(
        (
            tail["output"].as_str(),
            tail["hasMore"].as_bool(),
            tail["nextCursor"].as_u64()
        ),
        (Some("wo\r\n"), Some(true), Some(10))
    );
    let page =
        structured(manager.output(&json!({"processId": "process-a", "cursor": 2, "maxBytes": 3})));
    assert_eq!(
        (page["output"].as_str(), page["nextCursor"].as_u64()),
        (Some("e\r\n"), Some(5))
    );
    let notifications = manager.take_notifications();
    assert_eq!(
        notifications[0]["message"],
        "Background process process-a completed."
    );
}

#[cfg(unix)]
#[test]
fn a_stopped_process_reports_stopped_and_refuses_input() {
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let _ = manager.start(
        "process-b".to_owned(),
        &json!({"command": "sleep 30"}),
        workspace.path(),
        None,
    );
    let stopped = structured(manager.stop(&json!({"processId": "process-b"})));
    assert_eq!(
        stopped,
        json!({"processId": "process-b", "status": "stopped", "exitCode": null})
    );
    let ToolResult::Failure { code, .. } =
        manager.write(&json!({"processId": "process-b", "text": "x"}))
    else {
        unreachable!("a stopped process takes no input");
    };
    assert_eq!(code, "process_not_running");
    let ToolResult::Failure { code, .. } = manager.output(&json!({"processId": "process-missing"}))
    else {
        unreachable!("an unknown process has no output");
    };
    assert_eq!(code, "unknown_process");
}
