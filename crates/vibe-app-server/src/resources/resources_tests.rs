use super::*;

struct FakeMcpAuth {
    url: String,
    calls: StdMutex<Vec<String>>,
}

struct FakeConnectorTransport;

struct CountingEmptyConnectorCatalog {
    calls: std::sync::atomic::AtomicUsize,
}

impl ConnectorCatalogBackend for CountingEmptyConnectorCatalog {
    fn catalog<'a>(&'a self) -> ResourceFuture<'a, ConnectorCatalog> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(ConnectorCatalog {
                definitions: Vec::new(),
                connected: BTreeSet::new(),
            })
        })
    }
}

impl ConnectorBackend for FakeConnectorTransport {
    fn call<'a>(
        &'a self,
        _connector_id: &'a str,
        _tool: &'a str,
        _arguments: Value,
        _max_response_bytes: usize,
    ) -> vibe_core::integrations::ConnectorFuture<'a> {
        Box::pin(async { Ok(vibe_core::tools::ToolExecutionOutput::text("{}")) })
    }
}

struct FakeConnectorAuth {
    url: String,
    connected: bool,
    calls: StdMutex<Vec<String>>,
}

impl ConnectorAuthBackend for FakeConnectorAuth {
    fn auth_url<'a>(
        &'a self,
        session_id: &'a str,
        connector_id: &'a str,
    ) -> ResourceFuture<'a, Option<String>> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                .push(format!("auth:{session_id}:{connector_id}"));
            Ok(Some(self.url.clone()))
        })
    }

    fn refresh<'a>(
        &'a self,
        session_id: &'a str,
        connector_id: &'a str,
    ) -> ResourceFuture<'a, bool> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                .push(format!("refresh:{session_id}:{connector_id}"));
            Ok(self.connected)
        })
    }
}

impl McpAuthBackend for FakeMcpAuth {
    fn login<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, String> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                .push(format!("login:{session_id}:{}", config.alias));
            Ok(self.url.clone())
        })
    }

    fn complete<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, bool> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                .push(format!("complete:{session_id}:{}", config.alias));
            Ok(true)
        })
    }

    fn logout<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                .push(format!("logout:{session_id}:{}", config.alias));
            Ok(())
        })
    }

    fn close_session<'a>(&'a self, session_id: &'a str) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                .push(format!("close:{session_id}"));
            Ok(())
        })
    }
}

fn params(value: Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .expect("object")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn backend_request(
    session_id: &str,
    method: &str,
    params: BTreeMap<String, Value>,
) -> ResourceBackendRequest {
    ResourceBackendRequest::parse(session_id.to_owned(), method, &params, false)
        .expect("valid backend request")
}

fn disabled_mcp(alias: &str) -> McpServerConfig {
    McpServerConfig {
        alias: alias.to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: Url::parse("https://mcp.example/rpc").expect("MCP URL"),
            headers: BTreeMap::new(),
        },
        enabled: false,
        disabled_tools: Default::default(),
        startup_timeout_ms: vibe_core::mcp::DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        tool_timeout_ms: vibe_core::mcp::DEFAULT_MCP_TOOL_TIMEOUT_MS,
        auth: Default::default(),
        prompt: None,
        sampling_enabled: true,
    }
}

fn oauth_connector() -> ConnectorDefinition {
    ConnectorDefinition {
        id: "drive-id".to_owned(),
        name: "Drive".to_owned(),
        base_url: Url::parse("https://connectors.example/drive").expect("connector URL"),
        auth_kind: vibe_core::integrations::ConnectorAuthKind::OAuth,
        tools: vec![vibe_core::integrations::ConnectorTool {
            name: "search".to_owned(),
            description: "Search files".to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
        }],
    }
}

#[test]
fn trust_mutation_returns_a_canonical_notification_after_the_response() {
    let mut resources = ResourceService::default();
    let workspace = tempfile::tempdir().expect("workspace");
    let policy = PermissionStore::default();
    resources
        .open_session("s1", policy.clone(), ToolRegistry::default())
        .expect("session");
    let dispatch = resources
        .dispatch(
            "workspace/trust/decision",
            &params(json!({
                "sessionId": "s1",
                "cwd": workspace.path(),
                "decision": "trust_cwd"
            })),
            false,
        )
        .expect("trust decision");
    // The decision moved runtime state, which the server publishes as
    // `runtime/updated` rather than a name only this port ever spoke.
    assert!(dispatch.signals.runtime_updated);
    assert!(dispatch.signals.warnings.is_empty());
    assert!(dispatch.result.is_empty());
    assert_eq!(
        policy
            .try_trust_decision(workspace.path())
            .expect("canonical policy"),
        Some(TrustDecision::SessionTrusted)
    );
}

/// The stub this service used to answer the review surface from is gone,
/// and with it the only reason the service knew what a review was. The
/// methods are routed at the server against the session's engine now, so
/// this service has to refuse them rather than answer an empty panel.
#[test]
fn the_resource_service_no_longer_answers_the_review_surface() {
    let mut resources = ResourceService::default();
    for method in [
        "review/approve",
        "review/baseline",
        "review/hunks",
        "review/revert",
        "review/state",
        "review/turnDiff",
    ] {
        let error = resources
            .dispatch(method, &params(json!({"sessionId": "s1"})), false)
            .expect_err("the service holds no review state");
        assert!(
            matches!(error, ResourceError::MethodNotFound(_)),
            "{method} must not be answered here: {error}"
        );
    }
}

/// A service whose log file is its own, so the test reads what it wrote
/// rather than the operator's home.
fn logging_service() -> (tempfile::TempDir, ResourceService) {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let service = ResourceService::default().logging_to(FileLog::in_home(
        enclosure.path(),
        vibe_core::observability::LogSettings::default(),
    ));
    (enclosure, service)
}

fn read_logs(service: &mut ResourceService, limit: u64, offset: u64) -> Value {
    service
        .dispatch(
            "diagnostics/logs/read",
            &params(json!({"limit": limit, "offset": offset})),
            false,
        )
        .expect("logs")
        .result["logs"]
        .clone()
}

#[test]
fn diagnostics_and_logs_redact_sensitive_text() {
    let (_enclosure, mut resources) = logging_service();
    resources.record_diagnostic("config.toml", "Authorization: Bearer secret");
    resources.record_log(LogLevel::Error, "token=secret");
    let diagnostics = resources
        .dispatch("diagnostics/list", &BTreeMap::new(), false)
        .expect("diagnostics");
    let logs = read_logs(&mut resources, 10, 0);
    assert_eq!(
        diagnostics.result["issues"][0]["message"],
        "[redacted sensitive error]"
    );
    assert_eq!(logs["entries"][0]["message"], "[redacted sensitive error]");
}

/// US-018: the page comes from the file, so it carries the identifiers of
/// the process that wrote the line rather than the zeros a memory buffer
/// had nothing better to publish.
#[test]
fn a_page_carries_the_parsed_line_and_the_writing_process() {
    let (enclosure, mut resources) = logging_service();
    resources.record_log(LogLevel::Error, "the turn failed");
    // A line another process wrote is the same line to the reader.
    let path = enclosure.path().join("logs").join("vibe.log");
    let elsewhere = vibe_core::observability::format_log_line(
        vibe_core::auth::UtcTimestamp::now(),
        4_242,
        4_243,
        LogLevel::Warning,
        "another process was here",
        None,
    );
    let existing = std::fs::read_to_string(&path).expect("the log file");
    std::fs::write(&path, format!("{existing}{elsewhere}\n")).expect("the appended line");

    let logs = read_logs(&mut resources, 10, 0);
    let newest = &logs["entries"][0];
    assert_eq!(newest["message"], "another process was here");
    assert_eq!(newest["ppid"], 4_242);
    assert_eq!(newest["pid"], 4_243);
    assert_eq!(newest["level"], "WARNING");
    assert!(
        newest["rawLine"]
            .as_str()
            .is_some_and(|line| line.ends_with("another process was here")),
        "the raw line is published whole: {newest}"
    );
    assert!(
        newest["timestamp"]
            .as_str()
            .is_some_and(|timestamp| timestamp.contains('T')),
        "the timestamp is the stamp the line carried: {newest}"
    );
    assert!(
        newest["id"].as_str().is_some_and(|id| id.len() == 64),
        "the identity is a digest of the raw line: {newest}"
    );
    let (ppid, pid) = vibe_core::observability::process_identifiers();
    assert_eq!(logs["entries"][1]["pid"], pid);
    assert_eq!(logs["entries"][1]["ppid"], ppid);
    assert_eq!(logs["hasMore"], json!(false));
    assert_eq!(logs["cursor"], Value::Null);
}

/// US-018: a page that filled its limit says where the next one starts, and
/// the one that did not says nothing.
#[test]
fn a_filled_page_reports_where_the_next_one_starts() {
    let (_enclosure, mut resources) = logging_service();
    for index in 0..4 {
        resources.record_log(LogLevel::Error, &format!("record {index}"));
    }
    let first = read_logs(&mut resources, 2, 0);
    assert_eq!(first["entries"][0]["message"], "record 3");
    assert_eq!(first["hasMore"], json!(true));
    assert_eq!(first["cursor"], json!(2));
    let last = read_logs(&mut resources, 10, 2);
    assert_eq!(
        last["entries"]
            .as_array()
            .map(|entries| entries.len())
            .unwrap_or_default(),
        2
    );
    assert_eq!(last["hasMore"], json!(false));
    assert_eq!(last["cursor"], Value::Null);
}

/// US-018: no file at all is an empty page rather than an error, and a
/// limit outside the published range is refused before anything is read.
#[test]
fn an_absent_file_is_empty_and_an_impossible_page_is_refused() {
    let (_enclosure, mut resources) = logging_service();
    let logs = read_logs(&mut resources, 10, 0);
    assert_eq!(logs["entries"], json!([]));
    assert_eq!(logs["hasMore"], json!(false));
    assert_eq!(logs["cursor"], Value::Null);
    for page in [json!({"limit": 0}), json!({"limit": 501})] {
        let error = resources
            .dispatch("diagnostics/logs/read", &params(page.clone()), false)
            .expect_err("the page is refused");
        assert!(
            matches!(error, ResourceError::InvalidParams(_)),
            "{page} must be refused as invalid params: {error}"
        );
    }
}

/// US-018: a hand-edited line does not fail the request, and it does not
/// disappear from the numbering either.
#[test]
fn a_line_the_pattern_refuses_is_skipped_rather_than_failing_the_page() {
    let (enclosure, mut resources) = logging_service();
    resources.record_log(LogLevel::Error, "the turn failed");
    let path = enclosure.path().join("logs").join("vibe.log");
    let existing = std::fs::read_to_string(&path).expect("the log file");
    std::fs::write(&path, format!("{existing}an operator pasted this\n")).expect("the pasted line");
    let logs = read_logs(&mut resources, 10, 0);
    assert_eq!(
        logs["entries"]
            .as_array()
            .map(|entries| entries.len())
            .unwrap_or_default(),
        1,
        "only the line that parses is published: {logs}"
    );
    assert_eq!(logs["entries"][0]["message"], "the turn failed");
}

/// US-107: a permanent approval the configuration file refused is kept for
/// the session rather than failing the call, so the reason has to reach the
/// operator. `diagnostics/list` and the runtime snapshot are where they
/// read one, and the session that could not write is the session that
/// reports it.
#[tokio::test]
async fn a_permanent_approval_that_could_not_be_written_is_reported() {
    let mut resources = ResourceService::default();
    let store = PermissionStore::default().with_allowlist_persistence(Arc::new(
        |_tool: &str, _patterns: &[String]| Err("the configuration file is read-only".to_owned()),
    ));
    resources
        .open_session("session-1", store.clone(), ToolRegistry::default())
        .expect("the session opens");

    store
        .authorize(
            "bash",
            json!({"command": "cargo test"}),
            vibe_core::policy::PermissionContext::asking(vec![
                vibe_core::policy::PermissionRequirement::command("cargo test"),
            ]),
            &PermanentApproval,
        )
        .await
        .expect("the call the operator approved still runs");

    let reported = |dispatch: &ResourceDispatch| {
        dispatch.result["issues"]
            .as_array()
            .expect("issues is a list")
            .iter()
            .filter_map(|issue| issue["message"].as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
    };
    let diagnostics = resources
        .dispatch("diagnostics/list", &BTreeMap::new(), true)
        .expect("diagnostics");
    let listed = reported(&diagnostics);
    assert!(
        listed
            .iter()
            .any(|message| message.contains("bash") && message.contains("read-only")),
        "{listed:?}"
    );
    assert_eq!(
        diagnostics.result["issues"][0]["file"],
        json!(crate::server::CONFIG_FILE_LABEL)
    );

    let runtime = resources.runtime("session-1").expect("runtime");
    let issues = runtime["issues"].as_array().expect("issues is a list");
    assert!(
        issues.iter().any(|issue| {
            issue["message"]
                .as_str()
                .is_some_and(|message| message.contains("read-only"))
        }),
        "the runtime snapshot names it too: {issues:?}"
    );

    // Another session's failure is not this session's problem.
    resources
        .open_session(
            "session-2",
            PermissionStore::default(),
            ToolRegistry::default(),
        )
        .expect("the second session opens");
    let other = resources.runtime("session-2").expect("runtime");
    assert!(
        other["issues"]
            .as_array()
            .expect("issues is a list")
            .is_empty(),
        "{:?}",
        other["issues"]
    );
}

struct PermanentApproval;

impl vibe_core::policy::ApprovalAgent for PermanentApproval {
    fn request<'a>(
        &'a self,
        _request: vibe_core::policy::ApprovalRequest,
    ) -> vibe_core::policy::ApprovalFuture<'a> {
        Box::pin(async move { Ok(vibe_core::policy::ApprovalDecision::ApprovePermanently) })
    }
}

#[test]
fn session_scoped_resource_state_is_bounded_and_released() {
    let mut resources = ResourceService::default();
    for index in 0..MAX_RESOURCE_SESSIONS {
        resources
            .open_session(
                &format!("session-{index}"),
                PermissionStore::default(),
                ToolRegistry::default(),
            )
            .expect("within capacity");
    }
    assert!(matches!(
        resources.open_session(
            "overflow",
            PermissionStore::default(),
            ToolRegistry::default()
        ),
        Err(ResourceError::Conflict(_))
    ));

    resources.close_session("session-0");

    resources
        .open_session(
            "replacement",
            PermissionStore::default(),
            ToolRegistry::default(),
        )
        .expect("released capacity");
}

#[test]
fn mcp_rejects_non_https_endpoints() {
    let mut resources = ResourceService::default();
    let error = resources
        .dispatch(
            "mcp/add",
            &params(json!({"url": "http://mcp.example"})),
            false,
        )
        .expect_err("insecure URL");
    assert!(matches!(error, ResourceError::InvalidParams(_)));
}

#[tokio::test]
async fn mcp_oauth_routes_the_exact_source_and_rejects_unsafe_urls() {
    let auth = Arc::new(FakeMcpAuth {
        url: "https://auth.example/authorize?state=opaque".to_owned(),
        calls: StdMutex::new(Vec::new()),
    });
    let backend = CoreResourceBackend::default().with_mcp_auth(auth.clone());
    backend
        .open_session(ResourceSession {
            session_id: "s1".to_owned(),
            generation: 1,
            working_directory: "/workspace".to_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("open session");
    backend
        .configure_mcp("s1", vec![disabled_mcp("source-a")])
        .await
        .expect("configure source");
    let login = backend
        .dispatch(backend_request(
            "s1",
            "mcp/login",
            params(json!({"name": "source-a"})),
        ))
        .await
        .expect("OAuth URL");
    // The URL is not on the answer: it crosses as `mcp/authUrl`, which is
    // where a reference client reads it, and the answer declares only the
    // runtime the server fills in.
    assert!(!login.result.contains_key("auth"));
    assert_eq!(
        login.signals.auth_url,
        Some(McpAuthUrl {
            name: "source-a".to_owned(),
            url: "https://auth.example/authorize?state=opaque".to_owned(),
        })
    );
    let completion = backend
        .dispatch(backend_request(
            "s1",
            "mcp/auth/complete",
            params(json!({"name": "source-a"})),
        ))
        .await
        .expect("OAuth completion is checked");
    assert_eq!(completion.result["auth"]["verified"], true);
    backend
        .dispatch(backend_request(
            "s1",
            "mcp/logout",
            params(json!({"name": "source-a"})),
        ))
        .await
        .expect("logout");
    assert_eq!(
        auth.calls.lock().expect("calls").as_slice(),
        [
            "login:s1:source-a",
            "complete:s1:source-a",
            "logout:s1:source-a"
        ]
    );
    assert!(matches!(
        backend
            .dispatch(backend_request(
                "s1",
                "mcp/login",
                params(json!({"name": "unknown"})),
            ))
            .await,
        Err(ResourceError::NotFound(_))
    ));
    backend
        .close_session("s1", 1)
        .await
        .expect("OAuth session cleanup");
    assert_eq!(
        auth.calls.lock().expect("calls").last().map(String::as_str),
        Some("close:s1")
    );

    let unsafe_backend = CoreResourceBackend::default().with_mcp_auth(Arc::new(FakeMcpAuth {
        url: "http://auth.example/authorize".to_owned(),
        calls: StdMutex::new(Vec::new()),
    }));
    unsafe_backend
        .open_session(ResourceSession {
            session_id: "s2".to_owned(),
            generation: 1,
            working_directory: "/workspace".to_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("open unsafe session");
    unsafe_backend
        .configure_mcp("s2", vec![disabled_mcp("source-b")])
        .await
        .expect("configure unsafe source");
    assert!(matches!(
        unsafe_backend
            .dispatch(backend_request(
                "s2",
                "mcp/login",
                params(json!({"name": "source-b"})),
            ))
            .await,
        Err(ResourceError::Unavailable(_))
    ));
}

#[tokio::test]
async fn connectors_initialize_lazily_and_route_auth_to_the_exact_source() {
    let auth = Arc::new(FakeConnectorAuth {
        url: "https://connectors.example/authorize?state=opaque".to_owned(),
        connected: true,
        calls: StdMutex::new(Vec::new()),
    });
    let backend = CoreResourceBackend::default()
        .with_connectors(
            vec![oauth_connector()],
            Arc::new(FakeConnectorTransport),
            "credential",
            Url::parse("https://connectors.example").expect("catalog URL"),
        )
        .with_connector_auth(auth.clone());
    backend
        .open_session(ResourceSession {
            session_id: "s1".to_owned(),
            generation: 1,
            working_directory: "/workspace".to_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("open session");

    let listed = backend
        .dispatch(backend_request("s1", "connectors/read", BTreeMap::new()))
        .await
        .expect("connectors initialize");
    // The sources are published in the MCP state; this answer is the counts
    // and nothing else.
    assert_eq!(
        listed.result.keys().map(String::as_str).collect::<Vec<_>>(),
        ["counts"]
    );
    assert_eq!(listed.result["counts"]["total"], json!(1));
    let login = backend
        .dispatch(backend_request(
            "s1",
            "connectors/auth/read",
            params(json!({"name": "drive"})),
        ))
        .await
        .expect("connector auth URL");
    assert_eq!(
        login.result["url"],
        "https://connectors.example/authorize?state=opaque"
    );
    assert_eq!(
        auth.calls.lock().expect("calls").as_slice(),
        ["auth:s1:drive-id"]
    );
    let refreshed = backend
        .dispatch(backend_request(
            "s1",
            "connectors/refresh",
            params(json!({"name": "drive"})),
        ))
        .await
        .expect("connector refresh");
    assert_eq!(
        refreshed
            .result
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["toolCount"],
        "the runtime is filled in by the server, which owns it"
    );
    assert!(refreshed.signals.runtime_updated);
    let connectors = backend
        .dispatch(backend_request("s1", "mcp/read", BTreeMap::new()))
        .await
        .expect("the merged source list");
    assert_eq!(
        connectors.result["mcp"]["sources"][0],
        json!({
            "name": "Drive",
            "kind": "connector",
            "transport": "connector",
            "status": "connected",
            "tools": [{
                "name": "connector_Drive_search",
                "description": "Search files",
                "enabled": true,
            }],
        })
    );
    assert_eq!(
        auth.calls.lock().expect("calls").as_slice(),
        ["auth:s1:drive-id", "refresh:s1:drive-id"]
    );
}

#[tokio::test]
async fn connector_persistence_failure_leaves_runtime_state_unchanged() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let vibe_home = temporary.path().join("home/.vibe");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&vibe_home).expect("config directory");
    std::fs::create_dir_all(&workspace).expect("workspace directory");
    let config_path = vibe_home.join("config.toml");
    let store = LayeredConfig::new(
        vibe_core::config::ConfigPaths {
            vibe_home,
            working_directory: workspace.clone(),
        },
        vibe_core::config::registry::default_document(),
    );
    let backend = CoreResourceBackend::default()
        .with_config(store)
        .with_connectors(
            vec![oauth_connector()],
            Arc::new(FakeConnectorTransport),
            "credential",
            Url::parse("https://connectors.example").expect("catalog URL"),
        );
    backend
        .open_session(ResourceSession {
            session_id: "transaction".to_owned(),
            generation: 1,
            working_directory: workspace.to_string_lossy().into_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("session opens");
    backend
        .dispatch(backend_request(
            "transaction",
            "connectors/read",
            BTreeMap::new(),
        ))
        .await
        .expect("connector initializes");
    std::fs::write(&config_path, "invalid = [").expect("corrupt config fixture");

    assert!(
        backend
            .dispatch(backend_request(
                "transaction",
                "connectors/toggle",
                params(json!({"name": "drive", "disabled": true})),
            ))
            .await
            .is_err()
    );
    let session = backend.session("transaction").expect("session remains");
    let view = session
        .connectors
        .views()
        .expect("connector state")
        .remove(0);
    assert!(view.enabled);
}

/// Connector aliases used to be lowercased and are now published in the
/// case the reference keeps, so a preference persisted by an older build
/// names `drive` where the session now holds `Drive`. Resolving that entry
/// is what stops an upgrade from silently re-enabling a connector the
/// operator disabled.
#[tokio::test]
async fn a_preference_persisted_under_the_lowercased_alias_still_applies() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let vibe_home = temporary.path().join("home/.vibe");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&vibe_home).expect("config directory");
    std::fs::create_dir_all(&workspace).expect("workspace directory");
    std::fs::write(
        vibe_home.join("config.toml"),
        "[[connectors]]\nname = \"drive\"\ndisabled = true\ndisabled_tools = [\"search\"]\n",
    )
    .expect("preference written by an older build");
    let store = LayeredConfig::new(
        vibe_core::config::ConfigPaths {
            vibe_home,
            working_directory: workspace.clone(),
        },
        vibe_core::config::registry::default_document(),
    );
    let backend = CoreResourceBackend::default()
        .with_config(store)
        .with_connectors(
            vec![oauth_connector()],
            Arc::new(FakeConnectorTransport),
            "credential",
            Url::parse("https://connectors.example").expect("catalog URL"),
        );
    backend
        .open_session(ResourceSession {
            session_id: "upgraded".to_owned(),
            generation: 1,
            working_directory: workspace.to_string_lossy().into_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("session opens");

    backend
        .dispatch(backend_request(
            "upgraded",
            "connectors/read",
            BTreeMap::new(),
        ))
        .await
        .expect("connector initializes");

    let view = backend
        .session("upgraded")
        .expect("session")
        .connectors
        .views()
        .expect("connector state")
        .remove(0);
    assert_eq!(view.alias, "Drive");
    assert!(
        !view.enabled,
        "the persisted disable must survive the alias case change"
    );
    assert!(view.disabled_tools.contains("connector_Drive_search"));
}

#[tokio::test]
async fn core_backend_denies_stdio_mcp_before_workspace_trust() {
    let workspace = tempfile::tempdir().expect("workspace");
    let backend = CoreResourceBackend::default();
    backend
        .open_session(ResourceSession {
            session_id: "s1".to_owned(),
            generation: 1,
            working_directory: workspace.path().to_string_lossy().into_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("open session");
    let error = backend
        .dispatch(backend_request(
            "s1",
            "mcp/add",
            params(json!({
                "name": "untrusted",
                "transport": "stdio",
                "command": "must-not-launch"
            })),
        ))
        .await
        .expect_err("untrusted executable must be denied before spawn");
    assert!(
        matches!(error, ResourceError::Unavailable(message) if message.contains("workspace trust"))
    );
}

#[tokio::test]
async fn core_backend_denies_stdio_mcp_working_directory_outside_trust() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside directory");
    let policy = PermissionStore::default();
    policy
        .try_set_trust(
            workspace.path(),
            TrustDecision::SessionTrusted,
            TrustRootKind::Workspace,
        )
        .expect("trust workspace");
    let backend = CoreResourceBackend::default();
    backend
        .open_session(ResourceSession {
            session_id: "s1".to_owned(),
            generation: 1,
            working_directory: workspace.path().to_string_lossy().into_owned(),
            project_trusted: true,
            policy,
            tools: ToolRegistry::default(),
        })
        .expect("open session");
    let error = backend
        .dispatch(backend_request(
            "s1",
            "mcp/add",
            params(json!({
                "name": "outside",
                "transport": "stdio",
                "command": "must-not-launch",
                "workingDirectory": outside.path()
            })),
        ))
        .await
        .expect_err("outside working directory must be denied before spawn");
    assert!(
        matches!(error, ResourceError::Unavailable(message) if message.contains("workspace trust"))
    );
}

#[tokio::test]
async fn core_backend_runs_trusted_shell_and_cleans_the_owned_process() {
    let workspace = tempfile::tempdir().expect("workspace");
    let policy = PermissionStore::default();
    policy
        .try_set_trust(
            workspace.path(),
            TrustDecision::SessionTrusted,
            TrustRootKind::Workspace,
        )
        .expect("trust");
    let backend = CoreResourceBackend::default();
    backend
        .open_session(ResourceSession {
            session_id: "s1".to_owned(),
            generation: 1,
            working_directory: workspace.path().to_string_lossy().into_owned(),
            project_trusted: true,
            policy,
            tools: ToolRegistry::default(),
        })
        .expect("open session");
    let dispatch = backend
        .dispatch(backend_request(
            "s1",
            "shell/run",
            params(json!({
                "sessionId": "s1",
                "operationId": "shell-1",
                "command": "pwd"
            })),
        ))
        .await
        .expect("run shell");
    assert!(dispatch.signals.runtime_updated);
    let (completed, saw_output) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut saw_output = false;
        loop {
            let dispatch = backend
                .dispatch(backend_request(
                    "s1",
                    "shell/run",
                    params(json!({
                        "sessionId": "s1",
                        "operationId": "shell-1",
                        "command": "pwd"
                    })),
                ))
                .await
                .expect("poll shell");
            saw_output |= dispatch
                .result
                .get("shell")
                .and_then(|shell| shell.pointer("/output/chunks"))
                .and_then(Value::as_array)
                .is_some_and(|chunks| !chunks.is_empty());
            if dispatch
                .result
                .get("shell")
                .and_then(|shell| shell.pointer("/output/state/status"))
                .and_then(Value::as_str)
                != Some("running")
            {
                break (dispatch, saw_output);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shell completes within the test deadline");
    assert_eq!(
        completed
            .result
            .get("shell")
            .and_then(|shell| shell.pointer("/output/state/status"))
            .and_then(Value::as_str),
        Some("exited")
    );
    assert!(
        saw_output,
        "shell output must be drained before process release"
    );
    let denied = backend
        .dispatch(backend_request(
            "s1",
            "shell/run",
            params(json!({
                "sessionId": "s1",
                "operationId": "shell-2",
                "command": "rm forbidden"
            })),
        ))
        .await
        .expect_err("destructive shell command must be denied before spawn");
    assert!(matches!(denied, ResourceError::Conflict(_)));
    backend.close_session("s1", 1).await.expect("cleanup");
}

#[tokio::test]
async fn stale_close_cannot_remove_a_reattached_resource_generation() {
    let backend = CoreResourceBackend::default();
    let session = |generation| ResourceSession {
        session_id: "s1".to_owned(),
        generation,
        working_directory: "/workspace".to_owned(),
        project_trusted: false,
        policy: PermissionStore::default(),
        tools: ToolRegistry::default(),
    };
    backend.open_session(session(1)).expect("first attachment");
    backend.open_session(session(2)).expect("reattachment");

    backend
        .close_session("s1", 1)
        .await
        .expect("stale cleanup is harmless");
    let dispatch = backend
        .dispatch(backend_request("s1", "mcp/read", BTreeMap::new()))
        .await
        .expect("reattached resources remain available");
    assert!(dispatch.result.contains_key("mcp"));

    backend
        .close_session("s1", 2)
        .await
        .expect("current cleanup");
    assert!(matches!(
        backend
            .dispatch(backend_request("s1", "mcp/read", BTreeMap::new()))
            .await,
        Err(ResourceError::NotFound(_))
    ));
}

#[tokio::test]
async fn empty_connector_catalog_initializes_once_under_concurrent_reads() {
    let catalog = Arc::new(CountingEmptyConnectorCatalog {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let backend = CoreResourceBackend::default().with_connector_catalog(
        catalog.clone(),
        Arc::new(FakeConnectorTransport),
        "credential",
        Url::parse("https://connectors.example.test").expect("connector URL"),
    );
    backend
        .open_session(ResourceSession {
            session_id: "connector-read".to_owned(),
            generation: 1,
            working_directory: "/workspace".to_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        })
        .expect("open connector session");

    let first = backend.dispatch(backend_request(
        "connector-read",
        "connectors/read",
        BTreeMap::new(),
    ));
    let second = backend.dispatch(backend_request(
        "connector-read",
        "connectors/read",
        BTreeMap::new(),
    ));
    let (first, second) = tokio::join!(first, second);
    first.expect("first connector read");
    second.expect("second connector read");
    assert_eq!(catalog.calls.load(Ordering::Acquire), 1);
}
