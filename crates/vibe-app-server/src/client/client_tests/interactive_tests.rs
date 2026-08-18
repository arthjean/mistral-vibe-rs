//! Callbacks a person answers, serialized through the server, and the observer
//! that sequences what a client reads.

use super::*;

/// The clearing a tool raises reaches the driver bound to the turn the
/// server reserved, which is the identifier the tool cannot know.
#[tokio::test]
async fn a_raised_clearing_reaches_the_driver_with_the_reserved_turn() {
    /// One clearing as the driver received it.
    #[derive(Debug, PartialEq, Eq)]
    struct RecordedClearing {
        session_id: String,
        turn_id: String,
        continuation: String,
        plan_file_path: Option<String>,
    }

    #[derive(Default)]
    struct RecordingDriver {
        clearings: Mutex<Vec<RecordedClearing>>,
    }

    impl TurnDriver for RecordingDriver {
        fn run<'a>(&'a self, _reservation: &'a TurnReservation) -> DriverFuture<'a> {
            Box::pin(async { Err(DriverError::UnsupportedControl("turn/start")) })
        }

        fn clear_context(
            &self,
            session_id: &str,
            turn_id: &str,
            continuation: &str,
            plan_file_path: Option<&str>,
        ) -> Result<(), DriverError> {
            self.clearings
                .lock()
                .map_err(|_| DriverError::StatePoisoned)?
                .push(RecordedClearing {
                    session_id: session_id.to_owned(),
                    turn_id: turn_id.to_owned(),
                    continuation: continuation.to_owned(),
                    plan_file_path: plan_file_path.map(str::to_owned),
                });
            Ok(())
        }
    }

    let (sender, receiver) =
        tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(MAX_INTERACTIVE_CALLBACKS);
    let driver = Arc::new(RecordingDriver::default());
    let server = AppServer::default().using_surface_extension(
        Arc::new(InteractiveApprovalFactory {
            sender: sender.clone(),
        }),
        Arc::new(InteractiveSessionToolFactory {
            sender: sender.clone(),
            plan_directory: None,
        }),
    );
    let mut service = HeadlessService {
        client: InProcessClient::connect_with_server_and_client(
            server,
            ClientInfo {
                name: "clearing-test".to_owned(),
                version: "1".to_owned(),
                title: None,
                entrypoint: ClientEntrypoint::Cli,
                terminal_emulator: TerminalEmulator::Unknown,
            },
            ClientCapabilities {
                callback_kinds: vec![ClientCallbackKind::UserInput],
                ..ClientCapabilities::default()
            },
        )
        .expect("client connects"),
        driver: Arc::clone(&driver),
        interactive_callbacks: Some(receiver),
        interactive_backlog: VecDeque::new(),
        pending_interactive_callbacks: HashMap::new(),
    };
    let session_id = service.start_session(&options()).expect("session starts");
    let reservation = service
        .reserve_prompt(&session_id, &TurnRequest::text("plan"))
        .await
        .expect("turn reserves");

    let (response, acknowledgment) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::ClearContext {
            session_id: session_id.clone(),
            continuation: "Plan approved.".to_owned(),
            plan_file_path: Some("/plans/session.md".to_owned()),
            response,
        })
        .await
        .expect("clearing queues");
    assert!(
        service
            .drain_callbacks()
            .expect("the clearing drains")
            .is_empty(),
        "a clearing is not a callback entry"
    );
    acknowledgment
        .await
        .expect("the tool is answered")
        .expect("the driver accepted the clearing");
    assert_eq!(
        driver.clearings.lock().expect("clearings").as_slice(),
        [RecordedClearing {
            session_id: session_id.clone(),
            turn_id: reservation.turn_id.clone(),
            continuation: "Plan approved.".to_owned(),
            plan_file_path: Some("/plans/session.md".to_owned()),
        }]
    );
    service
        .fail_reserved(
            &reservation,
            "fixture complete",
            TurnErrorCode::InternalError,
        )
        .expect("fixture turn closes");
}

#[tokio::test]
async fn interactive_user_input_callbacks_serialize_and_resolve_through_the_server() {
    let mut service = HeadlessService::new_interactive_shared_with_server(
        Arc::new(EchoTurnDriver::new("unused")),
        AppServer::default(),
    )
    .expect("interactive service starts");
    let mut session_options = options();
    session_options.auto_approve = false;
    session_options.enabled_tools.clear();
    session_options.disabled_tools.clear();
    session_options.tool_filters.clear();
    let session_id = service
        .start_session(&session_options)
        .expect("session starts");
    let reservation = service
        .reserve_prompt(&session_id, &TurnRequest::text("ask"))
        .await
        .expect("turn reserves");
    let arguments = json!({
        "questions": [{
            "question": "Language?",
            "header": "Runtime",
            "options": [
                {"label": "Rust", "description": "Native"},
                {"label": "Python", "description": "Dynamic"}
            ],
            "multiSelect": false,
            "hideOther": false
        }]
    })
    .to_string();
    let first_tools = reservation.tools.clone();
    let first_arguments = arguments.clone();
    let first = tokio::spawn(async move {
        first_tools
            .execute("ask_user_question", &first_arguments)
            .await
    });
    let second_tools = reservation.tools.clone();
    let second =
        tokio::spawn(async move { second_tools.execute("ask_user_question", &arguments).await });

    let first_entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entries = service
                .drain_callbacks()
                .expect("callback queue remains valid");
            if let Some(entry) = entries.into_iter().next() {
                break entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first callback arrives");
    assert!(
        service
            .drain_callbacks()
            .expect("second request remains queued")
            .is_empty()
    );
    assert!(matches!(first_entry, PublicHistoryEntry::Callback { .. }));
    let PublicHistoryEntry::Callback {
        callback_id: first_callback_id,
        ..
    } = first_entry
    else {
        return;
    };
    service
        .respond_callback(json!({
            "sessionId": session_id,
            "callbackId": first_callback_id,
            "output": {
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "Language?",
                        "answer": "Rust",
                        "isOther": false
                    }],
                    "cancelled": false
                }
            }
        }))
        .expect("first response is accepted");

    let second_entry = service
        .drain_callbacks()
        .expect("queued callback opens")
        .pop()
        .expect("second callback is delivered");
    assert!(matches!(second_entry, PublicHistoryEntry::Callback { .. }));
    let PublicHistoryEntry::Callback {
        callback_id: second_callback_id,
        ..
    } = second_entry
    else {
        return;
    };
    service
        .respond_callback(json!({
            "sessionId": session_id,
            "callbackId": second_callback_id,
            "output": {
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "Language?",
                        "answer": "Python",
                        "isOther": false
                    }],
                    "cancelled": false
                }
            }
        }))
        .expect("second response is accepted");

    for task in [first, second] {
        let output = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("tool callback unblocks")
            .expect("tool task joins")
            .expect("tool returns output");
        assert_eq!(output.typed_result["cancelled"], false);
    }
    service
        .fail_reserved(
            &reservation,
            "fixture complete",
            TurnErrorCode::InternalError,
        )
        .expect("fixture turn closes");
}

#[tokio::test]
async fn interactive_approval_callback_returns_the_exact_policy_decision() {
    let (sender, receiver) =
        tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(MAX_INTERACTIVE_CALLBACKS);
    let driver = Arc::new(EchoTurnDriver::new("unused"));
    let server = AppServer::default().using_surface_extension(
        Arc::new(InteractiveApprovalFactory {
            sender: sender.clone(),
        }),
        Arc::new(InteractiveSessionToolFactory {
            sender: sender.clone(),
            plan_directory: driver.plan_directory(),
        }),
    );
    let mut service = HeadlessService {
        client: InProcessClient::connect_with_server_and_client(
            server,
            ClientInfo {
                name: "approval-test".to_owned(),
                version: "1".to_owned(),
                title: None,
                entrypoint: ClientEntrypoint::Cli,
                terminal_emulator: TerminalEmulator::Unknown,
            },
            ClientCapabilities {
                callback_kinds: vec![ClientCallbackKind::Approval],
                ..ClientCapabilities::default()
            },
        )
        .expect("client connects"),
        driver,
        interactive_callbacks: Some(receiver),
        interactive_backlog: VecDeque::new(),
        pending_interactive_callbacks: HashMap::new(),
    };
    let mut session_options = options();
    session_options.auto_approve = false;
    let session_id = service
        .start_session(&session_options)
        .expect("session starts");
    let reservation = service
        .reserve_prompt(&session_id, &TurnRequest::text("approve"))
        .await
        .expect("turn reserves");
    let approval_agent = InteractiveApprovalAgent {
        session_id: session_id.clone(),
        sender,
    };
    let requested = tokio::spawn(async move {
        approval_agent
            .request(ApprovalRequest {
                tool: "shell".to_owned(),
                input: json!({"command": "cargo test"}),
                requirements: vec![vibe_core::policy::PermissionRequirement::command(
                    "cargo test",
                )],
                rationale: "shell command requires approval".to_owned(),
            })
            .await
    });
    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(entry) = service
                .drain_callbacks()
                .expect("callback queue remains valid")
                .pop()
            {
                break entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval callback arrives");
    assert!(matches!(entry, PublicHistoryEntry::Callback { .. }));
    let PublicHistoryEntry::Callback {
        callback_id,
        detail,
        ..
    } = entry
    else {
        return;
    };
    let detail = serde_json::to_value(&detail).expect("the callback detail serializes");
    assert_eq!(
        detail["requiredPermissions"],
        json!([{
            "scope": "command_pattern",
            "invocationPattern": "cargo test",
            "sessionPattern": "cargo test *",
            "label": "cargo test *",
        }])
    );
    assert_eq!(detail["effect"]["input"], json!({"command": "cargo test"}));
    assert_eq!(detail["effect"]["kind"], "shell");
    service
        .respond_callback(json!({
            "sessionId": session_id,
            "callbackId": callback_id,
            "output": {
                "type": "approval",
                "decision": {"type": "approve_for_session"}
            }
        }))
        .expect("approval response is accepted");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), requested)
            .await
            .expect("approval unblocks")
            .expect("approval task joins")
            .expect("approval succeeds"),
        ApprovalDecision::ApproveForSession
    );
    service
        .fail_reserved(
            &reservation,
            "fixture complete",
            TurnErrorCode::InternalError,
        )
        .expect("fixture turn closes");
}

#[tokio::test]
async fn server_bound_observer_sequences_repeated_turns_without_replaying_history() {
    let mut service = HeadlessService::new_shared_with_server(
        Arc::new(EchoTurnDriver::new("reply")),
        AppServer::default(),
    )
    .expect("service starts");
    let session_id = service.start_session(&options()).expect("session starts");

    let first = service
        .reserve_prompt(&session_id, &TurnRequest::text("first"))
        .await
        .expect("first turn reserves");
    let (first_observer, mut first_updates) = service
        .interactive_update_channel_after(&session_id, &first.turn_id, 0)
        .expect("first observer binds");
    let first_outcome = service
        .driver()
        .run_observed(&first, first_observer)
        .await
        .expect("first turn runs");
    let mut first_event_ids = Vec::new();
    while let Ok(update) = first_updates.try_recv() {
        let ProgrammaticUpdate::HistoryEntry {
            event_id, entry, ..
        } = update
        else {
            continue;
        };
        assert_eq!(
            entry.metadata().turn_id.as_deref(),
            Some(first.turn_id.as_str())
        );
        first_event_ids.push(event_id);
    }
    assert!(!first_event_ids.is_empty());
    service
        .finish_reserved(&first, first_outcome)
        .expect("first turn finishes");
    let first_watermark = service
        .public_call("session/read", json!({"sessionId": session_id}))
        .expect("canonical state reads")["state"]["eventId"]
        .as_u64()
        .expect("canonical watermark");

    let second = service
        .reserve_prompt(&session_id, &TurnRequest::text("second"))
        .await
        .expect("second turn reserves");
    let (second_observer, mut second_updates) = service
        .interactive_update_channel_after(&session_id, &second.turn_id, first_watermark)
        .expect("second observer binds");
    let second_outcome = service
        .driver()
        .run_observed(&second, second_observer)
        .await
        .expect("second turn runs");
    let mut second_event_ids = Vec::new();
    while let Ok(update) = second_updates.try_recv() {
        let ProgrammaticUpdate::HistoryEntry {
            event_id, entry, ..
        } = update
        else {
            continue;
        };
        assert_eq!(
            entry.metadata().turn_id.as_deref(),
            Some(second.turn_id.as_str())
        );
        assert!(event_id > first_watermark);
        second_event_ids.push(event_id);
    }
    assert!(!second_event_ids.is_empty());
    assert!(
        second_event_ids
            .windows(2)
            .all(|window| window[1] == window[0].saturating_add(1))
    );
    service
        .finish_reserved(&second, second_outcome)
        .expect("second turn finishes");
}
