//! The scratchpad a session's runtime owns: opened once with its tools, named
//! by its prompt without touching the disk, and removed when the runtime ends
//! (reference `vibe/core/agent_loop/_loop.py`, `init_scratchpad` at the loop's
//! construction and `cleanup_scratchpad` in `aclose`).

use super::*;

/// The scratchpad `session_id`'s runtime holds.
fn scratchpad_of(server: &AppServer, session_id: &str) -> PathBuf {
    server
        .lock_sessions()
        .get(session_id)
        .and_then(|session| session.scratchpad.as_ref())
        .map(|scratchpad| scratchpad.path().to_owned())
        .expect("the session holds a scratchpad")
}

/// Reference `AgentLoop.aclose` removes the scratchpad on every runtime
/// shutdown, so a client that goes away without `session/close` leaves none
/// of its sessions' directories behind.
#[test]
fn a_dropped_connection_removes_the_scratchpads_of_its_sessions() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    for (id, session) in [(2, "session-1"), (3, "session-2")] {
        connection.dispatch(&request(
            id,
            "session/start",
            json!({"sessionId": session, "workingDirectory": "/workspace"}),
        ));
    }
    let first = scratchpad_of(&server, "session-1");
    let second = scratchpad_of(&server, "session-2");
    assert_ne!(first, second);
    assert!(first.is_dir() && second.is_dir());
    fs::write(first.join("notes.md"), "left by a tool").expect("a scratchpad file");

    drop(connection);

    assert!(!first.exists(), "{}", first.display());
    assert!(!second.exists(), "{}", second.display());
}

/// A unified session runs no agent loop, so it opens no scratchpad under the
/// system temporary directory: its harness keeps its notes in its own store.
#[test]
fn a_unified_session_opens_no_scratchpad() {
    let temporary = crate::test_root::tempdir().expect("tempdir");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(&working_directory).expect("workspace");
    let workspace = WorkspaceService::for_runtime_session_root(
        temporary.path().join("sessions"),
        working_directory.clone(),
    );
    let server = AppServer::with_workspace_service(workspace)
        .using_harness_selection(crate::harness::HarnessSelection::resolve(true, false));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "session-1", "workingDirectory": working_directory}),
    ));

    let sessions = server.lock_sessions();
    let session = sessions.get("session-1").expect("the unified session");
    assert!(session.scratchpad.is_none());
    assert_eq!(
        session.tools.guard().and_then(|guard| guard.scratchpad),
        None
    );
}

/// The scope a prompt is composed for names the runtime's scratchpad, the one
/// its guard grants, and building it creates nothing: a directory removed
/// under the session stays removed.
#[test]
fn building_a_prompt_scope_touches_no_file() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let scratchpad = scratchpad_of(&server, "session-1");
    fs::remove_dir(&scratchpad).expect("the scratchpad is removed under the session");

    let (scope, granted) = {
        let sessions = server.lock_sessions();
        let session = sessions.get("session-1").expect("the session");
        (
            crate::server::turns::prompt_scope(session, None),
            session.tools.guard().and_then(|guard| guard.scratchpad),
        )
    };

    assert_eq!(scope.scratchpad.as_ref(), Some(&scratchpad));
    assert_eq!(granted.as_ref(), Some(&scratchpad));
    assert!(!scratchpad.exists(), "{}", scratchpad.display());
}

/// A cleared history continues the same runtime under a new identifier, and
/// reference `_reset_session` keeps that runtime's scratchpad: the new
/// identifier opens no second directory, and the one the runtime holds goes
/// when its connection does, so nothing named after either identifier stays.
#[test]
fn a_cleared_session_keeps_one_scratchpad_and_leaves_none_behind() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(&working_directory).expect("workspace");
    let workspace = WorkspaceService::for_runtime_session_root(
        temporary.path().join("sessions"),
        working_directory.clone(),
    );
    let store = workspace.session_store();
    let mut saved = store
        .create("session-1", &working_directory.to_string_lossy(), None, 1)
        .expect("saved session");
    store
        .append_message(&mut saved, &ModelMessage::user("earlier"), 2)
        .expect("the session is saved");
    let server = AppServer::with_workspace_service(workspace);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "session-1", "resume": "session-1"}),
    ));
    let before = scratchpad_of(&server, "session-1");

    let cleared = connection.dispatch(&request(
        3,
        "session/history/clear",
        json!({"sessionId": "session-1"}),
    ));
    assert!(
        matches!(
            decode_frame(&cleared.outbound[0]).expect("the clear answer"),
            Envelope::Success(_)
        ),
        "{:?}",
        decode_frame(&cleared.outbound[0])
    );
    let (new_id, scope) = {
        let sessions = server.lock_sessions();
        let session = sessions.get("session-1").expect("the cleared session");
        (
            session.id.clone(),
            crate::server::turns::prompt_scope(session, None),
        )
    };
    assert_ne!(new_id, "session-1");
    assert_eq!(scratchpad_of(&server, &new_id), before);
    assert_eq!(scope.scratchpad.as_ref(), Some(&before));
    assert!(before.is_dir());

    drop(connection);

    assert!(!before.exists(), "{}", before.display());
}
