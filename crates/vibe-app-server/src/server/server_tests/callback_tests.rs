//! Callbacks end to end: delivery, refusal, cancellation, validation, and the
//! handoff that rebinds an open one onto a new session identifier.

use super::*;

/// US-173: the wire's `injectInvokedSkill` reaches the deferred driver
/// work on both methods, with the reference defaults: true on `turn/steer`
/// and false on `session/context/inject`.
#[test]
fn the_invoked_skill_flag_reaches_the_deferred_driver_work() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let inject_default = connection.dispatch(&request(
        3,
        "session/context/inject",
        json!({
            "sessionId": "session-1",
            "input": [{"type": "text", "text": "/probe"}],
            "asMessage": true
        }),
    ));
    assert!(matches!(
        inject_default.deferred.as_slice(),
        [DeferredWork::InjectContext {
            inject_invoked_skill: false,
            as_message: true,
            ..
        }]
    ));
    let inject_explicit = connection.dispatch(&request(
        4,
        "session/context/inject",
        json!({
            "sessionId": "session-1",
            "input": [{"type": "text", "text": "/probe"}],
            "asMessage": true,
            "injectInvokedSkill": true
        }),
    ));
    assert!(matches!(
        inject_explicit.deferred.as_slice(),
        [DeferredWork::InjectContext {
            inject_invoked_skill: true,
            ..
        }]
    ));

    let started = connection.dispatch(&request(
        5,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let turn_id = match decode_frame(&started.outbound[0]).expect("turn answer") {
        Envelope::Success(SuccessResponse { result, .. }) => {
            result["turn"]["id"].as_str().expect("turn id").to_owned()
        }
        other => unreachable!("turn/start did not answer: {other:?}"),
    };
    let steer_default = connection.dispatch(&request(
        6,
        "turn/steer",
        json!({
            "sessionId": "session-1",
            "expectedTurnId": turn_id,
            "input": [{"type": "text", "text": "/probe"}]
        }),
    ));
    assert!(
        matches!(
            steer_default.deferred.as_slice(),
            [DeferredWork::SteerTurn {
                inject_invoked_skill: true,
                ..
            }]
        ),
        "the omitted flag applies its declared default: {:?}",
        steer_default.deferred
    );
    let steer_explicit = connection.dispatch(&request(
        7,
        "turn/steer",
        json!({
            "sessionId": "session-1",
            "expectedTurnId": turn_id,
            "input": [{"type": "text", "text": "/probe"}],
            "injectInvokedSkill": false
        }),
    ));
    assert!(matches!(
        steer_explicit.deferred.as_slice(),
        [DeferredWork::SteerTurn {
            inject_invoked_skill: false,
            ..
        }]
    ));
}

#[test]
fn stale_mutations_and_duplicate_callbacks_leave_runtime_unchanged() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let before = server.session("session-1").expect("session view");
    let stale = connection.dispatch(&request(
        4,
        "turn/steer",
        json!({
            "sessionId": "session-1",
            "expectedTurnId": "turn-stale",
            "input": [{"type": "text", "text": "wrong"}]
        }),
    ));
    assert_eq!(
        server.session("session-1").expect("unchanged session"),
        before
    );
    assert!(matches!(
        decode_frame(&stale.outbound[0]).expect("stale response"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::StaleTurn,
                ..
            },
            ..
        })
    ));

    let event_id_before_callback = server
        .lock_sessions()
        .expect("sessions")
        .get("session-1")
        .expect("session")
        .event_watermark;
    let (callback_id, callback_request) = connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "approve?",
        )
        .expect("callback request");
    assert_eq!(
        server
            .lock_sessions()
            .expect("sessions")
            .get("session-1")
            .expect("session")
            .event_watermark,
        event_id_before_callback + 1
    );
    let callback_request = decode_frame(callback_request.last().expect("callback delivery"))
        .expect("callback request frame");
    let callback_request_id = match &callback_request {
        Envelope::Request(request) => request.id.clone(),
        _ => RequestId::String("invalid-callback-request".to_owned()),
    };
    assert!(matches!(
        &callback_request,
        Envelope::Request(ServerRequest {
            method,
            params,
            ..
        }) if method == "callback/call"
            && params["callback"]["callbackId"].as_str() == Some(callback_id.as_str())
            && params["callback"]["detail"]["kind"] == "approval"
    ));
    let acknowledgment = encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id: callback_request_id,
        result: result_map([
            ("callbackId", json!(callback_id)),
            ("accepted", json!(true)),
        ]),
    }));
    let acknowledgment = connection.dispatch(&acknowledgment);
    assert_eq!(acknowledgment, DispatchBatch::empty());
    let first = connection.dispatch(&request(
        5,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": callback_id,
            "output": {
                "type": "approval",
                "decision": {"type": "approve"},
                // The reference approval output declares an operator note,
                // so a client sending one is answered rather than rejected.
                "feedback": "keep the scope tight"
            }
        }),
    ));
    // The answer, then the `session/updated` the resumed status publishes.
    assert_eq!(first.outbound.len(), 2);
    assert_eq!(
        server
            .lock_sessions()
            .expect("sessions")
            .get("session-1")
            .expect("session")
            .event_watermark,
        event_id_before_callback + 2
    );
    let after = server.session("session-1").expect("resolved session");
    assert!(after.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    callback_id,
                    metadata:
                        PublicEntryMetadata {
                            generation_status: PublicEntryGenerationStatus::Completed,
                            ..
                        },
                    state: PublicCallbackState::Answered {
                        output: CallbackOutput::Approval { decision, feedback }
                    },
                    ..
                } if callback_id == "callback-1"
                    && decision.decision == ApprovalDecisionType::Approve
                    && feedback.as_deref() == Some("keep the scope tight")
            )
        })
    }));
    let duplicate = connection.dispatch(&request(
        6,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": "callback-1",
            "output": {
                "type": "approval",
                "decision": {"type": "approve"},
                "feedback": "keep the scope tight"
            }
        }),
    ));
    assert_eq!(
        server.session("session-1").expect("unchanged session"),
        after
    );
    assert!(matches!(
        decode_frame(&duplicate.outbound[0]).expect("duplicate response"),
        Envelope::Success(SuccessResponse { result, .. })
            if result.get("status").and_then(Value::as_str) == Some("duplicate")
    ));

    server
        .complete_turn(
            "session-1",
            "turn-1",
            ProjectionSnapshot {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                handoff_cause: None,
                watermark: 12,
                lifecycle: LifecycleState::Completed,
                title: Some("Retained title".to_owned()),
                history: Vec::new(),
            },
        )
        .expect("terminal projection");
    let completed = server.session("session-1").expect("completed session");
    assert!(completed.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.title.as_deref() == Some("Retained title")
            && snapshot.history.iter().any(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Callback {
                        callback_id,
                        state: PublicCallbackState::Answered { .. },
                        ..
                    } if callback_id == "callback-1"
                )
            })
    }));
}

#[test]
fn rejected_callback_delivery_cancels_the_owned_turn_once() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));

    let (callback_id, callback_request) = connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "approve?",
        )
        .expect("callback request");
    let callback_request = decode_frame(callback_request.last().expect("callback delivery"))
        .expect("callback request frame");
    assert!(matches!(callback_request, Envelope::Request(_)));
    let request_id = match callback_request {
        Envelope::Request(request) => request.id,
        _ => return,
    };
    let rejection = encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id: request_id.clone(),
        result: result_map([
            ("callbackId", json!(callback_id)),
            ("accepted", json!(false)),
        ]),
    }));
    let batch = connection.dispatch(&rejection);
    assert_eq!(
        batch.deferred,
        vec![
            DeferredWork::ResolveCallback {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                callback_id: "callback-1".to_owned(),
                accepted: false,
                value: Some("Client did not accept callback delivery".to_owned()),
            },
            DeferredWork::InterruptTurn {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        ]
    );
    let session = server.session("session-1").expect("cancelled session");
    assert_eq!(session.status, SessionStatus::Cancelled);
    assert_eq!(session.pending_callback, None);
    assert!(session.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    metadata:
                        PublicEntryMetadata {
                            generation_status: PublicEntryGenerationStatus::Completed,
                            ..
                        },
                    state: PublicCallbackState::Cancelled { reason },
                    ..
                } if reason == "Client did not accept callback delivery"
            )
        })
    }));

    let duplicate = connection.dispatch(&rejection);
    assert!(duplicate.close_after_flush);
    assert_eq!(connection.state(), ConnectionState::Closed);
}

#[test]
fn cancelled_user_input_is_answered_without_interrupting_the_turn() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let (callback_id, callback_request) = connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::UserInput,
            "continue?",
        )
        .expect("callback request");
    let request_id = match decode_frame(callback_request.last().expect("callback delivery"))
        .expect("callback frame")
    {
        Envelope::Request(request) => request.id,
        _ => return,
    };
    let acknowledgment = encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id: request_id,
        result: result_map([
            ("callbackId", json!(callback_id)),
            ("accepted", json!(true)),
        ]),
    }));
    assert_eq!(connection.dispatch(&acknowledgment), DispatchBatch::empty());

    let response = connection.dispatch(&request(
        4,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": "callback-1",
            "output": {
                "type": "user_input",
                "result": {"answers": [], "cancelled": true}
            }
        }),
    ));
    assert_eq!(
        response.deferred,
        vec![DeferredWork::ResolveCallback {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            callback_id: "callback-1".to_owned(),
            accepted: true,
            value: Some(
                json!({
                    "type": "user_input",
                    "result": {"answers": [], "cancelled": true}
                })
                .to_string()
            ),
        }]
    );
    let session = server.session("session-1").expect("session");
    assert_eq!(session.status, SessionStatus::Running);
    assert!(session.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    metadata:
                        PublicEntryMetadata {
                            generation_status: PublicEntryGenerationStatus::Completed,
                            ..
                        },
                    state: PublicCallbackState::Answered {
                        output: CallbackOutput::UserInput { result }
                    },
                    ..
                } if result.cancelled
            )
        })
    }));
}

#[test]
fn approval_denial_is_answered_and_only_cancel_turn_interrupts() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));

    connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "approve?",
        )
        .expect("denial callback");
    let denied = connection.dispatch(&request(
        4,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": "callback-1",
            "output": {
                "type": "approval",
                "decision": {"type": "deny"}
            }
        }),
    ));
    assert_eq!(
        denied.deferred,
        vec![DeferredWork::ResolveCallback {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            callback_id: "callback-1".to_owned(),
            accepted: true,
            value: Some(
                json!({
                    "type": "approval",
                    "decision": {"type": "deny"}
                })
                .to_string()
            ),
        }]
    );
    let denied_session = server.session("session-1").expect("denied session");
    assert_eq!(denied_session.status, SessionStatus::Running);
    assert!(denied_session.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    callback_id,
                    state: PublicCallbackState::Answered {
                        output: CallbackOutput::Approval { decision, .. }
                    },
                    ..
                } if callback_id == "callback-1"
                    && decision.decision == ApprovalDecisionType::Deny
            )
        })
    }));

    connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "cancel?",
        )
        .expect("cancel callback");
    let cancelled = connection.dispatch(&request(
        5,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": "callback-2",
            "output": {
                "type": "approval",
                "decision": {"type": "cancel_turn"}
            }
        }),
    ));
    assert_eq!(
        cancelled.deferred,
        vec![
            DeferredWork::ResolveCallback {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                callback_id: "callback-2".to_owned(),
                accepted: true,
                value: Some(
                    json!({
                        "type": "approval",
                        "decision": {"type": "cancel_turn"}
                    })
                    .to_string()
                ),
            },
            DeferredWork::InterruptTurn {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        ]
    );
    assert!(
        server
            .session("session-1")
            .expect("session")
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.history.iter().any(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Callback {
                        callback_id,
                        state: PublicCallbackState::Answered { .. },
                        ..
                    } if callback_id == "callback-2"
                )
            }))
    );
}

#[test]
fn answered_delivery_ignores_a_late_negative_acknowledgment() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let (_, first_delivery) = connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "first?",
        )
        .expect("first callback");
    let first_request_id = match decode_frame(first_delivery.last().expect("callback frame"))
        .expect("callback delivery")
    {
        Envelope::Request(request) => request.id,
        _ => return,
    };
    connection.dispatch(&request(
        4,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": "callback-1",
            "output": {
                "type": "approval",
                "decision": {"type": "approve"}
            }
        }),
    ));
    connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "second?",
        )
        .expect("second callback");

    let late_rejection = encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id: first_request_id,
        result: result_map([
            ("callbackId", json!("callback-1")),
            ("accepted", json!(false)),
        ]),
    }));
    assert_eq!(connection.dispatch(&late_rejection), DispatchBatch::empty());
    assert_eq!(connection.state(), ConnectionState::Ready);
    assert_eq!(
        server
            .session("session-1")
            .expect("session")
            .pending_callback,
        Some("callback-2".to_owned())
    );
}

/// The reference declares no plan-review field on a callback detail, so the
/// port's marker becomes the notice entry the reference publishes and the
/// detail that reaches the client carries only the declared keys.
#[test]
fn a_plan_review_becomes_a_notice_and_leaves_the_callback_detail_conformant() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let review = |file_path: Value| {
        json!({
            "kind": "user_input",
            "request": {
                "questions": [{
                    "question": "Switch to code mode?",
                    "options": [{"label": "Yes"}, {"label": "No"}]
                }],
                "footerNote": null,
            },
            "planReview": true,
            "filePath": file_path,
            "relatedEntryId": null,
        })
    };

    assert!(matches!(
        connection.request_callback_with_detail(
            "session-1",
            "turn-1",
            EngineCallbackKind::UserInput,
            "Review plan",
            review(Value::Null),
        ),
        Err(ServerError::InvalidCallbackDetail(_))
    ));

    let (callback_id, _) = connection
        .request_callback_with_detail(
            "session-1",
            "turn-1",
            EngineCallbackKind::UserInput,
            "Review plan",
            review(json!("/workspace/plan.md")),
        )
        .expect("the plan review is accepted");

    let sessions = server.lock_sessions().expect("sessions");
    let history = &sessions
        .get("session-1")
        .expect("session")
        .snapshot
        .as_ref()
        .expect("snapshot")
        .history;
    let notice = history
        .iter()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Notice {
                metadata, detail, ..
            } => Some((metadata, detail)),
            _ => None,
        })
        .expect("the plan review is published as a notice");
    assert_eq!(
        notice.1,
        &NoticeDetail::PlanReviewStarted {
            file_path: "/workspace/plan.md".to_owned()
        }
    );
    assert_eq!(
        notice.0.related_entry_id.as_deref(),
        Some(format!("callback:{callback_id}").as_str())
    );

    let detail = history
        .iter()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Callback { detail, .. } => Some(detail),
            _ => None,
        })
        .expect("the callback is published");
    let wire = serde_json::to_value(detail).expect("the detail serializes");
    let keys = wire
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(keys, ["kind", "relatedEntryId", "request"]);
}

#[test]
fn callback_requests_and_answers_are_validated_before_mutation() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let before = server.session("session-1").expect("session");
    for detail in [
        json!({
            "kind": "user_input",
            "request": {"questions": [], "footerNote": null},
            "relatedEntryId": null,
        }),
        json!({
            "kind": "user_input",
            "request": {
                "questions": [{
                    "question": "continue?",
                    "options": [
                        {"label": "Yes"},
                        {"label": "No"}
                    ]
                }],
                "footerNote": "x".repeat(MAX_CALLBACK_REQUEST_BYTES),
            },
            "relatedEntryId": null,
        }),
    ] {
        assert!(matches!(
            connection.request_callback_with_detail(
                "session-1",
                "turn-1",
                EngineCallbackKind::UserInput,
                "continue?",
                detail,
            ),
            Err(ServerError::InvalidCallbackDetail(_))
        ));
        assert_eq!(
            server.session("session-1").expect("unchanged session"),
            before
        );
    }

    connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::UserInput,
            "continue?",
        )
        .expect("valid callback");
    for (id, output) in [
        (
            4,
            json!({
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "wrong question",
                        "answer": "Yes",
                        "isOther": false
                    }],
                    "cancelled": false
                }
            }),
        ),
        (
            5,
            json!({
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "continue?",
                        "answer": "Maybe",
                        "isOther": false
                    }],
                    "cancelled": false
                }
            }),
        ),
    ] {
        let before_answer = server.session("session-1").expect("pending session");
        let rejected = connection.dispatch(&request(
            id,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": "callback-1",
                "output": output,
            }),
        ));
        assert!(matches!(
            decode_frame(&rejected.outbound[0]).expect("invalid response"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            server.session("session-1").expect("unchanged answer"),
            before_answer
        );
    }
}

#[test]
fn final_handoff_preserves_prior_turn_history_and_rebinds_callbacks() {
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
        .complete_turn(
            "session-1",
            "turn-1",
            ProjectionSnapshot {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                handoff_cause: None,
                watermark: 1,
                lifecycle: LifecycleState::Completed,
                title: Some("Cumulative".to_owned()),
                history: vec![message_entry(
                    "turn-1:entry-1",
                    "session-1",
                    "turn-1",
                    1,
                    "first",
                )],
            },
        )
        .expect("first turn");
    connection.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "second"}]}),
    ));
    connection
        .request_callback(
            "session-1",
            "turn-2",
            EngineCallbackKind::Approval,
            "approve?",
        )
        .expect("second-turn callback");
    connection.dispatch(&request(
        5,
        "callback/respond",
        json!({
            "sessionId": "session-1",
            "callbackId": "callback-1",
            "output": {
                "type": "approval",
                "decision": {"type": "approve"}
            }
        }),
    ));
    server
        .complete_turn(
            "session-1",
            "turn-2",
            ProjectionSnapshot {
                session_id: "session-2".to_owned(),
                turn_id: Some("turn-2".to_owned()),
                handoff_cause: None,
                watermark: 2,
                lifecycle: LifecycleState::Completed,
                title: None,
                history: vec![message_entry(
                    "turn-2:entry-1",
                    "session-2",
                    "turn-2",
                    2,
                    "second",
                )],
            },
        )
        .expect("second turn handoff");
    let session = server.session("session-1").expect("old alias resolves");
    assert_eq!(session.id, "session-2");
    let snapshot = session.snapshot.expect("cumulative snapshot");
    assert_eq!(snapshot.title.as_deref(), Some("Cumulative"));
    assert_eq!(snapshot.history.len(), 3);
    assert!(
        snapshot
            .history
            .iter()
            .all(|entry| { entry.metadata().session_id == "session-2" })
    );
    assert!(["turn-1:entry-1", "turn-2:entry-1"].into_iter().all(|id| {
        snapshot
            .history
            .iter()
            .any(|entry| entry.metadata().id == id)
    }));
    assert!(snapshot.history.iter().any(|entry| {
        matches!(
            entry,
            PublicHistoryEntry::Callback {
                state: PublicCallbackState::Answered { .. },
                ..
            }
        )
    }));
}

#[test]
fn malformed_callback_outputs_fail_closed_without_settling_state() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let (_, callback_request) = connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "approve?",
        )
        .expect("callback request");
    let request_id = match decode_frame(callback_request.last().expect("callback delivery"))
        .expect("callback frame")
    {
        Envelope::Request(request) => request.id,
        _ => return,
    };
    let acknowledgment = encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id: request_id,
        result: result_map([
            ("callbackId", json!("callback-1")),
            ("accepted", json!(true)),
        ]),
    }));
    assert_eq!(connection.dispatch(&acknowledgment), DispatchBatch::empty());

    for (id, output) in [
        (
            4,
            json!({"type": "approval", "decision": {"type": "future_choice"}}),
        ),
        (
            5,
            json!({
                "type": "user_input",
                "result": {"answers": [], "cancelled": "false"}
            }),
        ),
        (
            6,
            json!({
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "q",
                        "answer": "a",
                        "isOther": false,
                        "unexpected": true
                    }],
                    "cancelled": false
                }
            }),
        ),
    ] {
        let before = server.session("session-1").expect("pending session");
        let batch = connection.dispatch(&request(
            id,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": "callback-1",
                "output": output,
            }),
        ));
        assert!(matches!(
            decode_frame(&batch.outbound[0]).expect("invalid-params response"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            server.session("session-1").expect("unchanged session"),
            before
        );
    }
}

#[test]
fn attachment_counts_are_serialized_and_cleaned_on_close() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection
        .attach_session("session-1")
        .expect("attachment succeeds");
    connection
        .attach_session("session-1")
        .expect("duplicate attachment is idempotent");
    assert_eq!(
        server
            .session("session-1")
            .expect("attached view")
            .attachments,
        1
    );
    connection
        .detach_session("session-1")
        .expect("explicit detach succeeds");
    assert_eq!(
        server
            .session("session-1")
            .expect("explicitly detached view")
            .attachments,
        0
    );
    connection.close();
    assert_eq!(
        server
            .session("session-1")
            .expect("detached view")
            .attachments,
        0
    );
    let mut reattached = server.connect(TransportKind::InProcess);
    initialize(&mut reattached);
    reattached
        .attach_session("session-1")
        .expect("reattachment succeeds");
    let sessions = server.lock_sessions().expect("session state");
    let session = sessions.get("session-1").expect("reattached session");
    assert_eq!(session.attachments, 1);
    assert_eq!(session.resource_generation, 2);
}

#[test]
fn another_connection_cannot_mutate_an_unattached_session() {
    let server = AppServer::default();
    let mut owner = server.connect(TransportKind::InProcess);
    initialize(&mut owner);
    start_session(&mut owner);

    let mut intruder = server.connect(TransportKind::InProcess);
    initialize(&mut intruder);
    let read = intruder.dispatch(&request(
        3,
        "session/read",
        json!({"sessionId": "session-1"}),
    ));
    assert!(matches!(
        decode_frame(&read.outbound[0]).expect("forbidden response"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::Forbidden,
                ..
            },
            ..
        })
    ));
}
