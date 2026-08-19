//! Opening a session through the in-process client: the diagnostics it reports
//! once, the trust transition it honors, and the images it validates.

use super::*;

#[test]
fn plan_review_tool_is_absent_without_a_canonical_plan_directory() {
    let (sender, _receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
    let tools = ToolRegistry::default();
    InteractiveSessionToolFactory {
        sender,
        plan_directory: None,
    }
    .register("session", &tools)
    .expect("question tool registers");

    let names = tools
        .list()
        .expect("tools list")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["ask_user_question"]);
}

#[tokio::test]
async fn explicit_session_initialization_reports_mcp_diagnostics_once() {
    let temporary = tempfile::tempdir().expect("runtime home");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".vibe")).expect("project config directory");
    std::fs::write(
        workspace.join(".vibe/config.toml"),
        r#"
[[mcp_servers]]
name = "broken"
transport = "stdio"
command = "/must-not-run"
"#,
    )
    .expect("project MCP config");
    let workspace_service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("workspace service");
    let mut service = HeadlessService::new_shared_with_server(
        Arc::new(EchoTurnDriver::new("unused")),
        AppServer::with_workspace_service(workspace_service),
    )
    .expect("service starts");
    let mut session_options = options();
    session_options.session_id = Some("mcp-failure".to_owned());
    session_options.working_directory = workspace.to_string_lossy().into_owned();
    let session_id = service
        .start_session(&session_options)
        .expect("session starts before deferred MCP initialization");
    let diagnostics = service
        .initialize_pending_mcp(&session_id)
        .await
        .expect("MCP discovery failure is recoverable");
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("broken"));
    assert!(
        service
            .initialize_pending_mcp(&session_id)
            .await
            .expect("initialization is consumed once")
            .is_empty()
    );
}

#[tokio::test]
async fn trust_transition_rebinds_session_scoped_project_config_writes() {
    let temporary = tempfile::tempdir().expect("runtime home");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".vibe")).expect("project config directory");
    std::fs::write(workspace.join(".vibe/config.toml"), "").expect("project config fixture");
    let workspace_service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace.clone(),
            session_root: temporary.path().join("sessions"),
        },
        false,
    )
    .expect("workspace service");
    let mut service = HeadlessService::new_shared_with_server(
        Arc::new(EchoTurnDriver::new("unused")),
        AppServer::with_workspace_service(workspace_service),
    )
    .expect("service starts");
    let mut session_options = options();
    session_options.session_id = Some("trust-config".to_owned());
    session_options.working_directory = workspace.to_string_lossy().into_owned();
    session_options.trusted = false;
    let session_id = service
        .start_session(&session_options)
        .expect("untrusted session starts");

    let untrusted_write = service.public_call(
        "config/batchWrite",
        json!({
            "sessionId": session_id,
            "writes": [{
                "target": "project",
                "expectedFingerprint": null,
                "mutations": [{"path": ["theme"], "value": "dark"}],
            }],
        }),
    );
    assert!(
        untrusted_write.is_err(),
        "untrusted project write is rejected"
    );

    service
        .public_call_async(
            "workspace/trust/decision",
            json!({
                "sessionId": session_id,
                "cwd": workspace,
                "decision": "trust_cwd",
            }),
        )
        .await
        .expect("workspace trust commits");
    // Trust moved the selected target onto the project file, which the
    // published field surface reports as the first writable target.
    let trusted = service
        .public_call("config/fields/read", json!({"sessionId": session_id}))
        .expect("trusted config reads");
    assert_eq!(trusted["targets"][0], json!("project"));
    // The write names no fingerprint: the server takes the one on disk
    // inside the transaction that compares it.
    service
        .public_call(
            "config/batchWrite",
            json!({
                "sessionId": session_id,
                "writes": [{
                    "target": "project",
                    "mutations": [{"path": ["theme"], "value": "dark"}],
                }],
            }),
        )
        .expect("trusted project write commits");
    assert!(
        std::fs::read_to_string(workspace.join(".vibe/config.toml"))
            .expect("project config persisted")
            .contains("theme = \"dark\"")
    );
}

#[test]
fn mcp_initialization_reads_warnings_and_rejects_anything_else() {
    let notification = |method: &str, params: serde_json::Value| {
        serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .expect("notification frame")
    };
    let frames = vec![
        notification("runtime/updated", json!({"sessionId": "s", "runtime": {}})),
        notification(
            "warning",
            json!({"warning": {"message": "connection failed"}}),
        ),
        notification(
            "warning",
            json!({"warning": {"message": "connection failed"}}),
        ),
    ];
    assert_eq!(
        decode_mcp_warnings(&frames).expect("typed warnings"),
        vec!["connection failed"],
        "a repeated diagnostic is reported once"
    );

    for malformed in [
        vec![notification("warning", json!({"warning": {"code": "x"}}))],
        vec![notification("warning", json!({"warning": {"message": 7}}))],
        vec![notification("mcp/updated", json!({"mcp": {}}))],
    ] {
        assert!(matches!(
            decode_mcp_warnings(&malformed),
            Err(ClientError::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn prepared_prompt_reserves_already_validated_images() {
    let mut service = HeadlessService::new_shared(Arc::new(EchoTurnDriver::new("unused")))
        .expect("service starts");
    let session_id = service.start_session(&options()).expect("session starts");
    let image = ImageInput {
        media_type: "image/png".to_owned(),
        data: "aW1hZ2U=".to_owned(),
    };
    let turn = TurnRequest {
        prompt: "inspect @image.png".to_owned(),
        input: vec![
            PublicContentBlock::Text {
                text: "inspect @image.png".to_owned(),
            },
            PublicContentBlock::Image {
                attachment: json!({
                    "source": {"kind": "file", "path": "/missing/image.png"},
                    "alias": "image.png",
                    "mimeType": "image/png",
                }),
            },
        ],
        injected: false,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: None,
        mention_stats: None,
    };

    for invalid in [
        ImageInput {
            media_type: "image/png".to_owned(),
            data: "not-base64".to_owned(),
        },
        ImageInput {
            media_type: "image/png".to_owned(),
            data: "A".repeat(
                usize::try_from(vibe_core::images::MAX_IMAGE_BYTES)
                    .expect("image limit fits usize")
                    .saturating_add(2)
                    / 3
                    * 4
                    + 1,
            ),
        },
    ] {
        assert!(PreparedImages::try_new(vec![invalid]).is_err());
    }
    let no_images = PreparedImages::try_new(Vec::new()).expect("empty prepared image set");
    let error = service
        .reserve_prepared_prompt(&session_id, &turn, no_images)
        .await
        .expect_err("mismatched prepared images fail before reservation");
    assert!(error.to_string().contains("provider images"));
    let prepared_images =
        PreparedImages::try_new(vec![image.clone()]).expect("valid prepared image");
    let reservation = service
        .reserve_prepared_prompt(&session_id, &turn, prepared_images.clone())
        .await
        .expect("prepared prompt reserves without rereading its public file source");

    assert_eq!(reservation.prepared_images, Some(prepared_images));
    service
        .fail_reserved(&reservation, "test cleanup", TurnErrorCode::InternalError)
        .expect("reservation cleanup");
}
