use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
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

#[cfg(windows)]
use command_group::{AsyncCommandGroup, AsyncGroupChild};
#[cfg(not(windows))]
use tokio::process::Child;

use crate::workspace::{GitInspector, GitInspectorFuture, GitState, WorkspaceError};

const DEFAULT_QUEUE_CAPACITY: usize = 128;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
const DEFAULT_CLEANUP_GRACE: Duration = Duration::from_secs(2);

#[cfg(windows)]
type ManagedChild = AsyncGroupChild;
#[cfg(not(windows))]
type ManagedChild = Child;

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
    child: Mutex<ManagedChild>,
    stdin: Mutex<Option<ChildStdin>>,
    chunks: Mutex<mpsc::Receiver<ProcessChunk>>,
    readers: Mutex<Vec<JoinHandle<()>>>,
    state: Mutex<TerminalState>,
    process_group: Option<i32>,
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
            .envs(&spec.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
            program: spec.program.clone(),
            source,
        })?;
        #[cfg(windows)]
        let mut child = {
            let mut group = command.group();
            group.kill_on_drop(true);
            group.spawn().map_err(|source| ProcessError::Spawn {
                program: spec.program.clone(),
                source,
            })?
        };
        let process_group = child.id().and_then(|id| i32::try_from(id).ok());
        #[cfg(not(windows))]
        let stdin = child.stdin.take();
        #[cfg(not(windows))]
        let stdout = child
            .stdout
            .take()
            .ok_or(ProcessError::MissingPipe("stdout"))?;
        #[cfg(not(windows))]
        let stderr = child
            .stderr
            .take()
            .ok_or(ProcessError::MissingPipe("stderr"))?;
        #[cfg(windows)]
        let (stdin, stdout, stderr) = {
            let child = child.inner();
            (
                child.stdin.take(),
                child
                    .stdout
                    .take()
                    .ok_or(ProcessError::MissingPipe("stdout"))?,
                child
                    .stderr
                    .take()
                    .ok_or(ProcessError::MissingPipe("stderr"))?,
            )
        };
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
            process_group,
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
        self.signal_graceful(&process)?;
        let graceful = {
            let mut child = process.child.lock().await;
            tokio::time::timeout(self.cleanup_grace, child.wait()).await
        };
        let status = match graceful {
            Ok(Ok(status)) => status,
            Ok(Err(source)) => {
                *process.state.lock().await = TerminalState::Running;
                return Err(ProcessError::Io {
                    terminal_id: terminal_id.to_owned(),
                    source,
                });
            }
            Err(_) => {
                self.signal_force(&process)?;
                let mut child = process.child.lock().await;
                match tokio::time::timeout(self.cleanup_grace, child.wait()).await {
                    Ok(Ok(status)) => status,
                    Ok(Err(source)) => {
                        *process.state.lock().await = TerminalState::Running;
                        return Err(ProcessError::Io {
                            terminal_id: terminal_id.to_owned(),
                            source,
                        });
                    }
                    Err(_) => {
                        *process.state.lock().await = TerminalState::Running;
                        return Err(ProcessError::CleanupDeadline(terminal_id.to_owned()));
                    }
                }
            }
        };
        self.terminate_remaining_group(&process, true).await?;
        *process.state.lock().await = TerminalState::Interrupted {
            code: status.code(),
        };
        self.join_readers(&process).await;
        self.read(terminal_id).await
    }

    pub async fn release(&self, terminal_id: &str) -> Result<(), ProcessError> {
        let process = self.process(terminal_id).await?;
        self.refresh_state(&process).await?;
        if !process.state.lock().await.is_terminal() {
            return Err(ProcessError::NotRunning(terminal_id.to_owned()));
        }
        self.terminate_remaining_group(&process, false).await?;
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
            self.signal_graceful(&process)?;
            let graceful = {
                let mut child = process.child.lock().await;
                tokio::time::timeout(self.cleanup_grace, child.wait()).await
            };
            let status = match graceful {
                Ok(Ok(status)) => status,
                Ok(Err(source)) => {
                    *process.state.lock().await = TerminalState::Running;
                    return Err(ProcessError::Io {
                        terminal_id: process.id.clone(),
                        source,
                    });
                }
                Err(_) => {
                    self.signal_force(&process)?;
                    let mut child = process.child.lock().await;
                    match tokio::time::timeout(self.cleanup_grace, child.wait()).await {
                        Ok(Ok(status)) => status,
                        Ok(Err(source)) => {
                            *process.state.lock().await = TerminalState::Running;
                            return Err(ProcessError::Io {
                                terminal_id: process.id.clone(),
                                source,
                            });
                        }
                        Err(_) => {
                            *process.state.lock().await = TerminalState::Running;
                            return Err(ProcessError::CleanupDeadline(process.id.clone()));
                        }
                    }
                }
            };
            *process.state.lock().await = TerminalState::Interrupted {
                code: status.code(),
            };
        }
        self.terminate_remaining_group(&process, was_running)
            .await?;
        self.join_readers(&process).await;
        self.read(&process.id).await
    }

    async fn terminate_remaining_group(
        &self,
        process: &ManagedProcess,
        graceful_already_sent: bool,
    ) -> Result<(), ProcessError> {
        #[cfg(unix)]
        {
            if !self.group_alive(process)? {
                return Ok(());
            }
            if !graceful_already_sent {
                self.signal_graceful(process)?;
            }
            if self.wait_for_group_exit(process).await? {
                return Ok(());
            }
            self.signal_force(process)?;
            if self.wait_for_group_exit(process).await? {
                return Ok(());
            }
            Err(ProcessError::CleanupDeadline(process.id.clone()))
        }
        #[cfg(windows)]
        {
            if !graceful_already_sent {
                self.signal_force(process)?;
            }
            let mut child = process.child.lock().await;
            match tokio::time::timeout(self.cleanup_grace, child.wait()).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(source)) => Err(ProcessError::Io {
                    terminal_id: process.id.clone(),
                    source,
                }),
                Err(_) => Err(ProcessError::CleanupDeadline(process.id.clone())),
            }
        }
    }

    #[cfg(unix)]
    async fn wait_for_group_exit(&self, process: &ManagedProcess) -> Result<bool, ProcessError> {
        let deadline = tokio::time::Instant::now() + self.cleanup_grace;
        loop {
            if !self.group_alive(process)? {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    fn group_alive(&self, process: &ManagedProcess) -> Result<bool, ProcessError> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let Some(group) = process.process_group else {
            return Err(ProcessError::MissingProcessGroup(process.id.clone()));
        };
        match kill(Pid::from_raw(-group), None) {
            Ok(()) | Err(Errno::EPERM) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(error) => Err(ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: error.to_string(),
            }),
        }
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

    #[cfg(unix)]
    fn signal_graceful(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let Some(group) = process.process_group else {
            return Err(ProcessError::MissingProcessGroup(process.id.clone()));
        };
        match killpg(Pid::from_raw(group), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: error.to_string(),
            }),
        }
    }

    #[cfg(windows)]
    fn signal_graceful(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        process
            .child
            .try_lock()
            .map_err(|_| ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: "child process is busy".to_owned(),
            })?
            .start_kill()
            .map_err(|error| ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: error.to_string(),
            })
    }

    #[cfg(unix)]
    fn signal_force(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let Some(group) = process.process_group else {
            return Err(ProcessError::MissingProcessGroup(process.id.clone()));
        };
        match killpg(Pid::from_raw(group), Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: error.to_string(),
            }),
        }
    }

    #[cfg(windows)]
    fn signal_force(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        process
            .child
            .try_lock()
            .map_err(|_| ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: "child process is busy".to_owned(),
            })?
            .start_kill()
            .map_err(|error| ProcessError::Signal {
                terminal_id: process.id.clone(),
                message: error.to_string(),
            })
    }
}

impl GitInspector for TerminalManager {
    fn inspect<'a>(&'a self, root: &'a Path) -> GitInspectorFuture<'a> {
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
                .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
            let output = self
                .wait(&terminal_id)
                .await
                .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
            let bytes = output
                .chunks
                .iter()
                .filter(|chunk| chunk.stream == ProcessStream::Stdout)
                .flat_map(|chunk| chunk.bytes.iter().copied())
                .collect::<Vec<_>>();
            let text = String::from_utf8(bytes)
                .map_err(|_| WorkspaceError::InvalidEncoding(PathBuf::from("git status")))?;
            let mut lines = text.lines();
            let header = lines.next().unwrap_or_default();
            let branch = header
                .strip_prefix("## ")
                .and_then(|value| value.split(['.', ' ']).next())
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let changed_paths = lines
                .filter_map(|line| line.get(3..))
                .map(str::to_owned)
                .collect();
            let _ = self.release(&terminal_id).await;
            Ok(GitState {
                branch,
                head: None,
                changed_paths,
            })
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ClientToolRequest {
    FilesystemRead {
        path: String,
    },
    FilesystemWrite {
        path: String,
        content: Vec<u8>,
    },
    TerminalCreate {
        request_id: String,
        command: String,
        cwd: String,
    },
    TerminalCancelCreate {
        request_id: String,
    },
    TerminalWait {
        terminal_id: String,
    },
    TerminalOutput {
        terminal_id: String,
    },
    TerminalKill {
        terminal_id: String,
    },
    TerminalRelease {
        terminal_id: String,
    },
}

impl ClientToolRequest {
    fn required_capability(&self) -> ClientToolCapability {
        match self {
            Self::FilesystemRead { .. } => ClientToolCapability::FilesystemRead,
            Self::FilesystemWrite { .. } => ClientToolCapability::FilesystemWrite,
            Self::TerminalCreate { .. }
            | Self::TerminalCancelCreate { .. }
            | Self::TerminalWait { .. }
            | Self::TerminalOutput { .. }
            | Self::TerminalKill { .. }
            | Self::TerminalRelease { .. } => ClientToolCapability::Terminal,
        }
    }
}

pub trait ClientToolPort: Send + Sync {
    fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a>;
}

#[derive(Clone)]
pub struct ClientToolIo {
    capabilities: BTreeSet<ClientToolCapability>,
    port: Arc<dyn ClientToolPort>,
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
    command: String,
    cwd: String,
    request_id: String,
    cancel: watch::Receiver<bool>,
    result_sender: oneshot::Sender<Result<Value, ToolIoError>>,
}

impl ClientToolIo {
    #[must_use]
    pub fn new(
        capabilities: impl IntoIterator<Item = ClientToolCapability>,
        port: Arc<dyn ClientToolPort>,
    ) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
            port,
            active_terminals: Arc::new(Mutex::new(BTreeSet::new())),
            lifecycles: Arc::new(Mutex::new(ClientLifecycles::default())),
            next_lifecycle: Arc::new(AtomicU64::new(1)),
            cleanup_failures: Arc::new(Mutex::new(Vec::new())),
            cleanup_grace: DEFAULT_CLEANUP_GRACE,
        }
    }

    pub async fn request(&self, request: ClientToolRequest) -> Result<Value, ToolIoError> {
        let required = request.required_capability();
        if !self.capabilities.contains(&required) {
            return Err(ToolIoError::CapabilityNotAdvertised(required));
        }
        self.port.request(request).await
    }

    pub async fn run_shell(&self, command: String, cwd: String) -> Result<Value, ToolIoError> {
        if !self.capabilities.contains(&ClientToolCapability::Terminal) {
            return Err(ToolIoError::CapabilityNotAdvertised(
                ClientToolCapability::Terminal,
            ));
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ToolIoError::RuntimeUnavailable)?;
        let port = self.port.clone();
        let active_terminals = self.active_terminals.clone();
        let cleanup_failures = self.cleanup_failures.clone();
        let cleanup_grace = self.cleanup_grace;
        let lifecycle_id = self.next_lifecycle.fetch_add(1, Ordering::Relaxed);
        let request_id = format!("terminal-create-{lifecycle_id}");
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
                command,
                cwd,
                request_id,
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
                        "lifecycle-{lifecycle_id}: cleanup is still awaiting client acknowledgement"
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
        command,
        cwd,
        request_id,
        mut cancel,
        mut result_sender,
    } = context;
    let created = port.request(ClientToolRequest::TerminalCreate {
        request_id: request_id.clone(),
        command,
        cwd,
    });
    tokio::pin!(created);
    let cancelled = tokio::select! {
        created = &mut created => {
            let created = match created {
                Ok(created) => created,
                Err(error) => {
                    let _ = result_sender.send(Err(error));
                    return;
                }
            };
            Some(created)
        }
        changed = cancel.changed() => {
            let _ = changed;
            None
        }
        () = result_sender.closed() => {
            None
        }
    };
    let created = match cancelled {
        Some(created) => created,
        None => {
            let cancellation = cleanup_client_request(
                &port,
                ClientToolRequest::TerminalCancelCreate {
                    request_id: request_id.clone(),
                },
                cleanup_grace,
            )
            .await;
            match cancellation {
                Ok(response) if response["cancelled"] == true => {
                    let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
                    return;
                }
                Ok(response) => {
                    if let Some(terminal_id) = response.get("terminalId").and_then(Value::as_str) {
                        cleanup_client_terminal(
                            &port,
                            terminal_id,
                            cleanup_grace,
                            &cleanup_failures,
                        )
                        .await;
                        let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
                        return;
                    }
                    cleanup_failures.lock().await.push(format!(
                        "{request_id}: client did not acknowledge terminal creation cancellation"
                    ));
                }
                Err(error) => {
                    cleanup_failures.lock().await.push(format!(
                        "{request_id}: terminal creation cancellation failed: {error}"
                    ));
                }
            }
            match created.await {
                Ok(created) => created,
                Err(error) => {
                    let _ = result_sender.send(Err(error));
                    return;
                }
            }
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
    if result_sender.is_closed() || *cancel.borrow() {
        cleanup_client_terminal(&port, &terminal_id, cleanup_grace, &cleanup_failures).await;
        active_terminals.lock().await.remove(&terminal_id);
        let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
        return;
    }
    let waited = tokio::select! {
        waited = port.request(ClientToolRequest::TerminalWait {
            terminal_id: terminal_id.clone(),
        }) => waited,
        () = result_sender.closed() => {
            cleanup_client_terminal(&port, &terminal_id, cleanup_grace, &cleanup_failures).await;
            active_terminals.lock().await.remove(&terminal_id);
            return;
        }
        changed = cancel.changed() => {
            let _ = changed;
            cleanup_client_terminal(&port, &terminal_id, cleanup_grace, &cleanup_failures).await;
            active_terminals.lock().await.remove(&terminal_id);
            let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
            return;
        }
    };
    if waited.is_err() {
        let killed = cleanup_client_request(
            &port,
            ClientToolRequest::TerminalKill {
                terminal_id: terminal_id.clone(),
            },
            cleanup_grace,
        )
        .await;
        if let Err(error) = killed {
            record_cleanup_failure(&cleanup_failures, &terminal_id, error).await;
        }
    }
    let output = match waited {
        Ok(_) => tokio::select! {
            output = port.request(ClientToolRequest::TerminalOutput {
                terminal_id: terminal_id.clone(),
            }) => output,
            () = result_sender.closed() => {
                cleanup_client_terminal(&port, &terminal_id, cleanup_grace, &cleanup_failures).await;
                active_terminals.lock().await.remove(&terminal_id);
                return;
            }
            changed = cancel.changed() => {
                let _ = changed;
                cleanup_client_terminal(&port, &terminal_id, cleanup_grace, &cleanup_failures).await;
                active_terminals.lock().await.remove(&terminal_id);
                let _ = result_sender.send(Err(ToolIoError::LifecycleCancelled));
                return;
            }
        },
        Err(error) => Err(error),
    };
    let release = cleanup_client_request(
        &port,
        ClientToolRequest::TerminalRelease {
            terminal_id: terminal_id.clone(),
        },
        cleanup_grace,
    )
    .await;
    active_terminals.lock().await.remove(&terminal_id);
    let result = match (output, release) {
        (Ok(output), Ok(_)) => {
            if output.get("signal").is_some_and(|signal| !signal.is_null()) {
                Err(ToolIoError::TerminalSignal)
            } else {
                Ok(output)
            }
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    let _ = result_sender.send(result);
}

async fn cleanup_client_terminal(
    port: &Arc<dyn ClientToolPort>,
    terminal_id: &str,
    cleanup_grace: Duration,
    cleanup_failures: &Arc<Mutex<Vec<String>>>,
) {
    for request in [
        ClientToolRequest::TerminalKill {
            terminal_id: terminal_id.to_owned(),
        },
        ClientToolRequest::TerminalRelease {
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
    #[error("client terminal exited from a signal")]
    TerminalSignal,
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

    #[derive(Default)]
    struct FakeClientPort {
        requests: StdMutex<Vec<ClientToolRequest>>,
        fail_wait: AtomicBool,
    }

    impl ClientToolPort for FakeClientPort {
        fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .map_err(|_| ToolIoError::Request("fake lock poisoned".to_owned()))?
                    .push(request.clone());
                match request {
                    ClientToolRequest::TerminalCreate { .. } => {
                        Ok(json!({"terminalId": "client-terminal-1"}))
                    }
                    ClientToolRequest::TerminalWait { .. }
                        if self.fail_wait.load(Ordering::Acquire) =>
                    {
                        Err(ToolIoError::Request("timeout".to_owned()))
                    }
                    ClientToolRequest::TerminalOutput { .. } => {
                        Ok(json!({"stdout": "ok", "signal": null}))
                    }
                    _ => Ok(json!({})),
                }
            })
        }
    }

    #[tokio::test]
    async fn client_tool_io_validates_capability_and_terminal_ordering() {
        let port = Arc::new(FakeClientPort::default());
        let io = ClientToolIo::new([ClientToolCapability::Terminal], port.clone());
        let output = io
            .run_shell("echo ok".to_owned(), "/work".to_owned())
            .await
            .expect("run");
        assert_eq!(output["stdout"], "ok");
        {
            let requests = port.requests.lock().expect("requests");
            assert!(matches!(
                requests[0],
                ClientToolRequest::TerminalCreate { .. }
            ));
            assert!(matches!(
                requests[1],
                ClientToolRequest::TerminalWait { .. }
            ));
            assert!(matches!(
                requests[2],
                ClientToolRequest::TerminalOutput { .. }
            ));
            assert!(matches!(
                requests[3],
                ClientToolRequest::TerminalRelease { .. }
            ));
        }
        assert!(matches!(
            io.request(ClientToolRequest::FilesystemRead {
                path: "secret".to_owned(),
            })
            .await,
            Err(ToolIoError::CapabilityNotAdvertised(
                ClientToolCapability::FilesystemRead
            ))
        ));
    }

    #[tokio::test]
    async fn client_timeout_kills_before_release() {
        let port = Arc::new(FakeClientPort::default());
        port.fail_wait.store(true, Ordering::Release);
        let io = ClientToolIo::new([ClientToolCapability::Terminal], port.clone());
        assert!(
            io.run_shell("blocked".to_owned(), "/work".to_owned())
                .await
                .is_err()
        );
        let requests = port.requests.lock().expect("requests");
        assert!(matches!(
            requests[2],
            ClientToolRequest::TerminalKill { .. }
        ));
        assert!(matches!(
            requests[3],
            ClientToolRequest::TerminalRelease { .. }
        ));
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
                    ClientToolRequest::TerminalCancelCreate { .. } => std::future::pending().await,
                    _ => Ok(json!({})),
                }
            })
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
    }

    #[tokio::test]
    async fn client_task_abort_kills_and_releases_created_terminal() {
        let port = Arc::new(BlockingClientPort::default());
        let io = ClientToolIo::new([ClientToolCapability::Terminal], port.clone());
        let task_io = io.clone();
        let task = tokio::spawn(async move {
            task_io
                .run_shell("blocked".to_owned(), "/work".to_owned())
                .await
        });
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
            ClientToolRequest::TerminalKill { terminal_id }
                if terminal_id == "client-terminal-cancelled"
        )));
        assert!(requests.iter().any(|request| matches!(
            request,
            ClientToolRequest::TerminalRelease { terminal_id }
                if terminal_id == "client-terminal-cancelled"
        )));
    }

    #[tokio::test]
    async fn client_cleanup_keeps_unacknowledged_terminal_creation_tracked() {
        let port = Arc::new(BlockingCreatePort::default());
        let mut io = ClientToolIo::new([ClientToolCapability::Terminal], port.clone());
        io.cleanup_grace = Duration::from_millis(20);
        let task_io = io.clone();
        let task = tokio::spawn(async move {
            task_io
                .run_shell("blocked".to_owned(), "/work".to_owned())
                .await
        });
        port.entered.notified().await;

        assert!(matches!(
            io.cleanup_all().await,
            Err(ToolIoError::CleanupFailures(failures))
                if failures.iter().any(|failure| failure.contains("acknowledgement"))
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
            ClientToolRequest::TerminalKill { terminal_id }
                if terminal_id == "late-client-terminal"
        )));
        assert!(requests.iter().any(|request| matches!(
            request,
            ClientToolRequest::TerminalRelease { terminal_id }
                if terminal_id == "late-client-terminal"
        )));
    }
}
