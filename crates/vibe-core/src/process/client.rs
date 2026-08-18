//! Running a tool on the connected client instead of on this host.
//!
//! An editor that hosts the session may own the filesystem and the terminal the
//! agent should be reaching: its buffers can be ahead of what is on disk, and
//! its terminal is the one the user is watching. A client declares which of
//! those it hosts, and a tool that has a matching capability delegates rather
//! than acting locally.
//!
//! Delegation is a request-response over a port the client answers, which means
//! every one of them can be abandoned: a cancelled turn has to release the
//! terminal the client opened for it, so the lifecycles here own the cleanup
//! rather than leaving it to the tool that started the call.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::{DEFAULT_CLEANUP_GRACE, DEFAULT_CLIENT_TOOL_TIMEOUT, ToolIoFuture};

#[derive(Debug, Error)]
pub enum ToolIoError {
    #[error("client did not advertise capability {0:?}")]
    CapabilityNotAdvertised(ClientToolCapability),
    #[error("client response is missing `{0}`")]
    MalformedResponse(&'static str),
    #[error("the client did not answer {method} within {seconds}s")]
    Unanswered { method: &'static str, seconds: u64 },
    #[error("`{0}` is below the bound the client tool protocol declares")]
    OutOfBounds(&'static str),
    #[error("client terminal returned no exit status")]
    MissingExitStatus,
    #[error("client ToolIO requires a Tokio runtime")]
    RuntimeUnavailable,
    #[error("client terminal lifecycle ended before returning a result")]
    LifecycleEnded,
    #[error("client terminal lifecycle was cancelled by session cleanup")]
    LifecycleCancelled,
    #[error("client terminal lifecycle task failed: {0}")]
    LifecycleJoin(String),
    #[error("client terminal cleanup exceeded its deadline")]
    CleanupDeadline,
    #[error("one or more client terminals failed cleanup: {0:?}")]
    CleanupFailures(Vec<String>),
    #[error("client tool request failed: {0}")]
    Request(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClientToolCapability {
    FilesystemRead,
    FilesystemWrite,
    Terminal,
}

impl ClientToolCapability {
    /// The name a client declares this capability under during `initialize`.
    #[must_use]
    pub const fn declaration(self) -> &'static str {
        match self {
            Self::FilesystemRead => "filesystem/read",
            Self::FilesystemWrite => "filesystem/write",
            Self::Terminal => "terminal",
        }
    }
}

/// One server-to-client tool request, serialized as the method name the
/// reference routes and the parameter object it validates.
///
/// The variant names carry their reference method names and the fields their
/// reference aliases, so a request is put on the wire by serializing it rather
/// than by a second mapping that could drift from this one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum ClientToolRequest {
    #[serde(rename = "clientTool/readTextFile")]
    ReadTextFile {
        #[serde(rename = "sessionId")]
        session_id: String,
        path: String,
        line: Option<u64>,
        limit: Option<u64>,
    },
    #[serde(rename = "clientTool/writeTextFile")]
    WriteTextFile {
        #[serde(rename = "sessionId")]
        session_id: String,
        path: String,
        content: String,
    },
    #[serde(rename = "clientTool/terminal/create")]
    TerminalCreate {
        #[serde(rename = "sessionId")]
        session_id: String,
        command: String,
        args: Option<Vec<String>>,
        env: Option<BTreeMap<String, String>>,
        cwd: String,
        #[serde(rename = "outputByteLimit")]
        output_byte_limit: u64,
        #[serde(rename = "toolCallId")]
        tool_call_id: Option<String>,
    },
    #[serde(rename = "clientTool/terminal/wait")]
    TerminalWait {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "terminalId")]
        terminal_id: String,
    },
    #[serde(rename = "clientTool/terminal/output")]
    TerminalOutput {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "terminalId")]
        terminal_id: String,
    },
    #[serde(rename = "clientTool/terminal/kill")]
    TerminalKill {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "terminalId")]
        terminal_id: String,
    },
    #[serde(rename = "clientTool/terminal/release")]
    TerminalRelease {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "terminalId")]
        terminal_id: String,
    },
}

impl ClientToolRequest {
    /// The reference method name this request is issued under.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::ReadTextFile { .. } => "clientTool/readTextFile",
            Self::WriteTextFile { .. } => "clientTool/writeTextFile",
            Self::TerminalCreate { .. } => "clientTool/terminal/create",
            Self::TerminalWait { .. } => "clientTool/terminal/wait",
            Self::TerminalOutput { .. } => "clientTool/terminal/output",
            Self::TerminalKill { .. } => "clientTool/terminal/kill",
            Self::TerminalRelease { .. } => "clientTool/terminal/release",
        }
    }

    #[must_use]
    pub const fn required_capability(&self) -> ClientToolCapability {
        match self {
            Self::ReadTextFile { .. } => ClientToolCapability::FilesystemRead,
            Self::WriteTextFile { .. } => ClientToolCapability::FilesystemWrite,
            Self::TerminalCreate { .. }
            | Self::TerminalWait { .. }
            | Self::TerminalOutput { .. }
            | Self::TerminalKill { .. }
            | Self::TerminalRelease { .. } => ClientToolCapability::Terminal,
        }
    }

    /// Rejects a request the reference model would refuse before it costs a
    /// round trip.
    ///
    /// `line` and `limit` are bounded at one and `outputByteLimit` above zero
    /// upstream, so a call that violates either is a fault on this side and is
    /// reported as one rather than delegated for the client to reject.
    fn validate(&self) -> Result<(), ToolIoError> {
        match self {
            Self::ReadTextFile { line, limit, .. } => {
                if *line == Some(0) {
                    return Err(ToolIoError::OutOfBounds("line"));
                }
                if *limit == Some(0) {
                    return Err(ToolIoError::OutOfBounds("limit"));
                }
                Ok(())
            }
            Self::TerminalCreate {
                output_byte_limit, ..
            } => {
                if *output_byte_limit == 0 {
                    return Err(ToolIoError::OutOfBounds("outputByteLimit"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub trait ClientToolPort: Send + Sync {
    fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a>;

    /// Whether the connected client declared this capability during
    /// `initialize`. Asked per call rather than snapshotted, so a reconnection
    /// that changes the declaration is honored by tools already registered.
    fn supports(&self, capability: ClientToolCapability) -> bool;
}

/// What one delegated command needs, mirroring the reference
/// `ShellCommandRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientShellRequest {
    pub tool_call_id: Option<String>,
    pub command: String,
    pub args: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, String>>,
    pub cwd: String,
    pub output_byte_limit: u64,
    pub timeout: Duration,
}

/// What a delegated command produced, mirroring the reference
/// `ShellCommandResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientShellResult {
    pub stdout: String,
    pub stderr: String,
    pub returncode: i32,
    pub truncated: bool,
}

/// The file and terminal access one session delegates to its client.
///
/// Every request carries the session the tools were registered for, which is
/// what lets a client route the delegation back to the buffer or terminal the
/// user is looking at.
#[derive(Clone)]
pub struct ClientToolIo {
    session_id: String,
    port: Arc<dyn ClientToolPort>,
    timeout: Duration,
    active_terminals: Arc<Mutex<BTreeSet<String>>>,
    pub(super) lifecycles: Arc<Mutex<ClientLifecycles>>,
    next_lifecycle: Arc<AtomicU64>,
    cleanup_failures: Arc<Mutex<Vec<String>>>,
    pub(super) cleanup_grace: Duration,
}

pub(super) struct ClientLifecycle {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct ClientLifecycles {
    closing: bool,
    pub(super) entries: BTreeMap<u64, ClientLifecycle>,
}

struct ClientShellLifecycle {
    port: Arc<dyn ClientToolPort>,
    active_terminals: Arc<Mutex<BTreeSet<String>>>,
    cleanup_failures: Arc<Mutex<Vec<String>>>,
    cleanup_grace: Duration,
    session_id: String,
    create: ClientToolRequest,
    wait_timeout: Duration,
    request_timeout: Duration,
    cancel: watch::Receiver<bool>,
    result_sender: oneshot::Sender<Result<ClientShellResult, ToolIoError>>,
}

impl ClientToolIo {
    #[must_use]
    pub fn new(session_id: impl Into<String>, port: Arc<dyn ClientToolPort>) -> Self {
        Self {
            session_id: session_id.into(),
            port,
            timeout: DEFAULT_CLIENT_TOOL_TIMEOUT,
            active_terminals: Arc::new(Mutex::new(BTreeSet::new())),
            lifecycles: Arc::new(Mutex::new(ClientLifecycles::default())),
            next_lifecycle: Arc::new(AtomicU64::new(1)),
            cleanup_failures: Arc::new(Mutex::new(Vec::new())),
            cleanup_grace: DEFAULT_CLEANUP_GRACE,
        }
    }

    /// The same delegation with a stated deadline, which is how a test drives a
    /// client that never answers without waiting out the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn supports_read(&self) -> bool {
        self.port.supports(ClientToolCapability::FilesystemRead)
    }

    #[must_use]
    pub fn supports_write(&self) -> bool {
        self.port.supports(ClientToolCapability::FilesystemWrite)
    }

    #[must_use]
    pub fn supports_terminal(&self) -> bool {
        self.port.supports(ClientToolCapability::Terminal)
    }

    /// Reads a file through the client, returning what its buffer holds.
    ///
    /// `line` is sent only past the first, matching the reference, which leaves
    /// the field absent when the read starts at the top of the file.
    pub async fn read_text_file(
        &self,
        path: &str,
        line: Option<u64>,
        limit: Option<u64>,
    ) -> Result<String, ToolIoError> {
        let response = self
            .request(ClientToolRequest::ReadTextFile {
                session_id: self.session_id.clone(),
                path: path.to_owned(),
                line: line.filter(|line| *line != 1),
                limit,
            })
            .await?;
        response
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(ToolIoError::MalformedResponse("content"))
    }

    pub async fn write_text_file(&self, path: &str, content: &str) -> Result<(), ToolIoError> {
        self.request(ClientToolRequest::WriteTextFile {
            session_id: self.session_id.clone(),
            path: path.to_owned(),
            content: content.to_owned(),
        })
        .await
        .map(|_| ())
    }

    pub async fn request(&self, request: ClientToolRequest) -> Result<Value, ToolIoError> {
        let required = request.required_capability();
        if !self.port.supports(required) {
            return Err(ToolIoError::CapabilityNotAdvertised(required));
        }
        request.validate()?;
        let method = request.method();
        let timeout = self.timeout;
        tokio::time::timeout(timeout, self.port.request(request))
            .await
            .map_err(|_| ToolIoError::Unanswered {
                method,
                seconds: timeout.as_secs(),
            })?
    }

    /// Runs one command on a client terminal, from creation to release.
    ///
    /// The lifecycle runs in its own task so an abandoned call still reaches the
    /// kill and the release: a terminal the client opened on our behalf is ours
    /// to close, and dropping the future is not an excuse to leak it.
    pub async fn run_shell(
        &self,
        request: ClientShellRequest,
    ) -> Result<ClientShellResult, ToolIoError> {
        if !self.port.supports(ClientToolCapability::Terminal) {
            return Err(ToolIoError::CapabilityNotAdvertised(
                ClientToolCapability::Terminal,
            ));
        }
        let create = ClientToolRequest::TerminalCreate {
            session_id: self.session_id.clone(),
            command: request.command,
            args: request.args,
            env: request.env,
            cwd: request.cwd,
            output_byte_limit: request.output_byte_limit,
            tool_call_id: request.tool_call_id,
        };
        create.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ToolIoError::RuntimeUnavailable)?;
        let port = self.port.clone();
        let active_terminals = self.active_terminals.clone();
        let cleanup_failures = self.cleanup_failures.clone();
        let cleanup_grace = self.cleanup_grace;
        let session_id = self.session_id.clone();
        let request_timeout = self.timeout;
        let wait_timeout = request.timeout;
        let lifecycle_id = self.next_lifecycle.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancel_receiver) = watch::channel(false);
        let (result_sender, result_receiver) = oneshot::channel();
        let mut lifecycles = self.lifecycles.lock().await;
        if lifecycles.closing {
            return Err(ToolIoError::LifecycleCancelled);
        }
        let handle = runtime.spawn(async move {
            run_client_shell_lifecycle(ClientShellLifecycle {
                port,
                active_terminals,
                cleanup_failures,
                cleanup_grace,
                session_id,
                create,
                wait_timeout,
                request_timeout,
                cancel: cancel_receiver,
                result_sender,
            })
            .await;
        });
        lifecycles
            .entries
            .insert(lifecycle_id, ClientLifecycle { cancel, handle });
        drop(lifecycles);
        let result = result_receiver
            .await
            .map_err(|_| ToolIoError::LifecycleEnded)?;
        if let Some(lifecycle) = self.lifecycles.lock().await.entries.remove(&lifecycle_id) {
            lifecycle
                .handle
                .await
                .map_err(|error| ToolIoError::LifecycleJoin(error.to_string()))?;
        }
        result
    }

    pub async fn cleanup_all(&self) -> Result<(), ToolIoError> {
        let lifecycles = {
            let mut registry = self.lifecycles.lock().await;
            registry.closing = true;
            std::mem::take(&mut registry.entries)
        };
        for lifecycle in lifecycles.values() {
            let _ = lifecycle.cancel.send(true);
        }
        for (lifecycle_id, lifecycle) in lifecycles {
            let ClientLifecycle { cancel, mut handle } = lifecycle;
            match tokio::time::timeout(self.cleanup_grace.saturating_mul(2), &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    self.cleanup_failures.lock().await.push(format!(
                        "lifecycle-{lifecycle_id}: task join failed: {error}"
                    ));
                }
                Err(_) => {
                    self.cleanup_failures.lock().await.push(format!(
                        "lifecycle-{lifecycle_id}: cleanup is still awaiting client acknowledgment"
                    ));
                    self.lifecycles
                        .lock()
                        .await
                        .entries
                        .insert(lifecycle_id, ClientLifecycle { cancel, handle });
                }
            }
        }
        let terminal_ids = self
            .active_terminals
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for terminal_id in terminal_ids {
            cleanup_client_terminal(
                &self.port,
                &self.session_id,
                &terminal_id,
                self.cleanup_grace,
                &self.cleanup_failures,
            )
            .await;
            self.active_terminals.lock().await.remove(&terminal_id);
        }
        let failures = std::mem::take(&mut *self.cleanup_failures.lock().await);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ToolIoError::CleanupFailures(failures))
        }
    }

    pub async fn active_terminal_count(&self) -> usize {
        self.active_terminals.lock().await.len()
    }
}

async fn run_client_shell_lifecycle(context: ClientShellLifecycle) {
    let ClientShellLifecycle {
        port,
        active_terminals,
        cleanup_failures,
        cleanup_grace,
        session_id,
        create,
        wait_timeout,
        request_timeout,
        mut cancel,
        mut result_sender,
    } = context;
    let created = tokio::time::timeout(request_timeout, port.request(create));
    tokio::pin!(created);
    // A cancellation raised while the client is still minting the terminal
    // cannot be sent to it: the reference declares no method to withdraw a
    // creation, and a terminal id that arrives after we stopped waiting is
    // still ours to close. So the creation is awaited out and then killed.
    let cancelled = tokio::select! {
        created = &mut created => Some(created),
        changed = cancel.changed() => {
            let _ = changed;
            None
        }
        () = result_sender.closed() => None,
    };
    let abandoned = cancelled.is_none();
    let created = match cancelled {
        Some(created) => created,
        None => created.await,
    };
    let created = match created {
        Ok(Ok(created)) => created,
        Ok(Err(error)) => {
            let _ = result_sender.send(Err(error));
            return;
        }
        Err(_) => {
            let _ = result_sender.send(Err(ToolIoError::Unanswered {
                method: "clientTool/terminal/create",
                seconds: request_timeout.as_secs(),
            }));
            return;
        }
    };
    let terminal_id = match created.get("terminalId").and_then(Value::as_str) {
        Some(terminal_id) => terminal_id.to_owned(),
        None => {
            let _ = result_sender.send(Err(ToolIoError::MalformedResponse("terminalId")));
            return;
        }
    };
    active_terminals.lock().await.insert(terminal_id.clone());
    if abandoned || result_sender.is_closed() || *cancel.borrow() {
        cleanup_client_terminal(
            &port,
            &session_id,
            &terminal_id,
            cleanup_grace,
            &cleanup_failures,
        )
        .await;
        active_terminals.lock().await.remove(&terminal_id);
        let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
        return;
    }
    let terminal = |terminal_id: &str| (session_id.clone(), terminal_id.to_owned());
    let (wait_session, wait_terminal) = terminal(&terminal_id);
    let waited = tokio::select! {
        waited = tokio::time::timeout(wait_timeout, port.request(ClientToolRequest::TerminalWait {
            session_id: wait_session,
            terminal_id: wait_terminal,
        })) => waited.unwrap_or(Err(ToolIoError::Unanswered {
            method: "clientTool/terminal/wait",
            seconds: wait_timeout.as_secs(),
        })),
        () = result_sender.closed() => {
            cleanup_client_terminal(&port, &session_id, &terminal_id, cleanup_grace, &cleanup_failures).await;
            active_terminals.lock().await.remove(&terminal_id);
            return;
        }
        changed = cancel.changed() => {
            let _ = changed;
            cleanup_client_terminal(&port, &session_id, &terminal_id, cleanup_grace, &cleanup_failures).await;
            active_terminals.lock().await.remove(&terminal_id);
            let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
            return;
        }
    };
    let (output_session, output_terminal) = terminal(&terminal_id);
    let (waited, output) = match waited {
        Ok(waited) => {
            let output = tokio::select! {
                output = tokio::time::timeout(request_timeout, port.request(ClientToolRequest::TerminalOutput {
                    session_id: output_session,
                    terminal_id: output_terminal,
                })) => output.unwrap_or(Err(ToolIoError::Unanswered {
                    method: "clientTool/terminal/output",
                    seconds: request_timeout.as_secs(),
                })),
                () = result_sender.closed() => {
                    cleanup_client_terminal(&port, &session_id, &terminal_id, cleanup_grace, &cleanup_failures).await;
                    active_terminals.lock().await.remove(&terminal_id);
                    return;
                }
                changed = cancel.changed() => {
                    let _ = changed;
                    cleanup_client_terminal(&port, &session_id, &terminal_id, cleanup_grace, &cleanup_failures).await;
                    active_terminals.lock().await.remove(&terminal_id);
                    let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
                    return;
                }
            };
            (waited, output)
        }
        // A wait this client failed or never answered leaves its command
        // running, so the kill precedes the release rather than the release
        // standing alone.
        Err(error) => {
            let killed = cleanup_client_request(
                &port,
                ClientToolRequest::TerminalKill {
                    session_id: session_id.clone(),
                    terminal_id: terminal_id.clone(),
                },
                cleanup_grace,
            )
            .await;
            if let Err(failure) = killed {
                record_cleanup_failure(&cleanup_failures, &terminal_id, failure).await;
            }
            (Value::Null, Err(error))
        }
    };
    // The release closes every path, including the ones the client failed, so a
    // terminal it opened for us is never left behind.
    let release = cleanup_client_request(
        &port,
        ClientToolRequest::TerminalRelease {
            session_id: session_id.clone(),
            terminal_id: terminal_id.clone(),
        },
        cleanup_grace,
    )
    .await;
    active_terminals.lock().await.remove(&terminal_id);
    let result = match (output, release) {
        (Ok(output), Ok(_)) => shell_result(&waited, &output),
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    let _ = result_sender.send(result);
}

/// The command outcome the reference assembles from the wait and the output.
///
/// A terminal that reports neither an exit code nor a signal has told us
/// nothing about how the command ended, which upstream treats as a fault rather
/// than as a success with an unknown status.
pub(crate) fn shell_result(
    waited: &Value,
    output: &Value,
) -> Result<ClientShellResult, ToolIoError> {
    let exit_code = waited.get("exitCode").and_then(Value::as_i64);
    let signal = waited
        .get("signal")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if exit_code.is_none() && signal.is_none() {
        return Err(ToolIoError::MissingExitStatus);
    }
    let stdout = output
        .get("output")
        .and_then(Value::as_str)
        .ok_or(ToolIoError::MalformedResponse("output"))?
        .to_owned();
    let truncated = output
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or(ToolIoError::MalformedResponse("truncated"))?;
    Ok(ClientShellResult {
        stdout,
        stderr: signal.map_or_else(String::new, |signal| {
            format!("Process terminated by {signal}")
        }),
        returncode: exit_code
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or(-1),
        truncated,
    })
}

async fn cleanup_client_terminal(
    port: &Arc<dyn ClientToolPort>,
    session_id: &str,
    terminal_id: &str,
    cleanup_grace: Duration,
    cleanup_failures: &Arc<Mutex<Vec<String>>>,
) {
    for request in [
        ClientToolRequest::TerminalKill {
            session_id: session_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
        },
        ClientToolRequest::TerminalRelease {
            session_id: session_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
        },
    ] {
        if let Err(error) = cleanup_client_request(port, request, cleanup_grace).await {
            record_cleanup_failure(cleanup_failures, terminal_id, error).await;
        }
    }
}

async fn cleanup_client_request(
    port: &Arc<dyn ClientToolPort>,
    request: ClientToolRequest,
    cleanup_grace: Duration,
) -> Result<Value, ToolIoError> {
    tokio::time::timeout(cleanup_grace, port.request(request))
        .await
        .map_err(|_| ToolIoError::CleanupDeadline)?
}

async fn record_cleanup_failure(
    failures: &Arc<Mutex<Vec<String>>>,
    terminal_id: &str,
    error: ToolIoError,
) {
    failures
        .lock()
        .await
        .push(format!("{terminal_id}: {error}"));
}
