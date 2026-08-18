//! The programmatic surface: what crosses the JSON boundary, and what a thin
//! client can drive without a transport.

use super::*;

#[tokio::test]
async fn thin_client_uses_only_serialized_app_server_contracts() {
    let mut service =
        HeadlessService::new(EchoTurnDriver::new("hello back")).expect("service starts");
    let session_id = service.start_session(&options()).expect("session starts");
    let (observer, mut updates) = programmatic_update_channel(&session_id);
    let turn = service
        .prompt_observed(&session_id, "hello", observer)
        .await
        .expect("turn completes");
    assert_eq!(turn.final_assistant, "hello back");
    assert_eq!(turn.history.len(), 2);
    assert_eq!(turn.events.len(), 3);
    assert_eq!(turn.stop_reason, PublicTurnStopReason::Complete);
    let mut update_count = 0;
    while let Ok(update) = updates.try_recv() {
        let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
            continue;
        };
        assert_eq!(entry.metadata().turn_id.as_deref(), Some("turn-1"));
        update_count += 1;
    }
    assert_eq!(update_count, 2);
    service
        .close_session(&session_id)
        .await
        .expect("session closes");
    service.shutdown().expect("connection shuts down");
}

#[tokio::test]
async fn public_calls_preserve_notifications_and_execute_resource_work() {
    let workspace = tempfile::tempdir().expect("workspace");
    let release4 = Release4Service::with_backends(
        Arc::new(ProgrammaticProjects),
        Arc::new(ProgrammaticTeleport),
        Arc::new(ProgrammaticGit),
    )
    .with_loop_store(workspace.path().join("loops.json"))
    .expect("loop store");
    let mut service = HeadlessService::new_shared_with_server(
        Arc::new(EchoTurnDriver::new("unused")),
        AppServer::with_release4_service(release4),
    )
    .expect("service starts");
    let mut session_options = options();
    session_options.session_id = Some("public-dispatch".to_owned());
    session_options.working_directory = workspace.path().to_string_lossy().into_owned();
    session_options.trusted = false;
    let session_id = service
        .start_session(&session_options)
        .expect("session starts");

    let picker = service
        .public_call_async(
            "vibeCode/projects/open",
            json!({
                "sessionId": session_id,
                "workingDirectory": workspace.path(),
                "purpose": "configure",
            }),
        )
        .await
        .expect("project picker opens")
        .result;
    let picker_id = picker["pickerId"].as_str().expect("picker ID");
    service
        .public_call(
            "vibeCode/projects/select",
            json!({
                "sessionId": session_id,
                "pickerId": picker_id,
                "projectId": "project-public-dispatch",
            }),
        )
        .expect("project selects");
    let programmatic = service
        .teleport(
            &session_id,
            &workspace.path().to_string_lossy(),
            "continue",
            false,
        )
        .await
        .expect("programmatic Teleport completes");
    assert!(matches!(
        programmatic.as_slice(),
        [
            ProgrammaticTeleportEvent::SummarizingContext { .. },
            ProgrammaticTeleportEvent::CheckingGit { .. },
            ProgrammaticTeleportEvent::StartingWorkflow { .. },
            ProgrammaticTeleportEvent::Complete { .. },
        ]
    ));
    let teleport = service
        .public_call_async(
            "vibeCode/teleport/start",
            json!({
                "sessionId": session_id,
                "pickerId": picker_id,
                "projectId": "project-public-dispatch",
                "operationId": "teleport-public-dispatch",
                "workingDirectory": workspace.path(),
            }),
        )
        .await
        .expect("response and notifications decode together");
    assert_eq!(
        teleport.result["operationId"],
        json!("teleport-public-dispatch")
    );
    assert_eq!(teleport.notifications.len(), 4);
    assert_eq!(
        teleport
            .notifications
            .last()
            .map(|event| event.method.as_str()),
        Some("vibeCode/teleport/event")
    );
    assert_eq!(
        teleport
            .notifications
            .last()
            .map(|event| &event.params["event"]["kind"]),
        Some(&json!("complete"))
    );

    let trusted = service
        .public_call_async(
            "workspace/trust/decision",
            json!({
                "sessionId": session_id,
                "cwd": workspace.path(),
                "decision": "trust_cwd",
            }),
        )
        .await
        .expect("deferred resource response");
    assert!(trusted.result.is_empty());
    assert_eq!(
        trusted
            .notifications
            .first()
            .map(|event| event.method.as_str()),
        Some("runtime/updated")
    );
    let integrations = service
        .public_call_async(
            "mcp/read",
            json!({
                "sessionId": session_id,
            }),
        )
        .await
        .expect("deferred MCP resource response");
    assert!(integrations.result["mcp"]["sources"].is_array());
}
