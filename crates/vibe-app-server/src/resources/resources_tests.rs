use super::*;
use vibe_core::policy::TrustRootKind;

/// Reads a session's MCP state through the catalog, as `mcp_catalog/read`.
async fn read_mcp(
    backend: &CoreResourceBackend,
    session_id: &str,
) -> Result<McpCatalogOutcome, McpCatalogError> {
    backend
        .mcp_catalog(
            McpCatalogCall::Read {
                session_id: session_id.to_owned(),
            },
            McpCatalogTarget::Session(session_id.to_owned()),
            Arc::new(|_, _| {}),
        )
        .await
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
        Box::pin(async move {
            Ok(vibe_core::policy::ApprovalDecision::ApprovePermanently(
                None,
            ))
        })
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
    let dispatch = read_mcp(&backend, "s1")
        .await
        .expect("reattached resources remain available");
    assert!(dispatch.result.contains_key("mcp"));

    backend
        .close_session("s1", 2)
        .await
        .expect("current cleanup");
    assert!(matches!(
        read_mcp(&backend, "s1").await,
        Err(McpCatalogError::Refused {
            code: vibe_protocol::ProtocolErrorCode::NotFound,
            ..
        })
    ));
}
