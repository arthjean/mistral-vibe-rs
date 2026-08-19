//! The resource surface and the telemetry a client records through it.

use super::*;

/// US-005: the managed shell family is selected by
/// `managed_shell_tools_enabled`, which is the field the experiments layer
/// writes and the only switch the reference reads.
///
/// Absent and `false` publish the one-shot command tool alone; `true` adds
/// the four session tools, and the managed variant wins the family name by
/// selection priority.
#[tokio::test]
async fn the_managed_shell_family_is_selected_by_the_configuration_field() {
    for (document, managed) in [
        ("", false),
        ("managed_shell_tools_enabled = false\n", false),
        ("managed_shell_tools_enabled = true\n", true),
    ] {
        let temporary = tempfile::tempdir().expect("temporary home");
        let home = temporary.path().join("home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&home).expect("home directory");
        std::fs::create_dir_all(&workspace).expect("workspace directory");
        if !document.is_empty() {
            std::fs::write(home.join("config.toml"), document).expect("configuration fixture");
        }
        let workspace_service = WorkspaceService::new(
            crate::workspace::WorkspacePaths {
                vibe_home: home,
                working_directory: workspace.clone(),
                session_root: temporary.path().join("sessions"),
            },
            false,
        )
        .expect("workspace service");
        let server = AppServer::with_workspace_service(workspace_service);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": workspace}),
        ));

        let listed =
            connection.dispatch(&request(3, "tools/list", json!({"sessionId": "session-1"})));
        let published = match decode_frame(&listed.outbound[0]).expect("tools response") {
            Envelope::Success(SuccessResponse { result, .. }) => result["tools"]
                .as_array()
                .map(|published| {
                    published
                        .iter()
                        .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
                        .collect::<BTreeSet<_>>()
                })
                .expect("tools/list answers with the published names"),
            other => unreachable!("tools/list did not answer: {other:?}"),
        };
        assert!(
            published.contains("bash"),
            "`{document}` published no command tool: {published:?}"
        );
        for session_tool in [
            "bash_stdin",
            "bash_output",
            "bash_sessions",
            "bash_log_file",
        ] {
            assert_eq!(
                published.contains(session_tool),
                managed,
                "`{document}` disagrees on `{session_tool}`: {published:?}"
            );
        }
    }
}

#[tokio::test]
async fn operational_resources_are_typed_and_transport_failures_are_canonical() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let tools = connection.dispatch(&request(3, "tools/list", json!({"sessionId": "session-1"})));
    // `/workspace` is not a usable root, so the file tools stay unregistered
    // while the universal tools, which need no root, are still published.
    let published = match decode_frame(&tools.outbound[0]).expect("tools response") {
        Envelope::Success(SuccessResponse { result, .. }) => {
            result["tools"].as_array().map(|published| {
                published
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
                    .collect::<BTreeSet<_>>()
            })
        }
        _ => None,
    }
    .expect("tools/list answers with the published names");
    for universal in ["skill", "todo", "web_fetch"] {
        assert!(published.contains(universal), "{published:?}");
    }
    for workspace_tool in ["edit", "grep", "read_file", "write_file"] {
        assert!(!published.contains(workspace_tool), "{published:?}");
    }

    let add = connection.dispatch(&request(
        4,
        "mcp/add",
        json!({
            "sessionId": "session-1",
            "url": "https://127.0.0.1:9/mcp",
            "name": "example"
        }),
    ));
    let deferred = add.deferred.first();
    assert!(matches!(
        deferred,
        Some(DeferredWork::ResourceRequest { .. })
    ));
    let Some(DeferredWork::ResourceRequest {
        request_id,
        session_id,
        command,
    }) = deferred
    else {
        return;
    };
    let add = server
        .execute_resource_request(request_id.clone(), session_id.clone(), command.clone())
        .await;
    let Envelope::Success(SuccessResponse { result, .. }) =
        decode_frame(&add.outbound[0]).expect("MCP response")
    else {
        unreachable!("mcp/add answers");
    };
    assert_eq!(
        result.keys().map(String::as_str).collect::<Vec<_>>(),
        ["created", "name", "runtime", "url"]
    );
    assert_eq!(result["created"], json!(true));
    assert_eq!(result["name"], json!("example"));
    assert_eq!(result["url"], json!("https://127.0.0.1:9/mcp"));
    // The source could not be reached, so it is published as unavailable
    // rather than as a switch the operator threw.
    assert_eq!(
        result["runtime"]["mcp"]["sources"][0]["status"],
        json!("unavailable")
    );
    assert!(
        result["runtime"]["mcp"]["discoveryErrors"]["example"]
            .as_str()
            .is_some_and(|message| message.contains("MCP `example`")),
        "the source that would not start is named: {}",
        result["runtime"]["mcp"]
    );
    // The change and the problem cross under their reference names.
    assert!(matches!(
        decode_frame(&add.outbound[1]).expect("MCP notification"),
        Envelope::Notification(Notification { method, .. }) if method == "runtime/updated"
    ));
    assert!(matches!(
        decode_frame(&add.outbound[2]).expect("MCP warning"),
        Envelope::Notification(Notification { method, ref params, .. })
            if method == "warning"
                && params["warning"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("MCP `example`"))
    ));
}

#[tokio::test]
async fn attached_resource_backend_uses_session_tools_and_returns_canonical_state() {
    let workspace = tempfile::tempdir().expect("workspace");
    let backend = Arc::new(RecordingResourceBackend::default());
    let server = AppServer::with_resource_backend(backend.clone());
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
    assert!(
        backend
            .opened_with_tools
            .lock()
            .expect("backend state")
            .is_some_and(|count| count > 0)
    );

    let add = connection.dispatch(&request(
        3,
        "mcp/add",
        json!({
            "sessionId": "session-1",
            "url": "https://mcp.example",
            "name": "example"
        }),
    ));
    assert!(add.outbound.is_empty());
    let deferred = add.deferred.first();
    assert!(matches!(
        deferred,
        Some(DeferredWork::ResourceRequest { .. })
    ));
    let Some(DeferredWork::ResourceRequest {
        request_id,
        session_id,
        command,
    }) = deferred
    else {
        return;
    };
    let added = server
        .execute_resource_request(request_id.clone(), session_id.clone(), command.clone())
        .await;
    assert_eq!(added.outbound.len(), 2);
    assert!(matches!(
        decode_frame(&added.outbound[0]).expect("response"),
        Envelope::Success(SuccessResponse {
            id: RequestId::Integer(3),
            ..
        })
    ));
    assert!(matches!(
        decode_frame(&added.outbound[1]).expect("notification"),
        Envelope::Notification(Notification { method, .. }) if method == "runtime/updated"
    ));

    let read = connection.dispatch(&request(4, "mcp/read", json!({"sessionId": "session-1"})));
    let deferred = read.deferred.first();
    assert!(matches!(
        deferred,
        Some(DeferredWork::ResourceRequest { .. })
    ));
    let Some(DeferredWork::ResourceRequest {
        request_id,
        session_id,
        command,
    }) = deferred
    else {
        return;
    };
    let read = server
        .execute_resource_request(request_id.clone(), session_id.clone(), command.clone())
        .await;
    assert!(matches!(
        decode_frame(&read.outbound[0]).expect("canonical state"),
        Envelope::Success(SuccessResponse { result, .. })
            if result["mcp"]["sources"] == json!(["example"])
    ));

    let close = connection.dispatch(&request(
        5,
        "session/close",
        json!({"sessionId": "session-1"}),
    ));
    let deferred = close.deferred.last();
    assert!(matches!(
        deferred,
        Some(DeferredWork::CloseResources { .. })
    ));
    let Some(DeferredWork::CloseResources {
        session_id,
        generation,
    }) = deferred
    else {
        return;
    };
    server
        .close_resource_session(session_id, *generation)
        .await
        .expect("resource cleanup");
    assert_eq!(
        *backend.closed.lock().expect("closed state"),
        vec!["session-1".to_owned()]
    );
}

/// The client's own event stream, which the reference hands to the agent
/// loop's telemetry client and this port writes to the log file an operator
/// reads. Both gate it on `enable_telemetry`, so a client that records
/// against a session with telemetry off leaves nothing behind.
///
/// The sink is opened at `DEBUG` rather than at the level the environment
/// resolves, because what is under test is the gate: an informational
/// record is invisible under the shipped `WARNING` default, upstream as
/// much as here.
#[test]
fn a_recorded_client_event_is_kept_only_while_telemetry_is_enabled() {
    for enabled in [true, false] {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
        fs::create_dir_all(&vibe_home).expect("user config directory");
        fs::write(
            working_directory.join(".vibe/config.toml"),
            format!("enable_telemetry = {enabled}\n"),
        )
        .expect("telemetry configuration");
        let workspace_service = WorkspaceService::new(
            crate::workspace::WorkspacePaths {
                vibe_home,
                working_directory: working_directory.clone(),
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("workspace service");
        let server = AppServer::with_workspace_service(workspace_service).logging_to(FileLog::new(
            temporary.path().join("logs").join("vibe.log"),
            LogSettings {
                level: LogLevel::Debug,
                ..LogSettings::default()
            },
        ));
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": working_directory}),
        ));

        let recorded = connection.dispatch(&request(
            3,
            "telemetry/record",
            json!({
                "sessionId": "session-1",
                "name": "vibe.client_ready",
                "properties": {"surface": "editor"},
                "correlateLastRequest": true
            }),
        ));
        match decode_frame(&recorded.outbound[0]).expect("a recorded answer") {
            Envelope::Success(SuccessResponse { result, .. }) => {
                assert!(result.is_empty(), "the answer is empty: {result:?}");
            }
            other => unreachable!("telemetry/record did not answer: {other:?}"),
        }

        let logs = call(&mut connection, 4, "diagnostics/logs/read");
        let entries = logs["logs"]["entries"]
            .as_array()
            .expect("a log page")
            .iter()
            .filter(|entry| {
                entry["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("vibe.client_ready"))
            })
            .count();
        assert_eq!(
            entries,
            usize::from(enabled),
            "enable_telemetry = {enabled} decides whether the event is kept"
        );
    }
}

/// What a client-authored event handed to the telemetry client looks like.
#[derive(Debug, Clone, PartialEq)]
struct RecordedClientEvent {
    name: String,
    properties: serde_json::Map<String, Value>,
    session_id: Option<String>,
    correlate_last_request: bool,
}

#[derive(Default)]
struct RecordingClientTelemetry {
    events: Mutex<Vec<RecordedClientEvent>>,
}

impl ClientTelemetry for RecordingClientTelemetry {
    fn record_client_event(
        &self,
        name: &str,
        properties: serde_json::Map<String, Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    ) {
        if let Ok(mut events) = self.events.lock() {
            events.push(RecordedClientEvent {
                name: name.to_owned(),
                properties,
                session_id: session_id.map(ToOwned::to_owned),
                correlate_last_request,
            });
        }
    }
}

/// Reference `_dispatch_telemetry` hands the recorded event to the agent
/// loop's telemetry client, which ships it under the open-properties
/// envelope. The name and the properties are the client's, so neither is
/// rewritten, and the same key that keeps the event off the log keeps it
/// off the wire.
#[test]
fn a_recorded_client_event_reaches_the_telemetry_client_unmodified() {
    for enabled in [true, false] {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
        fs::create_dir_all(&vibe_home).expect("user config directory");
        fs::write(
            working_directory.join(".vibe/config.toml"),
            format!("enable_telemetry = {enabled}\n"),
        )
        .expect("telemetry configuration");
        let workspace_service = WorkspaceService::new(
            crate::workspace::WorkspacePaths {
                vibe_home,
                working_directory: working_directory.clone(),
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("workspace service");
        let telemetry = Arc::new(RecordingClientTelemetry::default());
        let server = AppServer::with_workspace_service(workspace_service)
            .using_client_telemetry(telemetry.clone());
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": working_directory}),
        ));

        connection.dispatch(&request(
            3,
            "telemetry/record",
            json!({
                "sessionId": "session-1",
                "name": "vibe.user_rating_feedback",
                "properties": {"rating": 4, "model": "medium", "note": "a path/like value"},
                "correlateLastRequest": true
            }),
        ));

        let events = telemetry.events.lock().expect("recorded events").clone();
        if !enabled {
            assert!(
                events.is_empty(),
                "enable_telemetry = false ships nothing: {events:?}"
            );
            continue;
        }
        let event = events.first().expect("one shipped event").clone();
        assert_eq!(event.name, "vibe.user_rating_feedback");
        assert_eq!(
            event.properties,
            json!({"rating": 4, "model": "medium", "note": "a path/like value"})
                .as_object()
                .expect("an object")
                .clone(),
            "a client's properties travel unmodified"
        );
        assert_eq!(event.session_id.as_deref(), Some("session-1"));
        assert!(
            event.correlate_last_request,
            "the correlation the client asked for is carried"
        );
    }
}

/// The reference model declares four fields, so a client that sends a fifth
/// is answered with the pointer to it rather than having it ignored.
#[test]
fn a_recorded_client_event_refuses_a_field_the_reference_does_not_declare() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "session-1", "workingDirectory": "/workspace"}),
    ));

    let refused = connection.dispatch(&request(
        3,
        "telemetry/record",
        json!({"sessionId": "session-1", "name": "probe", "surplus": true}),
    ));

    match decode_frame(&refused.outbound[0]).expect("a refusal") {
        Envelope::Error(error) => {
            assert_eq!(error.error.code, ProtocolErrorCode::InvalidParams);
            assert_eq!(error.error.data["errorCount"], json!(1));
            assert_eq!(error.error.data["issues"][0]["path"], json!(["surplus"]));
        }
        other => unreachable!("the surplus field was accepted: {other:?}"),
    }
}
