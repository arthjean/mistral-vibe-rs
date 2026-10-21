use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use super::*;
use crate::sync::lock;

/// A client that answers everything, recording what it was asked.
#[derive(Default)]
struct FakeClientPort {
    requests: StdMutex<Vec<ClientToolRequest>>,
    capabilities: StdMutex<BTreeSet<ClientToolCapability>>,
    fail_wait: AtomicBool,
}

impl FakeClientPort {
    fn hosting(capabilities: impl IntoIterator<Item = ClientToolCapability>) -> Arc<Self> {
        let port = Self::default();
        *port.capabilities.lock().expect("capabilities") = capabilities.into_iter().collect();
        Arc::new(port)
    }

    fn methods(&self) -> Vec<&'static str> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .map(ClientToolRequest::method)
            .collect()
    }
}

impl ClientToolPort for FakeClientPort {
    fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a> {
        Box::pin(async move {
            lock(&self.requests).push(request.clone());
            match request {
                ClientToolRequest::ReadTextFile { .. } => {
                    Ok(json!({"content": "from the buffer\n"}))
                }
                ClientToolRequest::TerminalCreate { .. } => {
                    Ok(json!({"terminalId": "client-terminal-1"}))
                }
                ClientToolRequest::TerminalWait { .. }
                    if self.fail_wait.load(Ordering::Acquire) =>
                {
                    Err(ToolIoError::Request(
                        "the client failed the wait".to_owned(),
                    ))
                }
                ClientToolRequest::TerminalWait { .. } => Ok(json!({"exitCode": 0})),
                ClientToolRequest::TerminalOutput { .. } => {
                    Ok(json!({"output": "ok", "truncated": false}))
                }
                _ => Ok(json!({})),
            }
        })
    }

    fn supports(&self, capability: ClientToolCapability) -> bool {
        self.capabilities
            .lock()
            .expect("capabilities")
            .contains(&capability)
    }
}

#[test]
fn client_tool_requests_serialize_as_the_reference_methods() {
    let read = ClientToolRequest::ReadTextFile {
        session_id: "session-1".to_owned(),
        path: "/work/main.rs".to_owned(),
        line: Some(4),
        limit: Some(10),
    };
    assert_eq!(
        serde_json::to_value(&read).expect("read serializes"),
        json!({
            "method": "clientTool/readTextFile",
            "params": {
                "sessionId": "session-1",
                "path": "/work/main.rs",
                "line": 4,
                "limit": 10,
            },
        })
    );
    let create = ClientToolRequest::TerminalCreate {
        session_id: "session-1".to_owned(),
        command: "cargo test".to_owned(),
        args: None,
        env: None,
        cwd: "/work".to_owned(),
        output_byte_limit: 1024,
        tool_call_id: Some("call-1".to_owned()),
    };
    assert_eq!(create.method(), "clientTool/terminal/create");
    assert_eq!(
        serde_json::to_value(&create).expect("create serializes")["params"]["outputByteLimit"],
        json!(1024)
    );
}

#[tokio::test]
async fn a_capability_the_client_did_not_declare_is_never_delegated() {
    let port = FakeClientPort::hosting([ClientToolCapability::Terminal]);
    let io = ClientToolIo::new("session-1", port.clone());
    assert!(io.supports_terminal());
    assert!(!io.supports_read());
    assert!(!io.supports_write());
    assert!(matches!(
        io.read_text_file("secret", None, None).await,
        Err(ToolIoError::CapabilityNotAdvertised(
            ClientToolCapability::FilesystemRead
        ))
    ));
    assert!(
        port.requests.lock().expect("requests").is_empty(),
        "a refused capability still reached the client"
    );
}

#[tokio::test]
async fn a_delegated_read_carries_the_reference_parameters() {
    let port = FakeClientPort::hosting([ClientToolCapability::FilesystemRead]);
    let io = ClientToolIo::new("session-1", port.clone());
    assert_eq!(
        io.read_text_file("main.rs", Some(1), Some(51))
            .await
            .expect("the client answers"),
        "from the buffer\n"
    );
    let requests = port.requests.lock().expect("requests");
    // The first line is the default, so the reference leaves `line` unset
    // rather than sending the offset it would have meant anyway.
    assert!(matches!(
        &requests[0],
        ClientToolRequest::ReadTextFile { session_id, path, line, limit }
            if session_id == "session-1" && path == "main.rs" && line.is_none() && *limit == Some(51)
    ));
}

#[tokio::test]
async fn a_read_below_the_reference_bounds_is_refused_before_it_is_sent() {
    let port = FakeClientPort::hosting([ClientToolCapability::FilesystemRead]);
    let io = ClientToolIo::new("session-1", port.clone());
    assert!(matches!(
        io.read_text_file("main.rs", Some(2), Some(0)).await,
        Err(ToolIoError::OutOfBounds("limit"))
    ));
    assert!(matches!(
        io.read_text_file("main.rs", Some(0), Some(10)).await,
        Err(ToolIoError::OutOfBounds("line"))
    ));
    assert!(
        port.requests.lock().expect("requests").is_empty(),
        "an out-of-bounds read reached the client"
    );
}

/// A command the client's terminal did not exit from reports the signal
/// that ended it, and one that reports neither is a fault rather than a
/// success with an unknown status.
#[test]
fn a_terminal_exit_status_is_read_the_way_the_reference_reads_it() {
    let output = json!({"output": "partial", "truncated": true});
    let signaled = shell_result(&json!({"exitCode": null, "signal": "SIGKILL"}), &output)
        .expect("a signal is an outcome");
    assert_eq!(signaled.returncode, -1);
    assert_eq!(signaled.stderr, "Process terminated by SIGKILL");
    assert!(signaled.truncated);

    let exited = shell_result(&json!({"exitCode": 2, "signal": null}), &output)
        .expect("an exit code is an outcome");
    assert_eq!(exited.returncode, 2);
    assert!(exited.stderr.is_empty());

    assert!(matches!(
        shell_result(&json!({"exitCode": null, "signal": null}), &output),
        Err(ToolIoError::MissingExitStatus)
    ));
}

#[tokio::test]
async fn a_terminal_below_the_reference_bounds_is_refused_before_it_is_sent() {
    let port = FakeClientPort::hosting([ClientToolCapability::Terminal]);
    let io = ClientToolIo::new("session-1", port.clone());
    assert!(matches!(
        io.run_shell(shell_request(0)).await,
        Err(ToolIoError::OutOfBounds("outputByteLimit"))
    ));
    assert!(
        port.requests.lock().expect("requests").is_empty(),
        "an out-of-bounds terminal reached the client"
    );
}

#[tokio::test]
async fn a_malformed_client_answer_names_the_field_it_is_missing() {
    struct SilentPort;

    impl ClientToolPort for SilentPort {
        fn request<'a>(&'a self, _request: ClientToolRequest) -> ToolIoFuture<'a> {
            Box::pin(async { Ok(json!({})) })
        }

        fn supports(&self, _capability: ClientToolCapability) -> bool {
            true
        }
    }

    let io = ClientToolIo::new("session-1", Arc::new(SilentPort));
    let failure = io
        .read_text_file("main.rs", None, None)
        .await
        .expect_err("an answer without content is a failure");
    assert!(matches!(failure, ToolIoError::MalformedResponse("content")));
    assert!(
        failure.to_string().contains("content"),
        "the failure does not name the field: {failure}"
    );
}

#[tokio::test]
async fn a_client_that_never_answers_names_the_delegation_it_left_open() {
    struct MutePort;

    impl ClientToolPort for MutePort {
        fn request<'a>(&'a self, _request: ClientToolRequest) -> ToolIoFuture<'a> {
            Box::pin(std::future::pending())
        }

        fn supports(&self, _capability: ClientToolCapability) -> bool {
            true
        }
    }

    let io =
        ClientToolIo::new("session-1", Arc::new(MutePort)).with_timeout(Duration::from_millis(20));
    let failure = io
        .read_text_file("main.rs", None, None)
        .await
        .expect_err("an unanswered read is a failure");
    assert!(
        failure
            .to_string()
            .contains("clientTool/readTextFile within"),
        "the failure does not name the delegation: {failure}"
    );
}

fn shell_request(output_byte_limit: u64) -> ClientShellRequest {
    ClientShellRequest {
        tool_call_id: Some("call-1".to_owned()),
        command: "echo ok".to_owned(),
        args: None,
        env: None,
        cwd: "/work".to_owned(),
        output_byte_limit,
        timeout: Duration::from_secs(30),
    }
}

#[tokio::test]
async fn a_delegated_command_runs_the_reference_terminal_sequence() {
    let port = FakeClientPort::hosting([ClientToolCapability::Terminal]);
    let io = ClientToolIo::new("session-1", port.clone());
    let result = io.run_shell(shell_request(1024)).await.expect("run");
    assert_eq!(result.stdout, "ok");
    assert_eq!(result.returncode, 0);
    assert!(result.stderr.is_empty());
    assert!(!result.truncated);
    assert_eq!(
        port.methods(),
        [
            "clientTool/terminal/create",
            "clientTool/terminal/wait",
            "clientTool/terminal/output",
            "clientTool/terminal/release",
        ],
        "the release is issued once, after the output"
    );
    assert_eq!(io.active_terminal_count().await, 0);
}

#[tokio::test]
async fn a_terminal_the_client_fails_mid_command_is_killed_and_released() {
    let port = FakeClientPort::hosting([ClientToolCapability::Terminal]);
    port.fail_wait.store(true, Ordering::Release);
    let io = ClientToolIo::new("session-1", port.clone());
    assert!(io.run_shell(shell_request(1024)).await.is_err());
    assert_eq!(
        port.methods(),
        [
            "clientTool/terminal/create",
            "clientTool/terminal/wait",
            "clientTool/terminal/kill",
            "clientTool/terminal/release",
        ],
        "a failed wait did not kill before releasing"
    );
    assert_eq!(io.active_terminal_count().await, 0);
}

#[derive(Default)]
struct BlockingClientPort {
    requests: StdMutex<Vec<ClientToolRequest>>,
    waiting: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

#[derive(Default)]
struct BlockingCreatePort {
    entered: tokio::sync::Notify,
    finish_create: tokio::sync::Notify,
    requests: StdMutex<Vec<ClientToolRequest>>,
}

impl ClientToolPort for BlockingCreatePort {
    fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a> {
        Box::pin(async move {
            lock(&self.requests).push(request.clone());
            match request {
                ClientToolRequest::TerminalCreate { .. } => {
                    self.entered.notify_one();
                    self.finish_create.notified().await;
                    Ok(json!({"terminalId": "late-client-terminal"}))
                }
                _ => Ok(json!({})),
            }
        })
    }

    fn supports(&self, _capability: ClientToolCapability) -> bool {
        true
    }
}

impl ClientToolPort for BlockingClientPort {
    fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a> {
        Box::pin(async move {
            lock(&self.requests).push(request.clone());
            match request {
                ClientToolRequest::TerminalCreate { .. } => {
                    Ok(json!({"terminalId": "client-terminal-cancelled"}))
                }
                ClientToolRequest::TerminalWait { .. } => {
                    self.waiting.notify_one();
                    std::future::pending::<Result<Value, ToolIoError>>().await
                }
                ClientToolRequest::TerminalRelease { .. } => {
                    self.released.notify_one();
                    Ok(json!({}))
                }
                _ => Ok(json!({})),
            }
        })
    }

    fn supports(&self, _capability: ClientToolCapability) -> bool {
        true
    }
}

#[tokio::test]
async fn client_task_abort_kills_and_releases_created_terminal() {
    let port = Arc::new(BlockingClientPort::default());
    let io = ClientToolIo::new("session-1", port.clone());
    let task_io = io.clone();
    let task = tokio::spawn(async move { task_io.run_shell(shell_request(1024)).await });
    port.waiting.notified().await;
    task.abort();
    assert!(task.await.expect_err("aborted").is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), port.released.notified())
        .await
        .expect("cleanup release");
    assert_eq!(io.active_terminal_count().await, 0);
    io.cleanup_all().await.expect("cleanup status");
    let requests = port.requests.lock().expect("requests");
    assert!(requests.iter().any(|request| matches!(
        request,
        ClientToolRequest::TerminalKill { terminal_id, .. }
            if terminal_id == "client-terminal-cancelled"
    )));
    assert!(requests.iter().any(|request| matches!(
        request,
        ClientToolRequest::TerminalRelease { terminal_id, .. }
            if terminal_id == "client-terminal-cancelled"
    )));
}

#[tokio::test]
async fn client_cleanup_keeps_unacknowledged_terminal_creation_tracked() {
    let port = Arc::new(BlockingCreatePort::default());
    let mut io = ClientToolIo::new("session-1", port.clone());
    io.cleanup_grace = Duration::from_millis(20);
    let task_io = io.clone();
    let task = tokio::spawn(async move { task_io.run_shell(shell_request(1024)).await });
    port.entered.notified().await;

    assert!(matches!(
        io.cleanup_all().await,
        Err(ToolIoError::CleanupFailures(failures))
            if failures.iter().any(|failure| failure.contains("acknowledgment"))
    ));
    assert_eq!(io.lifecycles.lock().await.entries.len(), 1);
    port.finish_create.notify_one();
    assert!(matches!(
        task.await.expect("run task"),
        Err(ToolIoError::LifecycleCancelled)
    ));
    io.cleanup_all().await.expect("late creation reconciled");
    let requests = port.requests.lock().expect("requests");
    assert!(requests.iter().any(|request| matches!(
        request,
        ClientToolRequest::TerminalKill { terminal_id, .. }
            if terminal_id == "late-client-terminal"
    )));
    assert!(requests.iter().any(|request| matches!(
        request,
        ClientToolRequest::TerminalRelease { terminal_id, .. }
            if terminal_id == "late-client-terminal"
    )));
}
