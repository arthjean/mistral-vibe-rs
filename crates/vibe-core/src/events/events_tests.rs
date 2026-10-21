//! What the reducer projects from each event, and what it refuses.

use serde_json::json;

use super::*;

fn event(id: u64, event: EngineEvent) -> EventEnvelope {
    EventEnvelope {
        session_id: "session-1".to_owned(),
        turn_id: None,
        emitted_at: id,
        working_directory: None,
        event_id: id,
        event,
    }
}

/// A projection that refuses an event is byte-identical to the one before
/// it, for every refusal [`reduce_event`] can raise.
///
/// [`ProjectionReducer::apply`] reduces in place, so this is the invariant
/// that keeps a rejected event from leaving a half-applied entry behind. An
/// arm that grows a check after a mutation fails here.
#[test]
fn a_rejected_event_leaves_the_projection_untouched() {
    let mut reducer = ProjectionReducer::new("session-1");
    // A refused event never advances the watermark, so the next one reuses
    // its identifier: the sequence has no gap for the reducer to reject
    // before it reaches the arm under test.
    let mut next_id = 1_u64;
    let mut apply = |reducer: &mut ProjectionReducer, projected: EngineEvent| {
        let outcome = reducer.apply(&event(next_id, projected));
        if outcome.is_ok() {
            next_id += 1;
        }
        outcome
    };

    // Every refusal an idle projection can raise, before anything is
    // projected at all: the empty state must survive them.
    let idle = reducer.state().clone();
    for refused in [
        EngineEvent::UserSteer {
            content: "steering a turn that never started".to_owned(),
            message_id: None,
        },
        EngineEvent::ModelText {
            message_id: None,
            text: "text with no turn".to_owned(),
        },
        EngineEvent::Title {
            title: "a title with no turn".to_owned(),
        },
    ] {
        let error = apply(&mut reducer, refused).expect_err("an idle turn refuses this");
        assert!(
            matches!(error, ProjectionError::IllegalTransition { .. }),
            "{error:?}"
        );
        assert_eq!(reducer.state(), &idle, "a refusal projected something");
    }

    apply(
        &mut reducer,
        EngineEvent::UserMessage {
            attachments: Vec::new(),
            message_id: None,
            content: "start".to_owned(),
            user_display_content: None,
        },
    )
    .expect("the turn starts");
    apply(
        &mut reducer,
        EngineEvent::ToolCall {
            call_id: "call-1".to_owned(),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
            remote: None,
        },
    )
    .expect("the tool call projects");
    let running = reducer.state().clone();

    // A stream and a result naming a call the projection never saw, and a
    // handoff leaving a session this projection is not on.
    let stream = apply(
        &mut reducer,
        EngineEvent::ToolStream {
            call_id: "call-unknown".to_owned(),
            chunk: "orphan chunk".to_owned(),
        },
    )
    .expect_err("a stream without its call is refused");
    assert!(
        matches!(stream, ProjectionError::IllegalTransition { .. }),
        "{stream:?}"
    );
    assert_eq!(reducer.state(), &running);

    let result = apply(
        &mut reducer,
        EngineEvent::ToolResult {
            call_id: "call-unknown".to_owned(),
            content: "orphan result".to_owned(),
            typed_result: Value::Null,
            projected_result: Value::Null,
            display: Value::Null,
            duration_ms: 1,
            is_error: false,
            cancelled: false,
            skipped: false,
            approval: crate::events::EffectApproval::default(),
        },
    )
    .expect_err("a result without its call is refused");
    assert!(
        matches!(result, ProjectionError::IllegalTransition { .. }),
        "{result:?}"
    );
    assert_eq!(reducer.state(), &running);

    let handoff = apply(
        &mut reducer,
        EngineEvent::SessionHandoff {
            from_session_id: "session-elsewhere".to_owned(),
            to_session_id: "session-2".to_owned(),
            cause: SessionHandoffCause::Compaction,
        },
    )
    .expect_err("a handoff from another session is refused");
    assert!(
        matches!(handoff, ProjectionError::InvalidHandoff { .. }),
        "{handoff:?}"
    );
    assert_eq!(
        reducer.state(),
        &running,
        "a refused handoff rebound the session"
    );

    // A callback answered under an identifier no open callback carries.
    apply(
        &mut reducer,
        EngineEvent::CallbackRequested {
            callback_id: "callback-1".to_owned(),
            kind: CallbackKind::Approval,
            prompt: "may I".to_owned(),
        },
    )
    .expect("the callback opens");
    let waiting = reducer.state().clone();
    let pending = apply(
        &mut reducer,
        EngineEvent::CallbackResolved {
            callback_id: "callback-unknown".to_owned(),
            accepted: true,
            value: None,
        },
    )
    .expect_err("an unknown callback is refused");
    assert!(
        matches!(pending, ProjectionError::CallbackNotPending(_)),
        "{pending:?}"
    );
    assert_eq!(reducer.state(), &waiting);

    // The turn still runs to completion afterward, so no refusal left the
    // projection in a state the next event cannot build on.
    apply(
        &mut reducer,
        EngineEvent::CallbackResolved {
            callback_id: "callback-1".to_owned(),
            accepted: true,
            value: None,
        },
    )
    .expect("the open callback resolves");
    apply(
        &mut reducer,
        EngineEvent::Lifecycle {
            state: LifecycleState::Completed,
            message: None,
        },
    )
    .expect("the turn completes");
    assert_eq!(reducer.state().lifecycle, LifecycleState::Completed);
}

#[test]
fn reducer_is_monotonic_and_transactional() {
    let mut reducer = ProjectionReducer::new("session-1");
    assert_eq!(
        reducer
            .apply(&event(
                1,
                EngineEvent::UserMessage {
                    attachments: Vec::new(),
                    message_id: None,
                    content: "hello".to_owned(),
                    user_display_content: None,
                },
            ))
            .expect("first event applies"),
        ApplyOutcome::Applied
    );
    let before = reducer.state().clone();
    assert_eq!(
        reducer
            .apply(&event(
                1,
                EngineEvent::ModelText {
                    message_id: None,
                    text: "duplicate".to_owned(),
                },
            ))
            .expect("duplicates are suppressed"),
        ApplyOutcome::Duplicate
    );
    assert_eq!(reducer.state(), &before);

    let foreign_duplicate = EventEnvelope {
        session_id: "session-2".to_owned(),
        turn_id: None,
        emitted_at: 1,
        working_directory: None,
        event_id: 1,
        event: EngineEvent::ModelText {
            message_id: None,
            text: "foreign duplicate".to_owned(),
        },
    };
    assert!(matches!(
        reducer.apply(&foreign_duplicate),
        Err(ProjectionError::ForeignSession { .. })
    ));
    assert_eq!(reducer.state(), &before);

    assert_eq!(
        reducer.apply(&event(
            3,
            EngineEvent::ModelText {
                message_id: None,
                text: "gap".to_owned(),
            }
        )),
        Err(ProjectionError::Gap {
            expected: 2,
            actual: 3
        })
    );
    assert_eq!(reducer.state(), &before);
}

#[test]
fn invalid_events_leave_public_state_unchanged() {
    let mut reducer = ProjectionReducer::new("session-1");
    let before = reducer.state().clone();
    let foreign = EventEnvelope {
        session_id: "session-2".to_owned(),
        turn_id: None,
        emitted_at: 1,
        working_directory: None,
        event_id: 1,
        event: EngineEvent::ModelText {
            message_id: None,
            text: "wrong".to_owned(),
        },
    };
    assert!(matches!(
        reducer.apply(&foreign),
        Err(ProjectionError::ForeignSession { .. })
    ));
    assert_eq!(reducer.state(), &before);

    assert!(matches!(
        reducer.apply(&event(
            1,
            EngineEvent::ModelText {
                message_id: None,
                text: "illegal".to_owned(),
            }
        )),
        Err(ProjectionError::IllegalTransition { .. })
    ));
    assert_eq!(reducer.state(), &before);
}

#[test]
fn callback_and_handoff_preserve_public_contract() {
    let mut reducer = ProjectionReducer::new("session-1");
    for envelope in [
        event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "ship it".to_owned(),
                user_display_content: None,
            },
        ),
        event(
            2,
            EngineEvent::CallbackRequested {
                callback_id: "callback-1".to_owned(),
                kind: CallbackKind::Approval,
                prompt: "approve?".to_owned(),
            },
        ),
        event(
            3,
            EngineEvent::CallbackResolved {
                callback_id: "callback-1".to_owned(),
                accepted: true,
                value: Some("yes".to_owned()),
            },
        ),
        event(
            4,
            EngineEvent::SessionHandoff {
                from_session_id: "session-1".to_owned(),
                to_session_id: "session-2".to_owned(),
                cause: SessionHandoffCause::Compaction,
            },
        ),
    ] {
        reducer.apply(&envelope).expect("valid lifecycle event");
    }
    assert_eq!(reducer.state().session_id, "session-2");
    assert_eq!(reducer.state().watermark, 4);
    assert!(
        reducer
            .state()
            .history
            .iter()
            .all(|entry| entry.metadata().session_id == "session-2")
    );
    assert!(matches!(
        reducer.state().history.get(1),
        Some(PublicHistoryEntry::Callback {
            state: PublicCallbackState::Answered {
                output: CallbackOutput::Approval { feedback, .. }
            },
            ..
        }) if feedback.as_deref() == Some("yes")
    ));
}

/// US-151: the boundary pair is one entry, created in progress and patched
/// where the reference patches it.
#[test]
fn the_compaction_pair_projects_as_one_entry_that_is_patched() {
    let mut reducer = ProjectionReducer::new("session-1");
    for envelope in [
        event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "compact".to_owned(),
                user_display_content: None,
            },
        ),
        event(
            2,
            EngineEvent::CompactionStarted {
                compaction_id: "compaction-1".to_owned(),
                current_context_tokens: 150_000,
                threshold: 120_000,
            },
        ),
    ] {
        reducer.apply(&envelope).expect("valid lifecycle event");
    }
    let entries = reducer.state().history.len();
    assert!(matches!(
        reducer.state().history.last(),
        Some(PublicHistoryEntry::Checkpoint { metadata, kind, message, .. })
            if kind == "compaction"
                && metadata.generation_status == PublicEntryGenerationStatus::InProgress
                && message.as_deref() == Some("Compacting context")
    ));

    reducer
        .apply(&event(
            3,
            EngineEvent::CompactionCompleted {
                compaction_id: "compaction-1".to_owned(),
                summary_length: 15,
                old_session_id: "session-1".to_owned(),
                new_session_id: "session-2".to_owned(),
            },
        ))
        .expect("the end event applies");
    assert_eq!(
        reducer.state().history.len(),
        entries,
        "the end event patches the entry the start created rather than adding one"
    );
    assert!(matches!(
        reducer.state().history.last(),
        Some(PublicHistoryEntry::Checkpoint { metadata, message, .. })
            if metadata.generation_status == PublicEntryGenerationStatus::Completed
                && message.as_deref() == Some("Context compacted")
    ));

    // The outcome event carries no history, like the stats event.
    reducer
        .apply(&event(
            4,
            EngineEvent::CompactionOutcome {
                compaction_id: "compaction-1".to_owned(),
                status: CompactionStatus::Success,
                context_tokens_before: 150_000,
                threshold: 120_000,
                reason: None,
            },
        ))
        .expect("the outcome applies");
    assert_eq!(reducer.state().history.len(), entries);
}

/// US-151: an end event whose start was never seen still leaves a coherent
/// entry, which is what a late subscriber reads.
#[test]
fn an_end_event_without_its_start_still_projects_an_entry() {
    let mut reducer = ProjectionReducer::new("session-1");
    for envelope in [
        event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "compact".to_owned(),
                user_display_content: None,
            },
        ),
        event(
            2,
            EngineEvent::CompactionCompleted {
                compaction_id: "compaction-1".to_owned(),
                summary_length: 15,
                old_session_id: "session-1".to_owned(),
                new_session_id: "session-2".to_owned(),
            },
        ),
    ] {
        reducer.apply(&envelope).expect("valid lifecycle event");
    }
    assert!(matches!(
        reducer.state().history.last(),
        Some(PublicHistoryEntry::Checkpoint { metadata, kind, message, .. })
            if kind == "compaction"
                && metadata.generation_status == PublicEntryGenerationStatus::Completed
                && message.as_deref() == Some("Context compacted")
    ));
}

/// US-151: the variant this port emitted before the pair still projects the
/// entry a stored transcript was written with.
#[test]
fn a_stored_compaction_event_projects_as_it_did() {
    let stored: EventEnvelope = serde_json::from_value(
        json!({"sessionId": "session-1", "eventId": 2, "type": "compaction", "summary": "short"}),
    )
    .expect("a stored compaction still deserializes");
    let mut reducer = ProjectionReducer::new("session-1");
    reducer
        .apply(&event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "compact".to_owned(),
                user_display_content: None,
            },
        ))
        .expect("the prompt applies");
    reducer.apply(&stored).expect("the stored event applies");
    assert!(matches!(
        reducer.state().history.last(),
        Some(PublicHistoryEntry::Checkpoint { metadata, kind, message, .. })
            if kind == "compaction"
                && metadata.generation_status == PublicEntryGenerationStatus::Completed
                && message.as_deref() == Some("short")
    ));
}

/// The reference publishes a clearing as a notice entry as well as a
/// notification, and it names the plan whose acceptance cleared the
/// context. A compaction publishes its checkpoint instead.
#[test]
fn a_cleared_context_leaves_a_notice_a_compaction_does_not() {
    let handoff = |id: u64, from: &str, to: &str, cause: SessionHandoffCause| EventEnvelope {
        session_id: from.to_owned(),
        turn_id: None,
        emitted_at: id,
        working_directory: None,
        event_id: id,
        event: EngineEvent::SessionHandoff {
            from_session_id: from.to_owned(),
            to_session_id: to.to_owned(),
            cause,
        },
    };
    let mut reducer = ProjectionReducer::new("session-1");
    for envelope in [
        event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "clear it".to_owned(),
                user_display_content: None,
            },
        ),
        handoff(2, "session-1", "session-2", SessionHandoffCause::Compaction),
        handoff(
            3,
            "session-2",
            "session-3",
            SessionHandoffCause::ContextCleared {
                plan_file_path: Some("/workspace/plan.md".to_owned()),
            },
        ),
    ] {
        reducer.apply(&envelope).expect("valid lifecycle event");
    }
    let notices = reducer
        .state()
        .history
        .iter()
        .filter_map(|entry| match entry {
            PublicHistoryEntry::Notice {
                metadata, detail, ..
            } => Some((metadata.session_id.clone(), detail.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices,
        vec![(
            "session-3".to_owned(),
            NoticeDetail::ContextCleared {
                plan_file_path: Some("/workspace/plan.md".to_owned()),
            },
        )],
        "only the clearing publishes a notice, and it carries the new session"
    );
}

/// The subagent detail names the session a client may open, which the
/// delegation result is the first thing to report.
#[test]
fn a_subagent_effect_publishes_the_child_session_it_ran() {
    let mut reducer = ProjectionReducer::new("session-1");
    for envelope in [
        event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "delegate".to_owned(),
                user_display_content: None,
            },
        ),
        event(
            2,
            EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "task".to_owned(),
                arguments: r#"{"task":"audit","agent":"explore"}"#.to_owned(),
                remote: None,
            },
        ),
        event(
            3,
            EngineEvent::ToolResult {
                call_id: "call-1".to_owned(),
                content: "done".to_owned(),
                typed_result: json!({
                    "response": "done",
                    "turns_used": 1,
                    "completed": true,
                }),
                projected_result: Value::Null,
                display: json!({
                    "kind": "subagent",
                    "effect": {
                        "parentSessionId": "session-1",
                        "childSessionId": "session-child",
                        "publicSessionId": "session-child",
                        "status": "completed",
                        "result": "done",
                        "turnsUsed": 1,
                        "completed": true,
                    },
                }),
                duration_ms: 1,
                is_error: false,
                cancelled: false,
                skipped: false,
                approval: crate::events::EffectApproval::default(),
            },
        ),
    ] {
        reducer.apply(&envelope).expect("valid lifecycle event");
    }
    let Some(PublicHistoryEntry::Effect { detail, .. }) = reducer
        .state()
        .history
        .iter()
        .find(|entry| matches!(entry, PublicHistoryEntry::Effect { .. }))
    else {
        panic!("the delegation publishes an effect entry");
    };
    assert_eq!(detail.kind, ToolEffectKind::Subagent);
    assert_eq!(detail.child_session_id.as_deref(), Some("session-child"));
}

#[test]
fn streaming_entries_seal_as_a_contiguous_tail() {
    let mut reducer = ProjectionReducer::new("session-1");
    for (id, emitted) in [
        (
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "go".to_owned(),
                user_display_content: None,
            },
        ),
        (
            2,
            EngineEvent::ModelText {
                message_id: None,
                text: "thinking".to_owned(),
            },
        ),
        (
            3,
            EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
                remote: None,
            },
        ),
        (
            4,
            EngineEvent::ToolCall {
                call_id: "call-2".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
                remote: None,
            },
        ),
    ] {
        reducer.apply(&event(id, emitted)).expect("valid event");
    }

    // Sealing walks back from the tail and seals the streamed text, but a
    // call still running stays open until its own result settles it
    // (reference `_complete_streamed_text`).
    let statuses = reducer
        .state()
        .history
        .iter()
        .map(PublicHistoryEntry::is_completed)
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![true, true, false, false]);
}

#[test]
fn public_projection_omits_private_reasoning_signature() {
    let private = ModelMessage::Assistant {
        message_id: None,
        reasoning_message_id: None,
        content: "answer".to_owned(),
        reasoning: Some("private chain".to_owned()),
        reasoning_payloads: vec![
            serde_json::json!({"type": "thinking", "signature": "provider-signature"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ],
        tool_calls: Vec::new(),
        keeps_empty_content: false,
    };
    let encoded_private = serde_json::to_string(&private).expect("private message serializes");
    assert!(encoded_private.contains("provider-signature"));

    let mut reducer = ProjectionReducer::new("session-1");
    reducer
        .apply(&event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "question".to_owned(),
                user_display_content: None,
            },
        ))
        .expect("turn starts");
    reducer
        .apply(&event(
            2,
            EngineEvent::ModelReasoning {
                message_id: None,
                text: "summary".to_owned(),
            },
        ))
        .expect("reasoning projects");
    let public = serde_json::to_string(reducer.state()).expect("public state serializes");
    assert!(!public.contains("provider-signature"));
}
/// Drives one proxied call through the reducer and hands back what the
/// projection published for it.
///
/// The reducer is the only path a client's display ever comes from, so a
/// presentation claim measured anywhere else proves nothing about what a
/// terminal or an editor bridge renders.
fn proxied_effect(
    remote: RemoteToolOrigin,
    name: &str,
    arguments: &str,
    settle: EngineEvent,
) -> (EffectDetail, PublicEffectState) {
    let mut reducer = ProjectionReducer::new("session-1");
    reducer
        .apply(&event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "go".to_owned(),
                user_display_content: None,
            },
        ))
        .expect("the turn starts");
    reducer
        .apply(&event(
            2,
            EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: name.to_owned(),
                arguments: arguments.to_owned(),
                remote: Some(remote),
            },
        ))
        .expect("the proxied call projects");
    reducer.apply(&event(3, settle)).expect("the call settles");
    reducer
        .state()
        .history
        .iter()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Effect { detail, state, .. } => {
                Some(((**detail).clone(), state.clone()))
            }
            _ => None,
        })
        .expect("the projection holds the effect entry")
}

/// The event a server answers a proxied call with.
fn remote_answer(typed_result: Value, is_error: bool) -> EngineEvent {
    EngineEvent::ToolResult {
        call_id: "call-1".to_owned(),
        content: "server unavailable".to_owned(),
        typed_result,
        projected_result: Value::Null,
        display: Value::Null,
        duration_ms: 4,
        is_error,
        cancelled: false,
        skipped: false,
        approval: crate::events::EffectApproval::default(),
    }
}

fn settled_display(state: &PublicEffectState) -> EffectResultDisplay {
    match state {
        PublicEffectState::Completed { display, .. }
        | PublicEffectState::Failed { display, .. } => Some(display.clone()),
        _ => None,
    }
    .expect("the call settled into a completed or a failed state")
}

#[test]
fn a_proxied_call_projects_the_presentation_its_source_names() {
    let (mcp, _) = proxied_effect(
        RemoteToolOrigin::mcp("search"),
        "mcp_docs_search",
        r#"{"query":"parity"}"#,
        remote_answer(json!({"ok": true, "tool": "search"}), false),
    );
    assert_eq!(mcp.display.summary, "mcp_docs_search");
    assert_eq!(mcp.display.status_text, "Calling MCP tool search");
    assert_eq!(
        mcp.display.settled_message.as_deref(),
        Some("mcp_docs_search")
    );

    let (connector, _) = proxied_effect(
        RemoteToolOrigin::connector("issue"),
        "connector_github_issue",
        "",
        remote_answer(json!({"ok": true, "tool": "issue"}), false),
    );
    // No arguments arrived, and the proxy renders none either way, so the
    // summary is the published name with no empty parentheses behind it.
    assert_eq!(connector.display.summary, "connector_github_issue");
    assert_eq!(
        connector.display.status_text,
        "Calling connector tool issue"
    );

    // A builtin keeps the status text its kind publishes.
    let mut reducer = ProjectionReducer::new("session-1");
    reducer
        .apply(&event(
            1,
            EngineEvent::UserMessage {
                attachments: Vec::new(),
                message_id: None,
                content: "go".to_owned(),
                user_display_content: None,
            },
        ))
        .expect("the turn starts");
    reducer
        .apply(&event(
            2,
            EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "bash".to_owned(),
                arguments: r#"{"command":"echo alpha"}"#.to_owned(),
                remote: None,
            },
        ))
        .expect("the builtin call projects");
    let PublicHistoryEntry::Effect { detail, .. } = reducer
        .state()
        .history
        .last()
        .expect("the effect entry")
        .clone()
    else {
        unreachable!("the last entry is the effect just projected")
    };
    assert_eq!(
        detail.display.status_text,
        ToolEffectKind::Shell.status_text()
    );
    assert_eq!(
        detail.display.settled_message.as_deref(),
        Some("echo alpha")
    );
}

#[test]
fn a_proxied_call_settles_from_what_its_server_answered() {
    // A result the server answered names the remote tool.
    let (_, answered) = proxied_effect(
        RemoteToolOrigin::mcp("search"),
        "mcp_docs_search",
        "{}",
        remote_answer(json!({"ok": true, "tool": "search"}), false),
    );
    let display = settled_display(&answered);
    assert!(display.success);
    assert_eq!(display.verb, "Ran");
    assert_eq!(display.message, "search");

    // The connector proxy prefixes it.
    let (_, connector) = proxied_effect(
        RemoteToolOrigin::connector("issue"),
        "connector_github_issue",
        "{}",
        remote_answer(json!({"ok": true, "tool": "issue"}), false),
    );
    assert_eq!(settled_display(&connector).message, "connector issue");

    // A proxy result reporting failure settles as one even though the call
    // itself did not error.
    let (_, refused) = proxied_effect(
        RemoteToolOrigin::mcp("search"),
        "mcp_docs_search",
        "{}",
        remote_answer(
            json!({"ok": false, "server": "docs", "tool": "search"}),
            false,
        ),
    );
    let refused = settled_display(&refused);
    assert!(!refused.success);
    assert_eq!(refused.message, "search");

    // An errored call reports what went wrong, ahead of the result branch.
    let (_, errored) = proxied_effect(
        RemoteToolOrigin::mcp("search"),
        "mcp_docs_search",
        "{}",
        remote_answer(json!({"ok": true, "tool": "search"}), true),
    );
    let errored = settled_display(&errored);
    assert!(!errored.success);
    assert_eq!(errored.message, "server unavailable");

    // An answer shaped like nothing the proxy declares settles as a failure
    // rather than taking the reducer down.
    let (_, unreadable) = proxied_effect(
        RemoteToolOrigin::mcp("search"),
        "mcp_docs_search",
        "{}",
        remote_answer(json!(["not", "a", "result"]), false),
    );
    let unreadable = settled_display(&unreadable);
    assert!(!unreadable.success);
    assert_eq!(unreadable.message, "No result");

    // What a real peer publishes is the server's own `structuredContent`,
    // which shares the proxy result's field names without carrying its
    // meaning. A tool reporting `ok: false` about its own subject settled a
    // successful call as a failure, and one carrying a `tool` key renamed
    // the header, until the settlement stopped reading a payload that names
    // no server.
    let (_, structured) = proxied_effect(
        RemoteToolOrigin::mcp("search"),
        "mcp_docs_search",
        "{}",
        remote_answer(
            json!({"ok": false, "tool": "the server's own field"}),
            false,
        ),
    );
    let structured = settled_display(&structured);
    assert!(structured.success);
    assert_eq!(structured.message, "search");
}

/// The Unified Harness publishes an effect's duration as a float.
#[test]
fn a_unified_effect_reads_with_a_fractional_duration() {
    let entry: PublicHistoryEntry = serde_json::from_value(json!({"createdAt": 1, "detail": {"display": {"content": null, "message": "quotes.txt", "settledMessage": "quotes.txt", "settledVerb": "Edited", "statusText": "Editing quotes", "suffix": "", "summary": "Edit quotes.txt", "verb": "Editing"}, "input": {"changes": [{"newString": "say \"hi\"\tnow", "oldString": "say 'hi'", "replaceAll": false}], "filePath": "quotes.txt"}, "kind": "file_edit", "toolName": "file_system.search_replace"}, "generationStatus": "completed", "id": "e1", "inputEntryId": "u1", "relatedEntryId": null, "sessionId": "s1", "state": {"approvalSource": "config", "approvalType": "always", "decision": "execute", "display": {"approvalNote": null, "message": "/w/quotes.txt", "success": true, "suffix": "", "verb": "Edited", "warnings": []}, "durationMs": 12.6, "output": {"file": "/w/quotes.txt", "newString": null, "occurrences": [{"newText": "say \"hi\"\tnow", "oldText": "say 'hi'", "startLine": 1}], "oldString": null}, "outputText": "", "status": "completed"}, "title": "file_system.search_replace", "turnId": "t1", "type": "effect", "updatedAt": 2})).unwrap();
    let PublicHistoryEntry::Effect {
        state: PublicEffectState::Completed { duration_ms, .. },
        ..
    } = entry
    else {
        panic!("an effect entry");
    };
    assert_eq!(duration_ms, 13);
}
