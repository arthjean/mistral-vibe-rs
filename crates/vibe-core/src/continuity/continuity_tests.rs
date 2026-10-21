use super::*;

fn user(content: &str) -> ModelMessage {
    ModelMessage::user(content.to_owned())
}

#[test]
fn handoff_preserves_routes_children_resources_and_durable_deduplication() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = SessionStore::new(temporary.path()).with_pointer_key("test");
    let mut metadata = store
        .create("root-session", "/workspace", None, 1)
        .expect("root session");
    store
        .append_message(&mut metadata, &user("before"), 2)
        .expect("message appends");
    let continuity = SessionContinuity::new(store.clone());
    continuity
        .attach(store.load("root-session").expect("root loads"))
        .expect("root attaches");
    continuity
        .bind_callback(
            "root-session",
            CallbackRoute {
                callback_id: "callback-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        )
        .expect("callback binds");
    continuity
        .add_child("root-session", "child-1")
        .expect("child registers");
    continuity
        .add_resource("root-session", "terminal-1")
        .expect("resource registers");
    assert!(
        continuity
            .accept_operation("root-session", "operation-1", 1)
            .expect("operation accepted")
    );

    let handoff = continuity
        .handoff(
            "root-session",
            "next-session",
            "current prompt",
            BTreeMap::new(),
            10,
        )
        .expect("handoff commits");
    assert_eq!(handoff.parent_session_id.as_deref(), Some("root-session"));
    assert!(handoff.child_session_ids.contains("child-1"));
    assert!(handoff.callback_routes.contains_key("callback-1"));
    assert!(handoff.resources.contains("terminal-1"));
    assert_eq!(
        continuity
            .snapshot("root-session")
            .expect("old alias still resolves")
            .session_id,
        "next-session"
    );
    assert!(
        !continuity
            .accept_operation("root-session", "operation-1", 2)
            .expect("duplicate ignored")
    );

    let restarted = SessionContinuity::new(store.clone());
    let recovered = restarted
        .reconnect("next-session")
        .expect("new session reconnects");
    assert!(recovered.completed_operations.contains("operation-1"));
    assert_eq!(recovered.watermark, 1);
}

#[test]
fn concurrent_operation_acceptance_is_serialized_and_deduplicated() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let store = SessionStore::new(temporary.path());
    let metadata = store
        .create("root", "/workspace", None, 1)
        .expect("root session");
    let continuity = SessionContinuity::new(store);
    continuity
        .attach(HydratedSession {
            metadata,
            messages: Vec::new(),
            current_config: BTreeMap::new(),
        })
        .expect("continuity attaches");
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let continuity = continuity.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            continuity.accept_operation("root", "same-operation", 1)
        }));
    }
    barrier.wait();
    let mut outcomes = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker joins").expect("operation"))
        .collect::<Vec<_>>();
    outcomes.sort_unstable();
    assert_eq!(outcomes, [false, true]);
    let reconnected = SessionContinuity::new(continuity.store.clone())
        .reconnect("root")
        .expect("durable reconnect");
    assert_eq!(reconnected.watermark, 1);
    assert!(reconnected.completed_operations.contains("same-operation"));
}

#[test]
fn rewind_and_clear_match_persistence_after_restart() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = SessionStore::new(temporary.path());
    let mut metadata = store
        .create("root-session", "/workspace", None, 1)
        .expect("root session");
    for (index, content) in ["one", "two", "three"].into_iter().enumerate() {
        store
            .append_message(
                &mut metadata,
                &user(content),
                u64::try_from(index).unwrap_or_default().saturating_add(2),
            )
            .expect("message appends");
    }
    let continuity = SessionContinuity::new(store.clone());
    continuity
        .attach(store.load("root-session").expect("session loads"))
        .expect("session attaches");
    let rewind = continuity
        .rewind("root-session", 1, BTreeMap::new(), 10)
        .expect("rewind persists");
    assert_eq!(rewind.messages, [user("one")]);
    continuity
        .clear_history("root-session", 11)
        .expect("history clears");
    assert!(
        store
            .load("root-session")
            .expect("restart loads")
            .messages
            .is_empty()
    );
}

#[test]
fn a_conflicting_callback_keeps_the_route_already_bound() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let store = SessionStore::new(temporary.path());
    let metadata = store
        .create("root", "/workspace", None, 1)
        .expect("root session");
    let continuity = SessionContinuity::new(store.clone());
    continuity
        .attach(HydratedSession {
            metadata,
            messages: Vec::new(),
            current_config: BTreeMap::new(),
        })
        .expect("root attaches");
    let route = |turn_id: &str| CallbackRoute {
        callback_id: "callback-1".to_owned(),
        turn_id: turn_id.to_owned(),
    };
    continuity
        .bind_callback("root", route("turn-1"))
        .expect("callback binds");

    assert!(matches!(
        continuity.bind_callback("root", route("turn-2")),
        Err(ContinuityError::CallbackConflict)
    ));
    let held = continuity.snapshot("root").expect("root attached");
    assert_eq!(held.callback_routes["callback-1"], route("turn-1"));
    let restarted = SessionContinuity::new(store);
    let recovered = restarted.reconnect("root").expect("root reconnects");
    assert_eq!(recovered.callback_routes["callback-1"], route("turn-1"));
}
