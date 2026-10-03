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
    let projects = ProjectsService::default()
        .with_loop_store(workspace.path().join("loops.json"))
        .expect("loop store");
    let mut service = HeadlessService::new_shared_with_server(
        Arc::new(EchoTurnDriver::new("unused")),
        AppServer::with_projects_service(projects),
    )
    .expect("service starts");
    let mut session_options = options();
    session_options.session_id = Some("public-dispatch".to_owned());
    session_options.working_directory = workspace.path().to_string_lossy().into_owned();
    session_options.trusted = false;
    let session_id = service
        .start_session(&session_options)
        .expect("session starts");

    // A decision is only offered about a workspace holding a file that trust
    // would unlock, as reference `decide_workspace_trust` requires.
    std::fs::write(workspace.path().join("AGENTS.md"), "guidance\n").expect("AGENTS.md");
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
    assert_eq!(trusted.result["status"], json!("trusted"));
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
