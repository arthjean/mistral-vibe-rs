//! What opening a session composes: the project MCP activation it schedules,
//! the tool filters it reads, and the history it hydrates.

use super::*;

#[test]
fn trusted_session_start_schedules_typed_project_mcp_activation() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let working_directory = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
    fs::create_dir_all(&vibe_home).expect("user config directory");
    fs::write(
        working_directory.join(".vibe/config.toml"),
        r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/fixture"
args = ["--stdio"]
startup_timeout_sec = 1
tool_timeout_sec = 2
"#,
    )
    .expect("project MCP config");
    let workspace = WorkspaceService::new(
        crate::workspace::WorkspacePaths {
            vibe_home,
            working_directory: working_directory.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("workspace service");
    let server = AppServer::with_workspace_service(workspace);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let trusted = connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "trusted",
            "workingDirectory": working_directory,
            "trustWorkspace": true
        }),
    ));
    assert!(matches!(
        trusted.deferred.as_slice(),
        [DeferredWork::ConfigureMcp {
            session_id,
            configs
        }] if session_id == "trusted"
            && configs.len() == 1
            && configs[0].alias == "fixture"
            && configs[0].startup_timeout_ms == 1_000
            && configs[0].tool_timeout_ms == 2_000
    ));

    let untrusted = connection.dispatch(&request(
        3,
        "session/start",
        json!({
            "sessionId": "untrusted",
            "workingDirectory": temporary.path().join("workspace"),
            "trustWorkspace": false
        }),
    ));
    assert!(untrusted.deferred.is_empty());
}

/// The configuration file is a shared surface with the reference, so the
/// two filter lists it carries reach the session the same way: the
/// allowlist stands when the client asks for none, the denylist
/// concatenates onto what the client sent, and an entry that does not
/// compile is reported rather than applied.
#[test]
fn session_start_reads_the_configured_tool_filters_and_reports_a_broken_entry() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let working_directory = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
    fs::create_dir_all(&vibe_home).expect("user config directory");
    fs::write(
        working_directory.join(".vibe/config.toml"),
        "enabled_tools = [\"read_file\", \"serena_*\"]\n\
             disabled_tools = [\"re:web_.*\", \"re:[\"]\n",
    )
    .expect("project tool filters");
    let workspace = WorkspaceService::new(
        crate::workspace::WorkspacePaths {
            vibe_home,
            working_directory: working_directory.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("workspace service");
    let server = AppServer::with_workspace_service(workspace);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "filtered",
            "workingDirectory": working_directory,
            "trustWorkspace": true,
            "disabledTools": ["exit_plan_mode"]
        }),
    ));

    let intent = server
        .sessions
        .lock()
        .expect("sessions")
        .get("filtered")
        .map(|session| session.intent.clone())
        .expect("the session started");
    assert_eq!(intent.enabled_tools, ["read_file", "serena_*"]);
    assert_eq!(
        intent.disabled_tools,
        ["exit_plan_mode", "re:[", "re:web_.*"]
    );

    let diagnostics = connection.dispatch(&request(
        3,
        "diagnostics/list",
        json!({"sessionId": "filtered"}),
    ));
    let reported = match decode_frame(&diagnostics.outbound[0]).expect("diagnostics response") {
        Envelope::Success(SuccessResponse { result, .. }) => result["issues"]
            .as_array()
            .map(|issues| {
                issues
                    .iter()
                    .filter_map(|issue| issue["message"].as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    assert!(
        reported
            .iter()
            .any(|message| message.contains("disabled_tools entry `re:[`")),
        "the broken entry must be named: {reported:?}"
    );

    // US-090: the same problem is on the runtime snapshot, named by the file
    // it came from, and the rest of the answer is still whole.
    let runtime = call_for(&mut connection, 4, "runtime/read", "filtered")["runtime"].clone();
    let issues = runtime["issues"].as_array().expect("issues is a list");
    assert!(
        issues.iter().any(|issue| {
            issue["file"] == json!(CONFIG_FILE_LABEL)
                && issue["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("disabled_tools entry `re:[`"))
        }),
        "the offending file must be named: {issues:?}"
    );
    assert!(issues.iter().all(|issue| {
        issue.as_object().is_some_and(|issue| {
            issue.len() == 2 && issue.contains_key("file") && issue.contains_key("message")
        })
    }));
    assert!(runtime["config"]["activeModel"].is_object());
}

/// The path the `vibe` binary and the ACP adapter actually take: they build
/// a [`crate::client::SessionOptions`] and never a raw params object, so a
/// run without `--enabled-tools` must leave the configured allowlist
/// standing, the way the reference passes `None` for an absent flag.
#[test]
fn a_client_that_asks_for_no_allowlist_keeps_the_configured_one() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let working_directory = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
    fs::create_dir_all(&vibe_home).expect("user config directory");
    fs::write(
        working_directory.join(".vibe/config.toml"),
        "enabled_tools = [\"read_file\", \"serena_*\"]\n",
    )
    .expect("project tool filters");
    let workspace = WorkspaceService::new(
        crate::workspace::WorkspacePaths {
            vibe_home,
            working_directory: working_directory.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("workspace service");
    let server = AppServer::with_workspace_service(workspace);
    let mut client =
        crate::client::InProcessClient::connect_with_server(server.clone()).expect("client");
    let session_id = client
        .start_session(&crate::client::SessionOptions {
            working_directory: working_directory.to_string_lossy().into_owned(),
            trusted: true,
            ..default_session_options()
        })
        .expect("the session starts");

    let intent = server
        .sessions
        .lock()
        .expect("sessions")
        .get(&session_id)
        .map(|session| session.intent.clone())
        .expect("the session started");
    assert_eq!(intent.enabled_tools, ["read_file", "serena_*"]);
}

/// The options a client sends when the operator passed no tool flag.
fn default_session_options() -> crate::client::SessionOptions {
    crate::client::SessionOptions {
        working_directory: String::new(),
        session_id: None,
        add_directories: Vec::new(),
        trusted: false,
        agent: None,
        tool_filters: Vec::new(),
        enabled_tools: Vec::new(),
        disabled_tools: Vec::new(),
        mcp_servers: Vec::new(),
        model: None,
        max_turns: None,
        max_tokens: None,
        max_price_micros: None,
        mode: None,
        thinking: false,
        reasoning_effort: None,
        auto_approve: false,
        resume: None,
        continue_session: false,
    }
}

/// A session attached from persisted state runs under the same
/// configuration a fresh one does, so the filter lists reach it there too
/// rather than only on the `session/start` path.
#[test]
fn an_attached_runtime_session_carries_the_configured_tool_filters() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let working_directory = temporary.path().join("workspace");
    let session_root = temporary.path().join("sessions");
    let vibe_home = temporary.path().join("home");
    fs::create_dir_all(&working_directory).expect("workspace");
    fs::create_dir_all(&vibe_home).expect("user config directory");
    // The user file, which applies whatever the workspace trust decision is.
    fs::write(
        vibe_home.join("config.toml"),
        "disabled_tools = [\"serena_*\"]\n",
    )
    .expect("user tool filters");
    vibe_core::storage::SessionStore::new(&session_root)
        .create("attached", &working_directory.to_string_lossy(), None, 1)
        .expect("persisted session");
    let workspace = WorkspaceService::new(
        crate::workspace::WorkspacePaths {
            vibe_home,
            working_directory: working_directory.clone(),
            session_root,
        },
        true,
    )
    .expect("workspace service");
    let server = AppServer::with_workspace_service(workspace);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    // Selecting an agent attaches the persisted session to this connection,
    // which is the path that rebuilds the intent from stored state.
    connection.dispatch(&request(
        2,
        "session/agent/update",
        json!({"sessionId": "attached", "name": "default"}),
    ));

    let intent = server
        .sessions
        .lock()
        .expect("sessions")
        .get("attached")
        .map(|session| session.intent.clone())
        .expect("the session attached");
    // The default agent adds its own entry, and the configured one survives
    // the agent overlay rather than being replaced by it.
    assert_eq!(intent.disabled_tools, ["exit_plan_mode", "serena_*"]);
}

/// Reference edge case: a tool whose prerequisite is missing is absent from
/// the surface rather than published and failed at call time, and the
/// session says which tool it withheld.
#[test]
fn a_tool_whose_prerequisite_is_missing_is_withheld_and_named() {
    let server =
        AppServer::default().using_session_tool_factory(Arc::new(UnavailablePrerequisiteTools));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let published = server
        .sessions
        .lock()
        .expect("sessions")
        .get("session-1")
        .map(|session| session.tools.clone())
        .expect("the session started")
        .available(&NameFilter::default(), &NameFilter::default())
        .expect("available")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert!(
        !published.contains(&"fixture_probe".to_owned()),
        "a tool with no prerequisite reached the surface: {published:?}"
    );

    let diagnostics = connection.dispatch(&request(
        3,
        "diagnostics/list",
        json!({"sessionId": "session-1"}),
    ));
    let reported = match decode_frame(&diagnostics.outbound[0]).expect("diagnostics response") {
        Envelope::Success(SuccessResponse { result, .. }) => result["issues"]
            .as_array()
            .map(|issues| {
                issues
                    .iter()
                    .filter_map(|issue| issue["message"].as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    assert!(
        reported
            .iter()
            .any(|message| message.contains("tool `fixture_probe` is withheld")),
        "the withheld tool must be named: {reported:?}"
    );
}

#[test]
fn session_start_hydrates_bounded_public_resume_history() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let working_directory = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    let session_root = temporary.path().join("sessions");
    fs::create_dir_all(&working_directory).expect("working directory");
    let store = vibe_core::storage::SessionStore::new(&session_root);
    let mut metadata = store
        .create(
            "durable-session",
            &working_directory.to_string_lossy(),
            None,
            10,
        )
        .expect("durable session");
    for (timestamp, message) in [
        (
            11,
            ModelMessage::System {
                content: "private system".to_owned(),
            },
        ),
        (12, ModelMessage::user("older question".to_owned())),
        (
            13,
            ModelMessage::Assistant {
                content: "older answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
        ),
        (14, ModelMessage::user("latest question".to_owned())),
        (
            15,
            ModelMessage::Assistant {
                content: "latest answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
        ),
    ] {
        store
            .append_message(&mut metadata, &message, timestamp)
            .expect("message persists");
    }
    let workspace = WorkspaceService::new(
        crate::workspace::WorkspacePaths {
            vibe_home,
            working_directory,
            session_root,
        },
        false,
    )
    .expect("workspace service");
    let server = AppServer::with_workspace_service(workspace);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let started = connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "durable-session",
            "resume": "durable-session",
            "historyLimit": 2
        }),
    ));
    let decoded = decode_frame(&started.outbound[0]).expect("start response");
    assert!(matches!(decoded, Envelope::Success(_)));
    let Envelope::Success(SuccessResponse { result, .. }) = decoded else {
        return;
    };
    let entries = result["state"]["history"]["entries"]
        .as_array()
        .expect("public history");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["content"][0]["text"], "latest question");
    assert_eq!(entries[1]["content"][0]["text"], "latest answer");
    assert!(
        entries
            .iter()
            .all(|entry| entry["sessionId"] == "durable-session")
    );
    let session = server.session("durable-session").expect("runtime session");
    assert_eq!(session.intent.resume.as_deref(), Some("durable-session"));
    assert!(!session.intent.continue_session);
    assert_eq!(
        session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.history.len()),
        Some(2)
    );
}
