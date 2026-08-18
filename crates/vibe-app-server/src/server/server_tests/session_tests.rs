//! The session lifecycle: settings, compaction, scheduled loops, rewind,
//! review and deletion.

use super::*;

#[test]
fn lifecycle_rejects_duplicate_initialize_and_unsolicited_responses() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    let duplicate = connection.dispatch(&request(
        2,
        "initialize",
        json!({
            "clientInfo": {
                "name": "test",
                "version": "1",
                "entrypoint": "programmatic",
                "terminalEmulator": "unknown"
            }
        }),
    ));
    let frame = decode_frame(&duplicate.outbound[0]).expect("error response");
    assert!(matches!(
        frame,
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidRequest,
                ..
            },
            ..
        })
    ));

    let unsolicited = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": "unknown",
        "result": {}
    }))
    .expect("response fixture");
    assert!(connection.dispatch(&unsolicited).close_after_flush);
    assert_eq!(connection.state(), ConnectionState::Closed);
}

#[test]
fn turn_is_reserved_before_deferred_work_is_exposed() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let batch = connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    assert_eq!(batch.outbound.len(), 1);
    assert_eq!(
        batch.deferred,
        vec![DeferredWork::RunTurn {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "hello".to_owned(),
            input: vec![PublicContentBlock::Text {
                text: "hello".to_owned(),
            }],
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
        }]
    );
    let session = server.session("session-1").expect("reserved session");
    assert_eq!(session.active_turn.as_deref(), Some("turn-1"));
    assert_eq!(session.status, SessionStatus::Running);

    let concurrent = connection.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "second"}]}),
    ));
    let frame = decode_frame(&concurrent.outbound[0]).expect("conflict response");
    assert!(matches!(
        frame,
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Conflict,
                ..
            },
            ..
        })
    ));
}

#[test]
fn settings_update_is_strict_and_applies_to_the_next_turn_while_active() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let turn = connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    assert_eq!(turn.deferred.len(), 1);

    let update = connection.dispatch(&request(
        4,
        "session/settings/update",
        json!({"sessionId": "session-1", "maxTurns": 0, "maxTokens": 64}),
    ));
    assert_eq!(update.outbound.len(), 1);
    // The model is not a reference session setting, so it travels under the
    // local name instead.
    let overridden = connection.dispatch(&request(
        5,
        "session/overrides/write",
        json!({"sessionId": "session-1", "model": "next-model"}),
    ));
    assert_eq!(overridden.outbound.len(), 1);
    let session = server.session("session-1").expect("session");
    assert_eq!(session.intent.max_turns, Some(0));
    assert_eq!(session.intent.max_tokens, Some(64));
    assert_eq!(session.intent.model.as_deref(), Some("next-model"));
    assert_eq!(session.active_turn.as_deref(), Some("turn-1"));

    // US-093: the reference method accepts `sessionId`, `maxTurns` and
    // `maxTokens`, and refuses everything else, including the five fields
    // this port moved to its own method.
    for (id, params) in [
        (6, json!({"sessionId": "session-1"})),
        (7, json!({"sessionId": "session-1", "maxTurns": 1.5})),
        (8, json!({"sessionId": "session-1", "maxTokens": -1})),
        (
            9,
            json!({"sessionId": "session-1", "maxTurns": 1, "future": true}),
        ),
        (10, json!({"sessionId": "session-1", "model": "other"})),
        (11, json!({"sessionId": "session-1", "mode": "plan"})),
        (12, json!({"sessionId": "session-1", "thinking": true})),
        (
            13,
            json!({"sessionId": "session-1", "reasoningEffort": "high"}),
        ),
        (14, json!({"sessionId": "session-1", "autoApprove": true})),
    ] {
        let invalid = decode_frame(
            &connection
                .dispatch(&request(id, "session/settings/update", params))
                .outbound[0],
        )
        .expect("error response");
        assert!(matches!(
            invalid,
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ));
    }
    let active_approval = connection.dispatch(&request(
        15,
        "session/overrides/write",
        json!({"sessionId": "session-1", "autoApprove": true}),
    ));
    assert!(matches!(
        decode_frame(&active_approval.outbound[0]).expect("active approval response"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Conflict,
                ..
            },
            ..
        })
    ));
    let active_agent = connection.dispatch(&request(
        16,
        "session/agent/update",
        json!({"sessionId": "session-1", "name": "plan"}),
    ));
    assert!(matches!(
        decode_frame(&active_agent.outbound[0]).expect("active agent response"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Conflict,
                ..
            },
            ..
        })
    ));
}

#[test]
fn manual_compaction_reserves_exclusive_session_work_and_failure_releases_it() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let compact = connection.dispatch(&request(
        3,
        "session/compact/start",
        json!({"sessionId": "session-1", "extraInstructions": "preserve decisions"}),
    ));
    assert!(compact.outbound.is_empty());
    assert_eq!(
        compact.deferred,
        vec![DeferredWork::CompactSession {
            request_id: RequestId::Integer(3),
            session_id: "session-1".to_owned(),
            extra_instructions: "preserve decisions".to_owned(),
        }]
    );
    let blocked = connection.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "race"}]}),
    ));
    assert!(matches!(
        decode_frame(&blocked.outbound[0]).expect("conflict"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Conflict,
                ..
            },
            ..
        })
    ));

    let failure = server.fail_manual_compaction(
        RequestId::Integer(3),
        "session-1",
        "injected provider failure",
    );
    assert!(matches!(
        decode_frame(&failure.outbound[0]).expect("typed compaction failure"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::CompactionFailed,
                ..
            },
            ..
        })
    ));
    let next = connection.dispatch(&request(
        5,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "retry"}]}),
    ));
    assert_eq!(next.deferred.len(), 1);
}

#[test]
fn due_loop_reserves_the_normal_turn_path_and_emits_a_sequenced_notice() {
    let temporary = tempfile::tempdir().expect("loop store");
    let loop_path = temporary.path().join("loops.json");
    let release4 = Release4Service::default()
        .with_loop_store(loop_path)
        .expect("persistent loop service");
    let created = release4
        .dispatch(
            "loops/create",
            &BTreeMap::from([
                ("sessionId".to_owned(), json!("session-1")),
                ("interval".to_owned(), json!("30s")),
                ("prompt".to_owned(), json!("scheduled prompt")),
                ("nowSeconds".to_owned(), json!(10)),
            ]),
        )
        .expect("loop");
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

    let server = AppServer::with_release4_service(release4);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let scheduled = server
        .reserve_due_loop("session-1", 40)
        .expect("scheduler")
        .expect("due loop");
    assert_eq!(scheduled.fire.loop_id, loop_id);
    assert_eq!(
        scheduled.fire.notice.params["entry"]["turnId"],
        scheduled.fire.notice.params["turnId"]
    );
    // The attachment snapshot opened the sequence at one.
    assert_eq!(scheduled.fire.notice.params["eventId"], 2);
    let DeferredWork::RunTurn {
        turn_id, prompt, ..
    } = scheduled.work
    else {
        return;
    };
    assert_eq!(prompt, "scheduled prompt");
    assert!(
        server
            .reserve_due_loop("session-1", 40)
            .expect("busy scheduler")
            .is_none()
    );
    server
        .fail_turn(
            "session-1",
            &turn_id,
            "injected interruption",
            TurnErrorCode::InternalError,
        )
        .expect("turn releases");
    server
        .finish_scheduled_loop(&loop_id, 41)
        .expect("loop reschedules");
    assert!(
        server
            .reserve_due_loop("session-1", 69)
            .expect("not yet due")
            .is_none()
    );
    assert!(
        server
            .reserve_due_loop("session-1", 70)
            .expect("due again")
            .is_some()
    );
}

#[test]
fn deleting_a_saved_session_removes_its_loops_from_durable_restart_state() {
    let temporary = tempfile::tempdir().expect("session deletion stores");
    let session_root = temporary.path().join("sessions");
    let working_directory = temporary.path().join("workspace");
    let loop_path = temporary.path().join("loops.json");
    fs::create_dir_all(&working_directory).expect("workspace");
    vibe_core::storage::SessionStore::new(&session_root)
        .create(
            "deleted-session",
            &working_directory.to_string_lossy(),
            None,
            1,
        )
        .expect("saved session");
    let release3 =
        Release3Service::for_runtime_session_root(session_root, working_directory.clone());
    let release4 = Release4Service::default()
        .with_loop_store(loop_path.clone())
        .expect("loop store");
    release4
        .dispatch(
            "loops/create",
            &BTreeMap::from([
                ("sessionId".to_owned(), json!("deleted-session")),
                ("interval".to_owned(), json!("30s")),
                ("prompt".to_owned(), json!("orphan check")),
                ("nowSeconds".to_owned(), json!(10)),
            ]),
        )
        .expect("owned loop");
    let server = AppServer::with_release3_service(release3).using_release4_service(release4);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    let deleted = connection.dispatch(&request(
        2,
        "session/delete",
        json!({"sessionId": "deleted-session"}),
    ));
    assert!(matches!(
        decode_frame(&deleted.outbound[0]).expect("delete response"),
        Envelope::Success(SuccessResponse { result, .. })
            if result.get("deleted").and_then(Value::as_bool) == Some(true)
    ));

    let restarted = Release4Service::default()
        .with_loop_store(loop_path)
        .expect("reloaded loop store");
    let listed = restarted
        .dispatch(
            "loops/list",
            &BTreeMap::from([("sessionId".to_owned(), json!("deleted-session"))]),
        )
        .expect("reloaded loops");
    assert_eq!(listed.result["loops"].as_array().map(Vec::len), Some(0));
}

#[test]
fn rewind_read_and_restore_use_live_target_specific_checkpoints() {
    let temporary = tempfile::tempdir().expect("rewind stores");
    let session_root = temporary.path().join("sessions");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(&working_directory).expect("workspace");
    fs::write(working_directory.join("main.txt"), "before\n").expect("workspace fixture");
    let store = vibe_core::storage::SessionStore::new(&session_root);
    let mut metadata = store
        .create(
            "source-session",
            &working_directory.to_string_lossy(),
            None,
            1,
        )
        .expect("source session");
    for (timestamp, content) in [(2, "first"), (3, "restore target"), (4, "latest")] {
        store
            .append_message(
                &mut metadata,
                &ModelMessage::user(content.to_owned()),
                timestamp,
            )
            .expect("user message");
    }
    let release3 =
        Release3Service::for_runtime_session_root(session_root.clone(), working_directory.clone());
    let server = AppServer::with_release3_service(release3);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    let started = connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "source-session", "resume": "source-session"}),
    ));
    assert!(matches!(
        decode_frame(&started.outbound[0]).expect("start response"),
        Envelope::Success(_)
    ));

    let review = server
        .lock_sessions()
        .expect("runtime sessions")
        .get("source-session")
        .and_then(|session| session.review.clone())
        .expect("live review manager");
    let first_turn = connection.dispatch(&request(
        3,
        "turn/start",
        json!({
            "sessionId": "source-session",
            "input": [{"type": "text", "text": "prior live turn"}]
        }),
    ));
    let first_turn_id = first_turn.deferred.iter().find_map(|work| match work {
        DeferredWork::RunTurn { turn_id, .. } => Some(turn_id.clone()),
        _ => None,
    });
    assert!(first_turn_id.is_some(), "first turn is reserved");
    let Some(first_turn_id) = first_turn_id else {
        return;
    };
    store
        .append_message(
            &mut metadata,
            &ModelMessage::user("prior live turn".to_owned()),
            5,
        )
        .expect("persist first live user message");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::Assistant {
                content: "prior answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
            6,
        )
        .expect("persist first live assistant message");
    server
        .fail_turn(
            "source-session",
            &first_turn_id,
            "completed fixture turn",
            TurnErrorCode::InternalError,
        )
        .expect("first turn seals");

    let checkpoint_turn = connection.dispatch(&request(
        4,
        "turn/start",
        json!({
            "sessionId": "source-session",
            "input": [{"type": "text", "text": "restore live target"}]
        }),
    ));
    let checkpoint_turn_id = checkpoint_turn.deferred.iter().find_map(|work| match work {
        DeferredWork::RunTurn { turn_id, .. } => Some(turn_id.clone()),
        _ => None,
    });
    assert!(checkpoint_turn_id.is_some(), "checkpoint turn is reserved");
    let Some(checkpoint_turn_id) = checkpoint_turn_id else {
        return;
    };
    review
        .edit(
            "main.txt",
            &[vibe_core::workspace::EditOperation {
                old_text: "before".to_owned(),
                new_text: "after".to_owned(),
                replace_all: false,
            }],
        )
        .expect("checkpoint edit");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::user("restore live target".to_owned()),
            7,
        )
        .expect("persist checkpoint user message");
    server
        .fail_turn(
            "source-session",
            &checkpoint_turn_id,
            "completed checkpoint fixture",
            TurnErrorCode::InternalError,
        )
        .expect("checkpoint seals");

    let read = connection.dispatch(&request(
        5,
        "session/rewind/read",
        json!({"sessionId": "source-session", "entryId": "history:5:user"}),
    ));
    let decoded = decode_frame(&read.outbound[0]).expect("rewind read response");
    assert!(matches!(decoded, Envelope::Success(_)));
    let Envelope::Success(SuccessResponse { result, .. }) = decoded else {
        return;
    };
    // The read answers the entry it was asked about and nothing else, and
    // the paths come from the session's own checkpoint log.
    assert_eq!(result["hasFileChanges"], json!(true));
    assert_eq!(result["paths"], json!(["main.txt"]));
    assert!(
        crate::app_server_surface_parity_tests::census_issues(
            "session/rewind/read",
            &Value::Object(result.clone().into_iter().collect()),
        )
        .is_empty(),
        "the read diverges from the reference census: {result:?}"
    );

    // A point no turn was captured for restores nothing rather than
    // planning from the nearest turn that was.
    let untouched = connection.dispatch(&request(
        52,
        "session/rewind/read",
        json!({"sessionId": "source-session", "entryId": "history:0:user"}),
    ));
    let Envelope::Success(SuccessResponse { result: quiet, .. }) =
        decode_frame(&untouched.outbound[0]).expect("untouched read")
    else {
        unreachable!("the read answers");
    };
    assert_eq!(quiet["hasFileChanges"], json!(false));
    assert_eq!(quiet["paths"], json!([]));

    let unknown = connection.dispatch(&request(
        51,
        "session/rewind/read",
        json!({"sessionId": "source-session", "entryId": "history:4:user"}),
    ));
    assert!(
        matches!(
            decode_frame(&unknown.outbound[0]).expect("unknown entry"),
            Envelope::Error(_)
        ),
        "an identifier no rewindable entry carries is refused"
    );

    let rejected = connection.dispatch(&request(
        6,
        "session/rewind",
        json!({
            "sessionId": "source-session",
            "entryId": "history:5:user",
            "restoreFiles": true,
            "inplace": "invalid"
        }),
    ));
    assert!(matches!(
        decode_frame(&rejected.outbound[0]).expect("rejected rewind"),
        Envelope::Error(_)
    ));
    assert_eq!(
        fs::read_to_string(working_directory.join("main.txt")).expect("rolled back workspace"),
        "after\n"
    );

    let rewound = connection.dispatch(&request(
        7,
        "session/rewind",
        json!({
            "sessionId": "source-session",
            "entryId": "history:5:user",
            "restoreFiles": true
        }),
    ));
    let decoded = decode_frame(&rewound.outbound[0]).expect("rewind response");
    assert!(matches!(decoded, Envelope::Success(_)));
    let Envelope::Success(SuccessResponse { result, .. }) = decoded else {
        return;
    };
    // `SessionRewindResponse` declares exactly these five fields, and the
    // session a client adopts is named by the state rather than by stored
    // metadata the response no longer carries.
    assert_eq!(
        result.keys().cloned().collect::<Vec<_>>(),
        vec![
            "message".to_owned(),
            "restoreErrors".to_owned(),
            "restoredPaths".to_owned(),
            "sessionLog".to_owned(),
            "state".to_owned()
        ]
    );
    assert!(
        crate::app_server_surface_parity_tests::census_issues(
            "session/rewind",
            &Value::Object(result.clone().into_iter().collect()),
        )
        .is_empty(),
        "the rewind diverges from the reference census: {result:?}"
    );
    let child_id = result["state"]["session"]["id"]
        .as_str()
        .expect("branch id");
    assert_ne!(child_id, "source-session");
    assert_eq!(result["message"], json!("restore live target"));
    assert_eq!(result["restoredPaths"], json!(["main.txt"]));
    assert_eq!(result["restoreErrors"], json!([]));
    assert_eq!(
        fs::read_to_string(working_directory.join("main.txt")).expect("restored workspace"),
        "before\n"
    );
    assert!(store.load("source-session").is_ok());
    assert!(server.session("source-session").is_ok());
    assert!(server.session(child_id).is_ok());
}

/// The three answers a rewind gives that the restoring fork does not: an
/// identifier nothing carries, an in-place rewind that creates no session,
/// and a rewind that was told to leave the workspace alone.
#[test]
fn rewind_refuses_an_unknown_entry_and_honors_inplace_and_untouched_files() {
    let temporary = tempfile::tempdir().expect("rewind stores");
    let session_root = temporary.path().join("sessions");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(&working_directory).expect("workspace");
    fs::write(working_directory.join("main.txt"), "before\n").expect("workspace fixture");
    let store = vibe_core::storage::SessionStore::new(&session_root);
    let mut metadata = store
        .create(
            "source-session",
            &working_directory.to_string_lossy(),
            None,
            1,
        )
        .expect("source session");
    for (timestamp, content) in [(2, "first"), (3, "second")] {
        store
            .append_message(
                &mut metadata,
                &ModelMessage::user(content.to_owned()),
                timestamp,
            )
            .expect("user message");
    }
    let release3 =
        Release3Service::for_runtime_session_root(session_root.clone(), working_directory.clone());
    let server = AppServer::with_release3_service(release3);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "source-session", "resume": "source-session"}),
    ));
    let review = server
        .lock_sessions()
        .expect("runtime sessions")
        .get("source-session")
        .and_then(|session| session.review.clone())
        .expect("live review manager");
    review.begin_turn_at("turn-1", 1).expect("begin turn");
    review
        .edit(
            "main.txt",
            &[vibe_core::workspace::EditOperation {
                old_text: "before".to_owned(),
                new_text: "after".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");

    // An identifier nothing carries is refused the same way whether the
    // restore staging or the rewind itself catches it, and both name it.
    for (id, restore_files) in [(3, false), (31, true)] {
        let unknown = connection.dispatch(&request(
            id,
            "session/rewind",
            json!({
                "sessionId": "source-session",
                "entryId": "history:9:user",
                "restoreFiles": restore_files
            }),
        ));
        let Envelope::Error(ErrorResponse { error, .. }) =
            decode_frame(&unknown.outbound[0]).expect("unknown entry")
        else {
            unreachable!("an entry nothing carries is refused");
        };
        assert_eq!(error.code, ProtocolErrorCode::NotFound);
        assert!(
            error.message.contains("history:9:user"),
            "the refusal names the entry: {}",
            error.message
        );
        assert_eq!(
            fs::read_to_string(working_directory.join("main.txt")).expect("workspace"),
            "after\n",
            "an entry nothing carries restores nothing"
        );
        assert_eq!(
            store
                .load("source-session")
                .expect("source is untouched")
                .messages
                .len(),
            2,
            "an entry nothing carries truncates nothing"
        );
    }

    let rewound = connection.dispatch(&request(
        4,
        "session/rewind",
        json!({
            "sessionId": "source-session",
            "entryId": "history:1:user",
            "inplace": true,
            "restoreFiles": false
        }),
    ));
    let Envelope::Success(SuccessResponse { result, .. }) =
        decode_frame(&rewound.outbound[0]).expect("in-place rewind")
    else {
        unreachable!("the rewind answers");
    };
    assert_eq!(result["restoredPaths"], json!([]));
    assert_eq!(result["restoreErrors"], json!([]));
    assert_eq!(
        result["state"]["session"]["id"],
        json!("source-session"),
        "an in-place rewind stays in the session it was asked about"
    );
    assert_eq!(
        fs::read_to_string(working_directory.join("main.txt")).expect("workspace"),
        "after\n",
        "a rewind told not to restore files leaves the workspace alone"
    );
    assert_eq!(
        store
            .list(None, 0, 100)
            .expect("saved sessions")
            .sessions
            .len(),
        1,
        "no session was forked"
    );
    assert_eq!(
        store
            .load("source-session")
            .expect("truncated source")
            .messages
            .len(),
        1
    );
}

#[test]
fn failed_rewind_attachment_rolls_back_session_and_workspace() {
    let temporary = tempfile::tempdir().expect("rewind rollback stores");
    let session_root = temporary.path().join("sessions");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(&working_directory).expect("workspace");
    fs::write(working_directory.join("main.txt"), "before\n").expect("workspace fixture");
    let store = vibe_core::storage::SessionStore::new(&session_root);
    let mut metadata = store
        .create(
            "source-session",
            &working_directory.to_string_lossy(),
            None,
            1,
        )
        .expect("source session");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::user("restore target".to_owned()),
            2,
        )
        .expect("user message");
    let release3 =
        Release3Service::for_runtime_session_root(session_root.clone(), working_directory.clone());
    let server = AppServer::with_release3_service(release3)
        .using_session_tool_factory(Arc::new(RejectForkTools));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "source-session", "resume": "source-session"}),
    ));
    let review = server
        .lock_sessions()
        .expect("runtime sessions")
        .get("source-session")
        .and_then(|session| session.review.clone())
        .expect("review manager");
    review.begin_turn_at("checkpoint", 0).expect("begin turn");
    review
        .edit(
            "main.txt",
            &[vibe_core::workspace::EditOperation {
                old_text: "before".to_owned(),
                new_text: "after".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");

    let rewound = connection.dispatch(&request(
        3,
        "session/rewind",
        json!({
            "sessionId": "source-session",
            "entryId": "history:0:user",
            "restoreFiles": true
        }),
    ));

    assert!(matches!(
        decode_frame(&rewound.outbound[0]).expect("rewind failure"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::InternalError,
                ..
            },
            ..
        })
    ));
    assert_eq!(
        fs::read_to_string(working_directory.join("main.txt")).expect("rolled back workspace"),
        "after\n"
    );
    let saved = store.list(None, 0, 100).expect("saved sessions").sessions;
    assert_eq!(
        saved
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["source-session"]
    );
    assert!(server.session("source-session").is_ok());
}

/// The six review methods, end to end over a real connection, against the
/// engine the turn boundaries drive. This is the whole point of the epic:
/// the panel used to be answered from a map production never wrote to, so
/// `review/state` published an empty file list in every session and the
/// other five answered `NotFound` for every path.
#[test]
fn the_review_surface_answers_the_six_methods_from_the_session_engine() {
    let temporary = tempfile::tempdir().expect("review surface stores");
    let session_root = temporary.path().join("sessions");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(&working_directory).expect("workspace");
    fs::write(working_directory.join("main.txt"), "one\n").expect("workspace fixture");
    let store = vibe_core::storage::SessionStore::new(&session_root);
    store
        .create(
            "review-session",
            &working_directory.to_string_lossy(),
            None,
            1,
        )
        .expect("source session");
    let release3 =
        Release3Service::for_runtime_session_root(session_root, working_directory.clone());
    let server = AppServer::with_release3_service(release3);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "review-session", "resume": "review-session"}),
    ));
    let review = server
        .lock_sessions()
        .expect("runtime sessions")
        .get("review-session")
        .and_then(|session| session.review.clone())
        .expect("review manager");
    review.begin_turn_at("turn-1", 1).expect("begin turn");
    review
        .edit(
            "main.txt",
            &[vibe_core::workspace::EditOperation {
                old_text: "one\n".to_owned(),
                new_text: "one\ntwo\n".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");

    let answer = |connection: &mut ServerConnection, id: i64, method: &str, params: Value| {
        let batch = connection.dispatch(&request(id, method, params));
        match decode_frame(&batch.outbound[0]).expect("an answer") {
            Envelope::Success(SuccessResponse { result, .. }) => result,
            other => unreachable!("{method} did not answer: {other:?}"),
        }
    };
    let session = json!({"sessionId": "review-session"});

    let state = answer(&mut connection, 3, "review/state", session.clone());
    let files = state["files"].as_array().expect("a file list");
    assert_eq!(files.len(), 1, "the turn's change is reviewable: {state:?}");
    assert_eq!(files[0]["path"], "main.txt");
    assert_eq!(files[0]["status"], "modified");
    let region = &files[0]["regions"][0];
    assert_eq!(region["kind"], "text");
    assert_eq!(region["owner"], json!({"kind": "agent", "turnId": 1}));
    assert_eq!(region["decision"], "pending");
    assert_eq!(region["dependsOn"], json!([]));
    assert_eq!(
        state["scopes"][0]["owner"],
        json!({"kind": "agent", "turnId": 1}),
        "the turn keeps its own review slot"
    );
    assert_eq!(state["scopes"][0]["files"][0]["regionCount"], 1);

    let baseline = answer(
        &mut connection,
        4,
        "review/baseline",
        json!({"sessionId": "review-session", "path": "main.txt"}),
    );
    assert_eq!(baseline["content"], "one\n");

    let hunks = answer(
        &mut connection,
        5,
        "review/hunks",
        json!({"sessionId": "review-session", "path": "main.txt"}),
    );
    assert_eq!(hunks["hunks"].as_array().expect("anchors").len(), 1);
    assert_eq!(hunks["hunks"][0]["side"], "additions");
    assert_eq!(hunks["hunks"][0]["line"], 1);

    let diff = answer(
        &mut connection,
        6,
        "review/turnDiff",
        json!({
            "sessionId": "review-session",
            "path": "main.txt",
            "owner": {"kind": "agent", "turnId": 1}
        }),
    );
    assert_eq!(diff["status"], "modified");
    assert_eq!(diff["baseline"], "one\n");
    assert_eq!(
        diff["current"], "one\ntwo\n",
        "the owner's own change is what turnDiff answers"
    );

    // The region the panel was shown is the region it sends back.
    let target = json!({
        "kind": "region",
        "path": "main.txt",
        "versionIndex": region["versionIndex"],
        "ordinal": region["ordinal"]
    });
    let approved = answer(
        &mut connection,
        7,
        "review/approve",
        json!({"sessionId": "review-session", "target": target}),
    );
    // `EmptyResponse` declares no field, so an empty object is exactly what
    // the census requires of both mutations. They are asserted here rather
    // than in the surface probe, which only calls read-only methods.
    assert!(
        approved.is_empty(),
        "an approval answers nothing: {approved:?}"
    );
    assert_eq!(
        fs::read_to_string(working_directory.join("main.txt")).expect("read"),
        "one\ntwo\n",
        "an approval leaves disk alone"
    );
    let resolved = answer(&mut connection, 8, "review/state", session.clone());
    assert_eq!(
        resolved["files"].as_array().expect("a file list").len(),
        0,
        "the file is resolved once its one region is decided"
    );
    assert_eq!(
        answer(
            &mut connection,
            9,
            "review/baseline",
            json!({"sessionId": "review-session", "path": "main.txt"})
        )["content"],
        "one\ntwo\n",
        "the accepted baseline now carries the kept region"
    );

    // A second turn, reverted whole, is written back to disk.
    review.begin_turn_at("turn-2", 2).expect("begin turn");
    review
        .edit(
            "main.txt",
            &[vibe_core::workspace::EditOperation {
                old_text: "two\n".to_owned(),
                new_text: "two\nthree\n".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");
    let reverted = answer(
        &mut connection,
        10,
        "review/revert",
        json!({
            "sessionId": "review-session",
            "target": {"kind": "scope", "owner": {"kind": "agent", "turnId": 2}}
        }),
    );
    assert!(reverted.is_empty());
    assert_eq!(
        fs::read_to_string(working_directory.join("main.txt")).expect("read"),
        "one\ntwo\n",
        "a revert is persisted immediately"
    );

    // What the engine refuses is `invalid_params`, which is the code the
    // reference answers a review failure with.
    let refused = connection.dispatch(&request(
        11,
        "review/approve",
        json!({
            "sessionId": "review-session",
            "target": {
                "kind": "region",
                "path": "main.txt",
                "versionIndex": 99,
                "ordinal": 4
            }
        }),
    ));
    assert!(
        matches!(
            decode_frame(&refused.outbound[0]).expect("a refusal"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ),
        "a region the file does not carry is refused"
    );
    let malformed = connection.dispatch(&request(
        12,
        "review/revert",
        json!({"sessionId": "review-session", "target": {"kind": "nonsense"}}),
    ));
    assert!(matches!(
        decode_frame(&malformed.outbound[0]).expect("a rejection"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidParams,
                ..
            },
            ..
        })
    ));
}

#[test]
fn failed_durable_session_delete_keeps_the_saved_session() {
    let temporary = tempfile::tempdir().expect("delete rollback stores");
    let session_root = temporary.path().join("sessions");
    let working_directory = temporary.path().join("workspace");
    let loop_path = temporary.path().join("loops.json");
    fs::create_dir_all(&working_directory).expect("workspace");
    let store = vibe_core::storage::SessionStore::new(&session_root);
    store
        .create(
            "retained-session",
            &working_directory.to_string_lossy(),
            None,
            1,
        )
        .expect("saved session");
    let release3 =
        Release3Service::for_runtime_session_root(session_root, working_directory.clone());
    let release4 = Release4Service::default()
        .with_loop_store(loop_path.clone())
        .expect("loop store");
    release4
        .dispatch(
            "loops/create",
            &BTreeMap::from([
                ("sessionId".to_owned(), json!("retained-session")),
                ("interval".to_owned(), json!("30s")),
                ("prompt".to_owned(), json!("retain on failure")),
                ("nowSeconds".to_owned(), json!(10)),
            ]),
        )
        .expect("owned loop");
    fs::remove_file(&loop_path).expect("remove loop file");
    fs::create_dir(&loop_path).expect("block loop persistence");
    let server = AppServer::with_release3_service(release3).using_release4_service(release4);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let deleted = connection.dispatch(&request(
        2,
        "session/delete",
        json!({"sessionId": "retained-session"}),
    ));
    assert!(matches!(
        decode_frame(&deleted.outbound[0]).expect("delete failure"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::InternalError,
                ..
            },
            ..
        })
    ));
    assert!(store.load("retained-session").is_ok());
}
