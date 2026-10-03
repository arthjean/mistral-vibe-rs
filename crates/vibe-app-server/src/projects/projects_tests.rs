use serde_json::json;
use tempfile::tempdir;

use super::*;

fn params(value: Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .expect("object")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[test]
fn session_rebind_transfers_loop_ownership() {
    let temporary = tempdir().expect("temporary directory");
    let service = ProjectsService::default()
        .with_loop_store(temporary.path().join("loops.json"))
        .expect("loop store");
    service
        .dispatch(
            "loops/create",
            &params(json!({
                "sessionId": "session-old",
                "prompt": "review",
                "interval": "30s",
                "nowSeconds": 10
            })),
        )
        .expect("loop creates");

    service
        .rebind_session("session-old", "session-new")
        .expect("state rebinds");

    let listed = service
        .dispatch("loops/list", &params(json!({"sessionId": "session-new"})))
        .expect("new session owns loop");
    assert_eq!(listed.result["loops"].as_array().map(Vec::len), Some(1));
    let old = service
        .dispatch("loops/list", &params(json!({"sessionId": "session-old"})))
        .expect("old session has no loops");
    assert_eq!(old.result["loops"].as_array().map(Vec::len), Some(0));
}

#[test]
fn scheduled_loops_are_owned_persistent_and_retry_safe() {
    let temporary = tempdir().expect("temporary directory");
    let path = temporary.path().join("loops.json");
    let service = ProjectsService::default()
        .with_loop_store(path.clone())
        .expect("loop store");
    let created = service
        .dispatch(
            "loops/create",
            &params(json!({
                "sessionId": "session-1",
                "prompt": "review",
                "interval": "30s",
                "nowSeconds": 10
            })),
        )
        .expect("create");
    let loop_id = created.result["loop"]["id"]
        .as_str()
        .expect("loop id")
        .to_owned();
    assert_eq!(loop_id.len(), 8);
    assert!(
        loop_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );
    assert!(matches!(
        service.fire_loop(&loop_id, 39, true),
        Err(ProjectsServiceError::Conflict(_))
    ));
    let fired = service.fire_loop(&loop_id, 40, true).expect("due loop");
    assert_eq!(fired.prompt, "review");
    assert!(matches!(
        service.fire_loop(&loop_id, 40, true),
        Err(ProjectsServiceError::Conflict(_))
    ));
    drop(service);

    let reloaded = ProjectsService::default()
        .with_loop_store(path)
        .expect("reload");
    assert!(matches!(
        reloaded.fire_loop(&loop_id, 40, true),
        Err(ProjectsServiceError::Conflict(_))
    ));
    reloaded
        .fire_loop(&loop_id, 70, true)
        .expect("interrupted running state preserves cadence");
    reloaded.finish_loop_fire(&loop_id, 71).expect("finish");
    let listed = reloaded
        .dispatch("loops/list", &params(json!({"sessionId": "session-1"})))
        .expect("list");
    assert_eq!(
        listed.result["loops"][0]["nextFireAt"].as_f64(),
        Some(100.0)
    );
    assert!(matches!(
        reloaded.dispatch(
            "loops/delete",
            &params(json!({"sessionId": "another", "loopId": loop_id}))
        ),
        Err(ProjectsServiceError::Loop(_))
    ));
}

#[test]
fn session_removal_deletes_owned_loops_transactionally_and_durably() {
    let temporary = tempdir().expect("loop store");
    let loop_path = temporary.path().join("loops.json");
    let mut service = ProjectsService::default()
        .with_loop_store(loop_path.clone())
        .expect("loop store");
    for (session_id, prompt) in [
        ("session-delete", "first"),
        ("session-delete", "second"),
        ("session-keep", "keep"),
    ] {
        service
            .dispatch(
                "loops/create",
                &params(json!({
                    "sessionId": session_id,
                    "prompt": prompt,
                    "interval": "30s",
                    "nowSeconds": 10,
                })),
            )
            .expect("loop creates");
    }

    service.loop_store = temporary.path().to_path_buf();
    assert!(matches!(
        service.remove_session("session-delete"),
        Err(ProjectsServiceError::Persistence(_))
    ));
    let unchanged = service
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-delete"})),
        )
        .expect("loops remain after failed persistence");
    assert_eq!(unchanged.result["loops"].as_array().map(Vec::len), Some(2));

    service.loop_store = loop_path.clone();
    assert_eq!(
        service
            .remove_session("session-delete")
            .expect("session removal"),
        2
    );
    drop(service);
    let reloaded = ProjectsService::default()
        .with_loop_store(loop_path)
        .expect("reloaded loop store");
    let deleted = reloaded
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-delete"})),
        )
        .expect("deleted session loops");
    assert_eq!(deleted.result["loops"].as_array().map(Vec::len), Some(0));
    let kept = reloaded
        .dispatch("loops/list", &params(json!({"sessionId": "session-keep"})))
        .expect("unrelated session loops");
    assert_eq!(kept.result["loops"].as_array().map(Vec::len), Some(1));
}

#[test]
fn session_removal_token_restores_loops_durably() {
    let temporary = tempdir().expect("session rollback store");
    let loop_path = temporary.path().join("loops.json");
    let mut service = ProjectsService::default()
        .with_loop_store(loop_path.clone())
        .expect("loop store");
    service
        .dispatch(
            "loops/create",
            &params(json!({
                "sessionId": "session-rollback",
                "prompt": "durable",
                "interval": "30s",
                "nowSeconds": 10,
            })),
        )
        .expect("loop creates");

    let removal = service
        .remove_session_transactional("session-rollback")
        .expect("transactional removal");
    assert_eq!(removal.session_id(), "session-rollback");
    assert_eq!(removal.removed_loop_count(), 1);

    let blocking_path = temporary.path().join("restore-blocked");
    fs::create_dir(&blocking_path).expect("blocking directory");
    service.loop_store = blocking_path;
    assert!(matches!(
        service.restore_session(&removal),
        Err(ProjectsServiceError::Persistence(_))
    ));
    let rolled_back = service
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-rollback"})),
        )
        .expect("loops after failed restoration");
    assert_eq!(
        rolled_back.result["loops"].as_array().map(Vec::len),
        Some(0),
        "failed durable restoration must leave the loops removed"
    );

    service.loop_store = loop_path.clone();
    service
        .restore_session(&removal)
        .expect("session restoration");
    let restored_loops = service
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-rollback"})),
        )
        .expect("restored loops");
    assert_eq!(
        restored_loops.result["loops"].as_array().map(Vec::len),
        Some(1)
    );

    drop(service);
    let reloaded = ProjectsService::default()
        .with_loop_store(loop_path)
        .expect("reloaded restored loops");
    let durable = reloaded
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-rollback"})),
        )
        .expect("durable restored loops");
    assert_eq!(durable.result["loops"].as_array().map(Vec::len), Some(1));
}
