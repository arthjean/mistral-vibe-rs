//! A turn from reservation to settlement, and what each terminal outcome
//! publishes.

use super::*;

/// US-102: the families are published against the resolver, so a
/// configuration change between two turns reaches the handlers without the
/// session's surface being registered again.
#[tokio::test]
async fn a_configuration_change_between_turns_reaches_the_published_tools() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("visible.txt"), "safe\n").expect("fixture");
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "session-1", "workingDirectory": workspace.path()}),
    ));
    connection.dispatch(&request(
        3,
        "workspace/trust/decision",
        json!({
            "sessionId": "session-1",
            "cwd": workspace.path(),
            "decision": "trust_cwd"
        }),
    ));
    let invocation = || ToolInvocation {
        call_id: "read-1".to_owned(),
        arguments: json!({"file_path": "visible.txt"}),
    };
    assert_eq!(
        server
            .invoke_tool("session-1", "read_file", invocation())
            .await
            .expect("the declared budget carries this file")
            .typed_result["content"],
        "        1\u{2192}safe"
    );

    // No re-registration: the same published surface, a moved budget.
    let config = server.release3.tool_config();
    config.update(
        "[read_file]\nmax_read_bytes = 2\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let refused = server
        .invoke_tool("session-1", "read_file", invocation())
        .await
        .expect_err("the lowered budget refuses the same file");
    assert!(refused.to_string().contains("2-byte budget"), "{refused}");
}

#[tokio::test]
async fn workspace_trust_controls_the_session_tool_registry() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("visible.txt"), "safe\n").expect("fixture");
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    let started = connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "workingDirectory": workspace.path()
        }),
    ));
    // The answer, then the snapshot the attachment publishes.
    assert_eq!(started.outbound.len(), 2);

    let invocation = || ToolInvocation {
        call_id: "read-1".to_owned(),
        arguments: json!({"file_path": "visible.txt"}),
    };
    assert!(
        server
            .invoke_tool("session-1", "read_file", invocation())
            .await
            .is_err()
    );

    let trusted = connection.dispatch(&request(
        3,
        "workspace/trust/decision",
        json!({
            "sessionId": "session-1",
            "cwd": workspace.path(),
            "decision": "trust_cwd"
        }),
    ));
    assert_eq!(trusted.outbound.len(), 2);
    assert_eq!(
        server
            .invoke_tool("session-1", "read_file", invocation())
            .await
            .expect("trusted read")
            .typed_result["content"],
        "        1\u{2192}safe"
    );

    connection.dispatch(&request(
        4,
        "workspace/trust/decision",
        json!({
            "sessionId": "session-1",
            "cwd": workspace.path(),
            "decision": "decline"
        }),
    ));
    assert!(
        server
            .invoke_tool("session-1", "read_file", invocation())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn auto_approve_update_rebinds_existing_workspace_tool_handlers() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file = workspace.path().join("editable.txt");
    std::fs::write(&file, "before\n").expect("fixture");
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    let started = connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "workingDirectory": workspace.path()
        }),
    ));
    // The answer, then the snapshot the attachment publishes.
    assert_eq!(started.outbound.len(), 2);
    let invocation = |call_id: &str| ToolInvocation {
        call_id: call_id.to_owned(),
        arguments: json!({
            "file_path": "editable.txt",
            "old_string": "before",
            "new_string": "after",
            "replace_all": false
        }),
    };

    assert!(
        server
            .invoke_tool("session-1", "edit", invocation("edit-denied"))
            .await
            .is_err()
    );
    let updated = connection.dispatch(&request(
        3,
        "session/overrides/write",
        json!({"sessionId": "session-1", "autoApprove": true}),
    ));
    assert!(matches!(
        decode_frame(&updated.outbound[0]).expect("settings response"),
        Envelope::Success(_)
    ));
    let rebound = server
        .invoke_tool("session-1", "edit", invocation("edit-approved"))
        .await
        .expect_err("edit still requires an active review turn");
    assert!(
        rebound
            .to_string()
            .contains("mutation requires an active turn"),
        "updated approval must reach the rebound edit handler: {rebound}"
    );
}

#[test]
fn resource_requests_validate_session_ownership_and_idle_review_mutations() {
    let server = AppServer::default();
    let mut owner = server.connect(TransportKind::InProcess);
    initialize(&mut owner);
    start_session(&mut owner);

    let malformed = owner.dispatch(&request(3, "tools/list", json!({"sessionId": 7})));
    assert!(matches!(
        decode_frame(&malformed.outbound[0]).expect("invalid params"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidParams,
                ..
            },
            ..
        })
    ));

    owner.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "busy"}]}),
    ));
    let review = owner.dispatch(&request(
        5,
        "review/revert",
        json!({"sessionId": "session-1", "target": {"kind": "all"}}),
    ));
    assert!(matches!(
        decode_frame(&review.outbound[0]).expect("review conflict"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Conflict,
                ..
            },
            ..
        })
    ));

    let mut intruder = server.connect(TransportKind::InProcess);
    initialize(&mut intruder);
    let forbidden = intruder.dispatch(&request(
        6,
        "runtime/read",
        json!({"sessionId": "session-1"}),
    ));
    assert!(matches!(
        decode_frame(&forbidden.outbound[0]).expect("forbidden resource"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Forbidden,
                ..
            },
            ..
        })
    ));
}

#[test]
fn closing_an_active_session_retains_ownership_until_terminal_cleanup() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let close = connection.dispatch(&request(
        4,
        "session/close",
        json!({"sessionId": "session-1"}),
    ));
    assert_eq!(
        close.deferred,
        vec![
            DeferredWork::InterruptTurn {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
            DeferredWork::CloseResources {
                session_id: "session-1".to_owned(),
                generation: 1,
            },
        ]
    );

    let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
    for envelope in [
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: None,
            emitted_at: 1,
            event_id: 1,
            event: vibe_core::events::EngineEvent::UserMessage {
                content: "hello".to_owned(),
            },
        },
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: None,
            emitted_at: 2,
            event_id: 2,
            event: vibe_core::events::EngineEvent::Lifecycle {
                state: LifecycleState::Cancelled,
                message: None,
            },
        },
    ] {
        reducer.apply(&envelope).expect("terminal event applies");
    }
    server
        .complete_turn("session-1", "turn-1", reducer.state().clone())
        .expect("closed turn finalizes");
    let session = server
        .session("session-1")
        .expect("session remains addressable");
    assert_eq!(session.status, SessionStatus::Closed);
    assert!(session.active_turn.is_none());
}

#[test]
fn driver_failure_releases_the_turn_reservation() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "first"}]}),
    ));
    server
        .fail_turn(
            "session-1",
            "turn-1",
            "provider failed",
            TurnErrorCode::BackendError,
        )
        .expect("failure finalizes");
    let retry = connection.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "retry"}]}),
    ));
    assert_eq!(retry.deferred.len(), 1);
    assert_eq!(
        server
            .session("session-1")
            .expect("retry session")
            .active_turn
            .as_deref(),
        Some("turn-2")
    );
}

#[test]
fn conversation_limits_complete_with_the_public_limit_reason() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "bounded"}]}),
    ));
    let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
    for envelope in [
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: vibe_core::events::EngineEvent::UserMessage {
                content: "bounded".to_owned(),
            },
        },
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 2,
            event_id: 2,
            event: vibe_core::events::EngineEvent::Lifecycle {
                state: LifecycleState::Completed,
                message: Some("Token limit reached".to_owned()),
            },
        },
    ] {
        reducer.apply(&envelope).expect("limit event applies");
    }
    let notification = server
        .complete_turn_with_stop_reason(
            "session-1",
            "turn-1",
            reducer.state().clone(),
            Some(vibe_core::events::PublicTurnStopReason::Limit),
        )
        .expect("limit turn completes");
    assert!(matches!(
        decode_frame(notification.last().expect("completion frame")).expect("completion notification"),
        Envelope::Notification(Notification {
            method,
            params,
            ..
        }) if method == "turn/completed"
            && params["turn"]["status"] == "completed"
            && params["turn"]["stopReason"] == "limit"
    ));
}

#[test]
fn provider_terminal_failures_preserve_their_public_error() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "bounded"}]}),
    ));
    let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
    for envelope in [
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: vibe_core::events::EngineEvent::UserMessage {
                content: "bounded".to_owned(),
            },
        },
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 2,
            event_id: 2,
            event: vibe_core::events::EngineEvent::Lifecycle {
                state: LifecycleState::Failed,
                message: Some("response too long".to_owned()),
            },
        },
    ] {
        reducer.apply(&envelope).expect("failure event applies");
    }
    let notification = server
        .complete_turn_with_details(
            "session-1",
            "turn-1",
            reducer.state().clone(),
            None,
            Some(PublicError {
                message: "The model's response exceeded the maximum output token limit.".to_owned(),
                code: Some("response_too_long".to_owned()),
                details: Value::Null,
            }),
        )
        .expect("failed turn completes");
    assert!(matches!(
        decode_frame(notification.last().expect("completion frame")).expect("completion notification"),
        Envelope::Notification(Notification {
            method,
            params,
            ..
        }) if method == "turn/completed"
            && params["turn"]["status"] == "failed"
            && params["turn"]["error"]["code"] == "response_too_long"
    ));
}

/// A handoff onto the session's own name is refused rather than published:
/// a reference projection validates a handoff by the two identifiers
/// differing, so emitting one would break the client it was meant to serve.
#[test]
fn a_handoff_onto_the_same_session_is_refused() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "clear"}]}),
    ));
    let mut reducer = vibe_core::events::ProjectionReducer::for_turn("session-1", "turn-1");
    reducer
        .apply(&vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: vibe_core::events::EngineEvent::UserMessage {
                content: "clear".to_owned(),
            },
        })
        .expect("the prompt projects");
    let snapshot = reducer.state().clone();
    for notice in [
        HandoffNotice::Compacted { summary_length: 4 },
        HandoffNotice::ContextCleared {
            plan_file_path: None,
        },
    ] {
        let error = server
            .handoff_active_turn(
                "session-1",
                "session-1",
                "turn-1",
                snapshot.clone(),
                &notice,
                1,
            )
            .expect_err("a handoff onto the same identifier is refused");
        assert!(matches!(error, ServerError::SessionConflict(id) if id == "session-1"));
    }
}

/// Every failing turn names a reason from the reference vocabulary, whether
/// the failure arrived with the projection or short-circuited it, so a
/// client can branch on the code rather than parse the message.
#[test]
fn a_failed_turn_publishes_a_reference_error_code() {
    let vocabulary = [
        "rate_limit",
        "context_too_long",
        "response_too_long",
        "refusal",
        "invalid_image_attachment",
        "images_not_supported",
        "compaction_failed",
        "backend_error",
        "internal_error",
    ];
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "fail"}]}),
    ));
    let notification = server
        .fail_turn(
            "session-1",
            "turn-1",
            "the provider rejected the request",
            TurnErrorCode::RateLimit,
        )
        .expect("failed turn settles");
    let Envelope::Notification(Notification { method, params, .. }) =
        decode_frame(notification.last().expect("completion frame"))
            .expect("completion notification")
    else {
        return;
    };
    assert_eq!(method, "turn/completed");
    assert_eq!(params["turn"]["error"]["code"], "rate_limit");
    assert!(
        vocabulary.contains(
            &params["turn"]["error"]["code"]
                .as_str()
                .expect("a failing turn names its code")
        ),
        "the code must come from the reference vocabulary: {params:?}"
    );
}

#[test]
fn handoff_atomically_migrates_the_runtime_to_the_projected_id() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "compact"}]}),
    ));
    let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
    for envelope in [
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: None,
            emitted_at: 1,
            event_id: 1,
            event: vibe_core::events::EngineEvent::UserMessage {
                content: "compact".to_owned(),
            },
        },
        vibe_core::events::EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: None,
            emitted_at: 2,
            event_id: 2,
            event: vibe_core::events::EngineEvent::SessionHandoff {
                from_session_id: "session-1".to_owned(),
                to_session_id: "session-2".to_owned(),
                cause: vibe_core::events::SessionHandoffCause::Compaction,
            },
        },
        vibe_core::events::EventEnvelope {
            session_id: "session-2".to_owned(),
            turn_id: None,
            emitted_at: 3,
            event_id: 3,
            event: vibe_core::events::EngineEvent::Lifecycle {
                state: LifecycleState::Completed,
                message: None,
            },
        },
    ] {
        reducer.apply(&envelope).expect("handoff event applies");
    }
    server
        .complete_turn("session-1", "turn-1", reducer.state().clone())
        .expect("handoff completes");
    assert_eq!(
        server
            .session("session-1")
            .expect("source alias resolves to target runtime")
            .id,
        "session-2"
    );
    assert_eq!(
        server.session("session-2").expect("target runtime").id,
        "session-2"
    );
}
