use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::child::{ChildGroup, Rung, TerminationError};
use crate::workspace::{GitInspector, GitInspectorFuture, GitState, WorkspaceError};

const DEFAULT_QUEUE_CAPACITY: usize = 128;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
const DEFAULT_CLEANUP_GRACE: Duration = Duration::from_secs(2);
/// How long one delegated request may stay unanswered before the tool reports
/// the delegation rather than holding the turn open on a client that stopped
/// responding.
pub const DEFAULT_CLIENT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

pub type ToolIoFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ToolIoError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessChunk {
    pub cursor: u64,
    pub stream: ProcessStream,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TerminalState {
    Running,
    Exited { code: Option<i32>, success: bool },
    Interrupted { code: Option<i32> },
    Failed { message: String },
}

impl TerminalState {
    fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub queue_capacity: usize,
    pub max_output_bytes: usize,
}

impl ProcessSpec {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, working_directory: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            working_directory: working_directory.into(),
            environment: BTreeMap::new(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRead {
    pub terminal_id: String,
    pub chunks: Vec<ProcessChunk>,
    pub state: TerminalState,
    pub backpressure_dropped: bool,
}

struct ManagedProcess {
    id: String,
    child: Mutex<ChildGroup>,
    stdin: Mutex<Option<ChildStdin>>,
    chunks: Mutex<mpsc::Receiver<ProcessChunk>>,
    readers: Mutex<Vec<JoinHandle<()>>>,
    state: Mutex<TerminalState>,
    output_dropped: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct TerminalManager {
    processes: Arc<Mutex<BTreeMap<String, Arc<ManagedProcess>>>>,
    next_terminal: Arc<AtomicU64>,
    cleanup_grace: Duration,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self {
            processes: Arc::new(Mutex::new(BTreeMap::new())),
            next_terminal: Arc::new(AtomicU64::new(1)),
            cleanup_grace: DEFAULT_CLEANUP_GRACE,
        }
    }
}

impl TerminalManager {
    #[must_use]
    pub fn with_cleanup_grace(cleanup_grace: Duration) -> Self {
        Self {
            cleanup_grace,
            ..Self::default()
        }
    }

    pub async fn run(&self, spec: ProcessSpec) -> Result<String, ProcessError> {
        if spec.program.as_os_str().is_empty() {
            return Err(ProcessError::InvalidProgram);
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.arguments)
            .current_dir(&spec.working_directory)
            .envs(&spec.environment);
        let (child, pipes) =
            ChildGroup::spawn(&mut command).map_err(|source| ProcessError::Spawn {
                program: spec.program.clone(),
                source,
            })?;
        let stdin = pipes.stdin;
        let stdout = pipes.stdout.ok_or(ProcessError::MissingPipe("stdout"))?;
        let stderr = pipes.stderr.ok_or(ProcessError::MissingPipe("stderr"))?;
        let terminal_sequence = self.next_terminal.fetch_add(1, Ordering::Relaxed);
        let terminal_id = format!("terminal-{terminal_sequence}");
        let (sender, receiver) = mpsc::channel(spec.queue_capacity.max(1));
        let cursor = Arc::new(AtomicU64::new(0));
        let output_bytes = Arc::new(AtomicUsize::new(0));
        let output_dropped = Arc::new(AtomicBool::new(false));
        let readers = vec![
            tokio::spawn(read_stream(
                stdout,
                ProcessStream::Stdout,
                sender.clone(),
                cursor.clone(),
                output_bytes.clone(),
                output_dropped.clone(),
                spec.max_output_bytes,
            )),
            tokio::spawn(read_stream(
                stderr,
                ProcessStream::Stderr,
                sender,
                cursor,
                output_bytes,
                output_dropped.clone(),
                spec.max_output_bytes,
            )),
        ];
        let process = Arc::new(ManagedProcess {
            id: terminal_id.clone(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            chunks: Mutex::new(receiver),
            readers: Mutex::new(readers),
            state: Mutex::new(TerminalState::Running),
            output_dropped,
        });
        self.processes
            .lock()
            .await
            .insert(terminal_id.clone(), process);
        Ok(terminal_id)
    }

    pub async fn write(&self, terminal_id: &str, bytes: &[u8]) -> Result<(), ProcessError> {
        let process = self.process(terminal_id).await?;
        if process.state.lock().await.is_terminal() {
            return Err(ProcessError::NotRunning(terminal_id.to_owned()));
        }
        let mut stdin = process.stdin.lock().await;
        let input = stdin
            .as_mut()
            .ok_or_else(|| ProcessError::StdinClosed(terminal_id.to_owned()))?;
        input
            .write_all(bytes)
            .await
            .map_err(|source| ProcessError::Io {
                terminal_id: terminal_id.to_owned(),
                source,
            })
    }

    pub async fn close_stdin(&self, terminal_id: &str) -> Result<(), ProcessError> {
        let process = self.process(terminal_id).await?;
        process.stdin.lock().await.take();
        Ok(())
    }

    pub async fn read(&self, terminal_id: &str) -> Result<ProcessRead, ProcessError> {
        let process = self.process(terminal_id).await?;
        let mut chunks = Vec::new();
        {
            let mut receiver = process.chunks.lock().await;
            while let Ok(chunk) = receiver.try_recv() {
                chunks.push(chunk);
            }
        }
        self.refresh_state(&process).await?;
        Ok(ProcessRead {
            terminal_id: terminal_id.to_owned(),
            chunks,
            state: process.state.lock().await.clone(),
            backpressure_dropped: process.output_dropped.swap(false, Ordering::AcqRel),
        })
    }

    pub async fn wait(&self, terminal_id: &str) -> Result<ProcessRead, ProcessError> {
        let process = self.process(terminal_id).await?;
        loop {
            self.refresh_state(&process).await?;
            if process.state.lock().await.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.join_readers(&process).await;
        self.read(terminal_id).await
    }

    pub async fn list(&self) -> Vec<(String, TerminalState)> {
        let processes = self
            .processes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut output = Vec::with_capacity(processes.len());
        for process in processes {
            let _ = self.refresh_state(&process).await;
            output.push((process.id.clone(), process.state.lock().await.clone()));
        }
        output.sort_by(|left, right| left.0.cmp(&right.0));
        output
    }

    pub async fn interrupt(&self, terminal_id: &str) -> Result<ProcessRead, ProcessError> {
        let process = self.process(terminal_id).await?;
        if process.state.lock().await.is_terminal() {
            return self.read(terminal_id).await;
        }
        self.interrupt_running(&process).await?;
        self.join_readers(&process).await;
        self.read(terminal_id).await
    }

    /// Signals a live process group and records the resulting exit status.
    ///
    /// The state stays `Running` on failure so a caller can retry rather than
    /// observe a terminal state for a process that is still alive.
    async fn interrupt_running(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        let status = process
            .child
            .lock()
            .await
            .shut_down(self.cleanup_grace, Rung::Terminate)
            .await
            .map_err(|error| process.termination_error(error))?;
        process
            .child
            .lock()
            .await
            .reap_group(self.cleanup_grace, true)
            .await
            .map_err(|error| process.termination_error(error))?;
        *process.state.lock().await = TerminalState::Interrupted {
            code: status.code(),
        };
        Ok(())
    }

    pub async fn release(&self, terminal_id: &str) -> Result<(), ProcessError> {
        let process = self.process(terminal_id).await?;
        self.refresh_state(&process).await?;
        if !process.state.lock().await.is_terminal() {
            return Err(ProcessError::NotRunning(terminal_id.to_owned()));
        }
        process
            .child
            .lock()
            .await
            .reap_group(self.cleanup_grace, false)
            .await
            .map_err(|error| process.termination_error(error))?;
        self.join_readers(&process).await;
        self.processes.lock().await.remove(terminal_id);
        Ok(())
    }

    pub async fn cleanup_all(&self) -> Result<Vec<ProcessRead>, ProcessError> {
        let processes = self
            .processes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut waits = FuturesUnordered::new();
        for process in processes {
            let manager = self.clone();
            waits.push(async move {
                let id = process.id.clone();
                (id, manager.finish_cleanup(process).await)
            });
        }
        let mut output = Vec::new();
        let mut failures = Vec::new();
        while let Some((id, result)) = waits.next().await {
            match result {
                Ok(read) => {
                    self.processes.lock().await.remove(&id);
                    output.push(read);
                }
                Err(error) => failures.push(format!("{id}: {error}")),
            }
        }
        output.sort_by(|left, right| left.terminal_id.cmp(&right.terminal_id));
        failures.sort();
        if failures.is_empty() {
            Ok(output)
        } else {
            Err(ProcessError::CleanupFailures(failures))
        }
    }

    async fn finish_cleanup(
        &self,
        process: Arc<ManagedProcess>,
    ) -> Result<ProcessRead, ProcessError> {
        let was_running = !process.state.lock().await.is_terminal();
        if was_running {
            self.interrupt_running(&process).await?;
        } else {
            process
                .child
                .lock()
                .await
                .reap_group(self.cleanup_grace, false)
                .await
                .map_err(|error| process.termination_error(error))?;
        }
        self.join_readers(&process).await;
        self.read(&process.id).await
    }

    async fn process(&self, terminal_id: &str) -> Result<Arc<ManagedProcess>, ProcessError> {
        self.processes
            .lock()
            .await
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| ProcessError::NotFound(terminal_id.to_owned()))
    }

    async fn refresh_state(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        if process.state.lock().await.is_terminal() {
            return Ok(());
        }
        let status = process
            .child
            .lock()
            .await
            .try_wait()
            .map_err(|source| ProcessError::Io {
                terminal_id: process.id.clone(),
                source,
            })?;
        if let Some(status) = status {
            *process.state.lock().await = TerminalState::Exited {
                code: status.code(),
                success: status.success(),
            };
        }
        Ok(())
    }

    async fn join_readers(&self, process: &ManagedProcess) {
        let readers = std::mem::take(&mut *process.readers.lock().await);
        for mut reader in readers {
            if tokio::time::timeout(self.cleanup_grace, &mut reader)
                .await
                .is_err()
            {
                reader.abort();
                let _ = reader.await;
                process.output_dropped.store(true, Ordering::Release);
            }
        }
    }
}

impl ManagedProcess {
    /// Names the terminal a shutdown failure belongs to.
    fn termination_error(&self, error: TerminationError) -> ProcessError {
        match error {
            TerminationError::Deadline => ProcessError::CleanupDeadline(self.id.clone()),
            TerminationError::Signal(message) | TerminationError::Wait(message) => {
                ProcessError::Signal {
                    terminal_id: self.id.clone(),
                    message,
                }
            }
        }
    }
}

impl GitInspector for TerminalManager {
    fn inspect<'a>(&'a self, root: &'a std::path::Path) -> GitInspectorFuture<'a> {
        Box::pin(async move {
            let mut spec = ProcessSpec::new("git", root);
            spec.arguments = vec![
                "-C".to_owned(),
                root.to_string_lossy().into_owned(),
                "status".to_owned(),
                "--porcelain=v1".to_owned(),
                "--branch".to_owned(),
            ];
            let terminal_id = self
                .run(spec)
                .await
                .map_err(|error| WorkspaceError::GitInspection(error.to_string()))?;
            let output = self
                .wait(&terminal_id)
                .await
                .map_err(|error| WorkspaceError::GitInspection(error.to_string()))?;
            let _ = self.release(&terminal_id).await;
            let bytes = output
                .chunks
                .iter()
                .filter(|chunk| chunk.stream == ProcessStream::Stdout)
                .flat_map(|chunk| chunk.bytes.iter().copied())
                .collect::<Vec<_>>();
            let text = String::from_utf8(bytes).map_err(|_| {
                WorkspaceError::GitInspection("git status output is not valid UTF-8".to_owned())
            })?;
            Ok(GitState::from_porcelain(&text))
        })
    }
}

async fn read_stream<R>(
    mut reader: R,
    stream: ProcessStream,
    sender: mpsc::Sender<ProcessChunk>,
    cursor: Arc<AtomicU64>,
    output_bytes: Arc<AtomicUsize>,
    output_dropped: Arc<AtomicBool>,
    max_output_bytes: usize,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 8_192];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let previous = output_bytes.fetch_add(read, Ordering::AcqRel);
        if previous >= max_output_bytes {
            output_dropped.store(true, Ordering::Release);
            continue;
        }
        let accepted = read.min(max_output_bytes.saturating_sub(previous));
        if accepted < read {
            output_dropped.store(true, Ordering::Release);
        }
        let chunk_cursor = cursor.fetch_add(1, Ordering::Relaxed);
        if sender
            .try_send(ProcessChunk {
                cursor: chunk_cursor,
                stream,
                bytes: buffer[..accepted].to_vec(),
            })
            .is_err()
        {
            output_dropped.store(true, Ordering::Release);
        }
    }
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
    lifecycles: Arc<Mutex<ClientLifecycles>>,
    next_lifecycle: Arc<AtomicU64>,
    cleanup_failures: Arc<Mutex<Vec<String>>>,
    cleanup_grace: Duration,
}

struct ClientLifecycle {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct ClientLifecycles {
    closing: bool,
    entries: BTreeMap<u64, ClientLifecycle>,
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
fn shell_result(waited: &Value, output: &Value) -> Result<ClientShellResult, ToolIoError> {
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

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("process program is empty")]
    InvalidProgram,
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("spawned process has no {0} pipe")]
    MissingPipe(&'static str),
    #[error("terminal `{0}` does not exist")]
    NotFound(String),
    #[error("terminal `{0}` is not running")]
    NotRunning(String),
    #[error("terminal `{0}` stdin is closed")]
    StdinClosed(String),
    #[error("terminal `{terminal_id}` I/O failed: {source}")]
    Io {
        terminal_id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("terminal `{terminal_id}` signal failed: {message}")]
    Signal {
        terminal_id: String,
        message: String,
    },
    #[error("terminal `{0}` has no owned process group")]
    MissingProcessGroup(String),
    #[error("terminal `{0}` exceeded its cleanup deadline and remains running")]
    CleanupDeadline(String),
    #[error("one or more terminals failed cleanup: {0:?}")]
    CleanupFailures(Vec<String>),
}

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

/// Suspends this process the way `Ctrl+Z` does in a shell job.
///
/// The caller is responsible for restoring the terminal first: the process stops
/// where the signal is delivered and resumes on the following line.
#[cfg(unix)]
pub fn suspend_current_process() -> Result<(), String> {
    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTSTP)
        .map_err(|error| format!("suspend failed: {error}"))
}

#[cfg(not(unix))]
pub fn suspend_current_process() -> Result<(), String> {
    Err("suspend is unsupported on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_process_streams_and_reaps_owned_group() {
        let directory = tempdir().expect("tempdir");
        let manager = TerminalManager::default();
        let mut spec = ProcessSpec::new("/bin/sh", directory.path());
        spec.arguments = vec!["-c".to_owned(), "printf out; printf err >&2".to_owned()];
        let terminal_id = manager.run(spec).await.expect("spawn");
        let output = manager.wait(&terminal_id).await.expect("wait");
        assert_eq!(
            output.state,
            TerminalState::Exited {
                code: Some(0),
                success: true
            }
        );
        let stdout = output
            .chunks
            .iter()
            .filter(|chunk| chunk.stream == ProcessStream::Stdout)
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect::<Vec<_>>();
        let stderr = output
            .chunks
            .iter()
            .filter(|chunk| chunk.stream == ProcessStream::Stderr)
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
        manager.release(&terminal_id).await.expect("release");
        assert!(manager.list().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn release_reaps_descendants_after_process_group_leader_exits() {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let directory = tempdir().expect("tempdir");
        let manager = TerminalManager::with_cleanup_grace(Duration::from_millis(200));
        let mut spec = ProcessSpec::new("/bin/sh", directory.path());
        spec.arguments = vec![
            "-c".to_owned(),
            "sleep 30 </dev/null >/dev/null 2>&1 & printf %s $!".to_owned(),
        ];
        let terminal_id = manager.run(spec).await.expect("spawn");
        let output = manager.wait(&terminal_id).await.expect("wait");
        let descendant = String::from_utf8(
            output
                .chunks
                .iter()
                .filter(|chunk| chunk.stream == ProcessStream::Stdout)
                .flat_map(|chunk| chunk.bytes.iter().copied())
                .collect(),
        )
        .expect("pid")
        .parse::<i32>()
        .expect("numeric pid");
        manager.release(&terminal_id).await.expect("release");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            match kill(Pid::from_raw(descendant), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) | Err(Errno::EPERM) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => {
                    let gone = matches!(&result, Err(Errno::ESRCH));
                    assert!(gone, "descendant remained after release: {result:?}");
                    break;
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_signals_all_processes_and_leaves_no_handles() {
        let directory = tempdir().expect("tempdir");
        let manager = TerminalManager::with_cleanup_grace(Duration::from_millis(200));
        for _ in 0..2 {
            let mut spec = ProcessSpec::new("/bin/sh", directory.path());
            spec.arguments = vec!["-c".to_owned(), "sleep 30".to_owned()];
            manager.run(spec).await.expect("spawn");
        }
        let outcomes = manager.cleanup_all().await.expect("cleanup");
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome.state, TerminalState::Interrupted { .. }))
        );
    }

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
                self.requests
                    .lock()
                    .map_err(|_| ToolIoError::Request("fake lock poisoned".to_owned()))?
                    .push(request.clone());
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

        let io = ClientToolIo::new("session-1", Arc::new(MutePort))
            .with_timeout(Duration::from_millis(20));
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
                self.requests
                    .lock()
                    .map_err(|_| ToolIoError::Request("fake lock poisoned".to_owned()))?
                    .push(request.clone());
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
                self.requests
                    .lock()
                    .map_err(|_| ToolIoError::Request("fake lock poisoned".to_owned()))?
                    .push(request.clone());
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
}
