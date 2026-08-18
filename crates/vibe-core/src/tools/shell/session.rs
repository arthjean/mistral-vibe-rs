//! A managed session: a process that outlives the call that started it.
//!
//! The legacy variant runs one command to completion and answers with what it
//! printed. A managed one starts a process the model then polls, feeds and
//! kills through the family's four session tools, so the state has to survive
//! the call: a log on disk the reader pumps into, a manifest beside it so a
//! later process can describe a session this one left running, and an in-memory
//! entry while the terminal is still open.
//!
//! [`SessionHandle`] is what keeps the two lives of a session from drifting.
//! Reference answers `read_output`, `inspect_session`, `info` and
//! `list_sessions` from one `SessionInfo`, built from the live session or
//! validated from the manifest, so every tool that describes a session here
//! reads the same value whichever life it is in.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::policy::{ApprovalAgent, PermissionContext, PermissionStore, PolicyGuardedTool};
use crate::process::{ProcessChunk, TerminalManager, TerminalState};
use crate::shell::ShellConfig;
use crate::tools::config::{
    ShellCommandConfig, ShellInlineConfig, ShellOutputConfig, ToolConfigResolver,
};
use crate::tools::{
    OwnedToolHandlerFuture, ToolError, ToolExecutionOutput, ToolHandler, ToolInvocation,
    ToolOutputSink,
};

use super::decode::read_file_window;
use super::host::{ShellFamily, windows_shell_arguments};
use super::{
    PUMP_INTERVAL, SESSIONS_DIRECTORY, byte_limit, command_argument, exit_status,
    is_family_session_id, process_error, process_spec, string_argument, timeout_argument,
};

/// One Vibe session's shell state: the terminals it opened and the managed
/// sessions still addressable by the model.
pub(super) struct SessionShell {
    pub(super) family: ShellFamily,
    pub(super) terminals: TerminalManager,
    pub(super) managed: Mutex<BTreeMap<String, Arc<ManagedSession>>>,
    /// The sessions a previous process left behind, by id, holding the manifest
    /// each one wrote before its client stopped. Guarded by a blocking lock
    /// because the family loads them while it is being constructed, which is
    /// synchronous.
    pub(super) orphaned: StdMutex<BTreeMap<String, Value>>,
    pub(super) log_root: PathBuf,
}

impl SessionShell {
    pub(super) fn sessions_directory(&self) -> PathBuf {
        self.log_root.join(SESSIONS_DIRECTORY)
    }

    /// Reads the manifests this family owns and records what they describe.
    ///
    /// Reference `_load_orphaned_manifests` rewrites a manifest that still says
    /// `running`, because the process that would have settled it is gone. A
    /// manifest that cannot be read is skipped rather than failing the load, so
    /// one corrupt file never hides the sessions beside it.
    pub(super) fn load_orphaned_manifests(&self) {
        let directory = self.sessions_directory();
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return;
        };
        let mut orphaned = BTreeMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(mut metadata) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .filter(Value::is_object)
            else {
                continue;
            };
            let Some(id) = metadata
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| is_family_session_id(self.family, id))
                .map(str::to_owned)
            else {
                continue;
            };
            if metadata.get("status").and_then(Value::as_str)
                == Some(SessionStatus::Running.as_str())
            {
                metadata["status"] = Value::String(SessionStatus::Orphaned.as_str().to_owned());
                metadata["updatedAtMs"] = Value::String(now_ms().to_string());
                if let Ok(rendered) = serde_json::to_vec_pretty(&metadata) {
                    let _ = std::fs::write(&path, rendered);
                }
            }
            orphaned.insert(id, metadata);
        }
        if let Ok(mut store) = self.orphaned.lock() {
            *store = orphaned;
        }
    }

    /// The manifest recorded for `session_id`, if it names an orphan.
    pub(super) fn orphan(&self, session_id: &str) -> Option<Value> {
        self.orphaned
            .lock()
            .ok()
            .and_then(|store| store.get(session_id).cloned())
    }

    pub(super) fn orphans(&self) -> Vec<Value> {
        self.orphaned
            .lock()
            .map(|store| store.values().cloned().collect())
            .unwrap_or_default()
    }

    pub(super) fn forget_orphan(&self, session_id: &str) {
        if let Ok(mut store) = self.orphaned.lock() {
            store.remove(session_id);
        }
    }
}

// --------------------------------------------------------------------------
// Managed sessions
// --------------------------------------------------------------------------

/// Reference `Status`, the states a managed session reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionStatus {
    Running,
    Completed,
    Killed,
    TimedOut,
    /// Reference `orphaned`: a session a previous process left behind, whose
    /// manifest and log survive it while the terminal that produced them does
    /// not.
    Orphaned,
}

impl SessionStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Orphaned => "orphaned",
        }
    }
}

#[derive(Debug)]
pub(super) struct SessionState {
    status: SessionStatus,
    exit_code: Option<i32>,
    backpressure_dropped: bool,
    updated_at_ms: u128,
}

pub(super) struct ManagedSession {
    pub(super) id: String,
    pub(super) terminal_id: String,
    pub(super) command: String,
    working_directory: String,
    shell: String,
    pub(super) log_path: PathBuf,
    pub(super) manifest_path: PathBuf,
    created_at_ms: u128,
    /// Reference `SessionInfo.pty_backend`, absent when the host provided no
    /// terminal and the session fell back to pipes.
    pty_backend: Option<&'static str>,
    /// Reference `SessionInfo.reader_error`, which this port also uses to name
    /// the reason a terminal could not be opened.
    reader_error: Option<String>,
    state: StdMutex<SessionState>,
}

impl ManagedSession {
    pub(super) fn snapshot(&self) -> (SessionStatus, Option<i32>, bool) {
        self.state
            .lock()
            .map_or((SessionStatus::Running, None, false), |state| {
                (state.status, state.exit_code, state.backpressure_dropped)
            })
    }

    pub(super) fn info(&self) -> Value {
        let (status, exit_code, dropped) = self.snapshot();
        let updated_at_ms = self
            .state
            .lock()
            .map_or(self.created_at_ms, |state| state.updated_at_ms);
        json!({
            "sessionId": self.id,
            "command": self.command,
            "cwd": self.working_directory,
            "shell": self.shell,
            "ptyBackend": self.pty_backend,
            "status": status.as_str(),
            "exitCode": exit_code,
            "outputPath": self.log_path.to_string_lossy(),
            "createdAtMs": self.created_at_ms.to_string(),
            "updatedAtMs": updated_at_ms.to_string(),
            "backpressureDropped": dropped,
            "readerError": self.reader_error,
        })
    }

    pub(super) fn is_running(&self) -> bool {
        self.snapshot().0 == SessionStatus::Running
    }

    pub(super) fn settle(&self, status: SessionStatus, exit_code: Option<i32>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status;
            state.exit_code = exit_code;
            state.updated_at_ms = now_ms();
        }
        self.save_manifest();
    }

    /// Records what a later process needs to report this session as orphaned.
    ///
    /// Reference `_save_manifest` writes it beside the log at every state
    /// change, so a client that dies between two of them still leaves a
    /// manifest describing the session as it last was.
    pub(super) fn save_manifest(&self) {
        let Ok(rendered) = serde_json::to_vec_pretty(&self.info()) else {
            return;
        };
        let _ = std::fs::write(&self.manifest_path, rendered);
    }
}

/// The wall-clock milliseconds a session records its transitions at.
pub(super) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default()
}

pub(super) async fn run_managed_command(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let requested_directory = string_argument(arguments, "cwd")
        .map_or_else(|| working_directory.to_path_buf(), PathBuf::from);
    let mut config = config.clone();
    if let Some(executable) = string_argument(arguments, "shell") {
        config.executable = PathBuf::from(executable);
        // Reference `build_windows_shell_argv` derives the argument form from
        // the executable it was handed, so an override carries its own flags
        // rather than the ones the family resolved. The POSIX family has no
        // such rule: its arguments stay whatever the session resolved.
        if shell.family != ShellFamily::Bash {
            config.arguments = windows_shell_arguments(&config.executable);
        }
    }
    let session = start_managed_session(
        shell,
        &config,
        &requested_directory,
        &command,
        arguments.get("env"),
        settings.max_output_bytes,
    )
    .await?;
    // One window and one handle for every exit of this call: the inline budget
    // is read once so the four answers below cannot drift apart.
    let limit = byte_limit(arguments, output, settings.max_inline_bytes);
    let handle = SessionHandle::Live(session.clone());
    let background = arguments["background"].as_bool().unwrap_or(false);
    if background {
        return session_output(&handle, 0, limit);
    }
    let hard_timeout =
        arguments["hard_timeout"].as_bool().unwrap_or(false) || arguments["timeout"].is_u64();
    let timeout = timeout_argument(arguments, settings);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while session.is_running() && Instant::now() < deadline {
        tokio::time::sleep(PUMP_INTERVAL).await;
    }
    if session.is_running() {
        if !hard_timeout {
            // A soft timeout leaves the session running: the model polls it
            // with the family's output tool instead of losing the work.
            return session_output(&handle, 0, limit);
        }
        kill_managed_session(shell, &session, SessionStatus::TimedOut).await?;
        let rendered = session_output(&handle, 0, limit)?;
        return Err(ToolError::Execution(format!(
            "the command timed out after {timeout}s and its process group was terminated: \
             `{command}`\nsession_id: {}\noutput:\n{}",
            session.id, rendered.model_text
        )));
    }
    let rendered = session_output(&handle, 0, limit)?;
    let status = rendered.typed_result["exitCode"].as_i64().unwrap_or(0);
    if status != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {status}: `{command}`\nsession_id: {}\noutput:\n{}",
            session.id, rendered.model_text
        )));
    }
    Ok(rendered)
}

pub(super) async fn start_managed_session(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: Option<&Value>,
    max_output_bytes: usize,
) -> Result<Arc<ManagedSession>, ToolError> {
    if !working_directory.is_dir() {
        return Err(ToolError::Execution(format!(
            "`{}` is not a directory",
            working_directory.display()
        )));
    }
    let sessions_directory = shell.sessions_directory();
    std::fs::create_dir_all(&sessions_directory).map_err(|error| {
        ToolError::Execution(format!(
            "the session log directory `{}` cannot be created: {error}",
            sessions_directory.display()
        ))
    })?;
    let id = new_session_id(shell.family);
    let log_path = sessions_directory.join(format!("{id}.log"));
    let manifest_path = sessions_directory.join(format!("{id}.json"));
    std::fs::write(&log_path, b"").map_err(|error| {
        ToolError::Execution(format!(
            "the session log `{}` cannot be created: {error}",
            log_path.display()
        ))
    })?;
    let terminal_id = shell
        .terminals
        .run(process_spec(
            shell.family,
            config,
            working_directory,
            command,
            environment,
            max_output_bytes,
            true,
        ))
        .await
        .map_err(process_error)?;
    let backend = shell
        .terminals
        .backend(&terminal_id)
        .await
        .unwrap_or_default();
    let created_at_ms = now_ms();
    let session = Arc::new(ManagedSession {
        id,
        terminal_id,
        command: command.to_owned(),
        working_directory: working_directory.to_string_lossy().into_owned(),
        shell: config.executable.to_string_lossy().into_owned(),
        log_path,
        manifest_path,
        created_at_ms,
        pty_backend: backend.pty,
        reader_error: backend.degraded,
        state: StdMutex::new(SessionState {
            status: SessionStatus::Running,
            exit_code: None,
            backpressure_dropped: false,
            updated_at_ms: created_at_ms,
        }),
    });
    session.save_manifest();
    // A session id that was orphaned by a previous process and is now live
    // again answers from the live entry rather than from the manifest.
    shell.forget_orphan(&session.id);
    shell
        .managed
        .lock()
        .await
        .insert(session.id.clone(), session.clone());
    spawn_pump(shell.terminals.clone(), session.clone());
    Ok(session)
}

/// Drains a session's terminal into its log until the process exits.
///
/// The terminal queue is bounded, so nothing but a reader draining it keeps a
/// chatty background command from losing output. The log is the cursor's source
/// of truth, which is what lets the output and log-file tools answer for a
/// session long after it exited.
pub(super) fn spawn_pump(terminals: TerminalManager, session: Arc<ManagedSession>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let Ok(read) = terminals.read(&session.terminal_id).await else {
                session.settle(SessionStatus::Killed, None);
                return;
            };
            append_chunks(&session, &read.chunks, read.backpressure_dropped);
            if !matches!(read.state, TerminalState::Running) {
                if let Ok(final_read) = terminals.wait(&session.terminal_id).await {
                    append_chunks(
                        &session,
                        &final_read.chunks,
                        final_read.backpressure_dropped,
                    );
                    if session.is_running() {
                        session.settle(
                            SessionStatus::Completed,
                            Some(exit_status(&final_read.state)),
                        );
                    }
                }
                // The output is captured, so the child is reaped now rather
                // than waiting for the session to be killed or closed.
                let _ = terminals.release(&session.terminal_id).await;
                return;
            }
            tokio::time::sleep(PUMP_INTERVAL).await;
        }
    });
}

pub(super) fn append_chunks(session: &ManagedSession, chunks: &[ProcessChunk], dropped: bool) {
    if dropped && let Ok(mut state) = session.state.lock() {
        state.backpressure_dropped = true;
    }
    if chunks.is_empty() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .open(&session.log_path)
    else {
        return;
    };
    let mut ordered = chunks.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|chunk| chunk.cursor);
    for chunk in ordered {
        let _ = file.write_all(&chunk.bytes);
    }
}

/// A session id for `family`.
///
/// Reference `TerminalSessionManager.session_prefix` is per family, and the
/// families share one session directory, so the prefix is what keeps one
/// family's tools from reading, feeding or killing another's session.
pub(super) fn new_session_id(family: ShellFamily) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let mut suffix = [0_u8; 4];
    // A collision would let one session read another's log, so the id carries
    // real entropy rather than a counter.
    if getrandom::fill(&mut suffix).is_err() {
        suffix = (stamp as u32).to_le_bytes();
    }
    format!(
        "{}_{stamp}_{}",
        family.name(),
        suffix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Wraps one managed-family handler, which all share the same shape: a session
/// store, the call arguments, and the turn's output budget.
/// What a session tool reads off its own `tools.<name>` entry.
///
/// The four session tools declare different subsets: `_stdin` declares neither
/// limit, `_output` declares both, and `_sessions` and `_log_file` only the
/// read window. One value carries all of them so the four handlers share a
/// signature; a tool that declares neither reads its base declaration and uses
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SessionLimits {
    pub(super) max_inline_bytes: usize,
    pub(super) max_poll_seconds: f64,
}

impl SessionLimits {
    pub(super) fn resolve(config: &ToolConfigResolver, tool: &str) -> Self {
        let inline: ShellInlineConfig = config.view(tool);
        let polling: ShellOutputConfig = config.view(tool);
        Self {
            max_inline_bytes: inline.max_inline_bytes,
            max_poll_seconds: polling.max_poll_seconds,
        }
    }
}

pub(super) fn session_handler<F, Fut>(
    shell: Arc<SessionShell>,
    config: ToolConfigResolver,
    tool: String,
    run: F,
) -> Arc<dyn ToolHandler>
where
    F: Fn(Arc<SessionShell>, Value, ToolOutputSink, SessionLimits) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'static,
{
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let limits = SessionLimits::resolve(&config, &tool);
            let future = run(shell.clone(), invocation.arguments.clone(), output, limits);
            Box::pin(future)
        },
    )
}

/// Wraps a session tool in the permission guard, which is what reads its
/// configured permission: the three polling tools produce no requirement of
/// their own, so the configuration is the whole decision.
pub(super) fn guarded_session(
    tool: String,
    policy: &PermissionStore,
    approval: &Arc<dyn ApprovalAgent>,
    inner: Arc<dyn ToolHandler>,
) -> Arc<dyn ToolHandler> {
    Arc::new(PolicyGuardedTool::new(
        tool,
        policy.clone(),
        approval.clone(),
        Arc::new(|_invocation| Ok(PermissionContext::deferred())),
        inner,
    ))
}

/// The session `session_id` names, live or left behind.
///
/// Reference `_live_session` separates the two: a live session is driven, an
/// orphaned one is only described, and an id that is neither is unknown. The
/// three answers are distinct here for the same reason, so a tool that needs a
/// process says so rather than reporting the id as missing.
pub(super) enum SessionHandle {
    Live(Arc<ManagedSession>),
    Orphaned(Value),
}

impl SessionHandle {
    /// The manifest shape both sides publish.
    ///
    /// Reference `read_output`, `inspect_session`, `info` and `list_sessions`
    /// all answer with one `SessionInfo`, built from the live session by
    /// `_session_info_locked` and from the manifest by `_info_from_manifest`,
    /// which validates it verbatim. So an orphan reports the status its own
    /// process last recorded rather than one this side invents, and every tool
    /// that describes a session reads the same value.
    pub(super) fn info(&self) -> Value {
        match self {
            Self::Live(session) => session.info(),
            Self::Orphaned(manifest) => manifest.clone(),
        }
    }

    /// The log the session wrote, which outlives the process that wrote it.
    pub(super) fn log_path(&self) -> PathBuf {
        match self {
            Self::Live(session) => session.log_path.clone(),
            Self::Orphaned(manifest) => PathBuf::from(
                manifest
                    .get("outputPath")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
        }
    }

    /// Whether more bytes are still coming, which is what decides a cut
    /// character is held back. An orphan's terminal is gone, and
    /// [`SessionShell::load_orphaned_manifests`] already rewrote any manifest
    /// that still said `running`, so it never answers `true`.
    pub(super) fn is_running(&self) -> bool {
        matches!(self, Self::Live(session) if session.is_running())
    }
}

pub(super) async fn session_handle(
    shell: &SessionShell,
    session_id: &str,
) -> Result<SessionHandle, ToolError> {
    let sessions = shell.managed.lock().await;
    if let Some(session) = sessions.get(session_id) {
        return Ok(SessionHandle::Live(session.clone()));
    }
    drop(sessions);
    if let Some(manifest) = shell.orphan(session_id) {
        return Ok(SessionHandle::Orphaned(manifest));
    }
    let active = shell
        .managed
        .lock()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let listed = if active.is_empty() {
        "none".to_owned()
    } else {
        active.join(", ")
    };
    Err(ToolError::Execution(format!(
        "unknown session `{session_id}`; active sessions: {listed}"
    )))
}

pub(super) async fn managed_session(
    shell: &SessionShell,
    session_id: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    match session_handle(shell, session_id).await? {
        SessionHandle::Live(session) => Ok(session),
        SessionHandle::Orphaned(_) => Err(ToolError::Execution(format!(
            "session `{session_id}` was left running by a previous process and has no live \
             terminal; read its log instead"
        ))),
    }
}

/// Reads one session's log from `cursor` and reports where the next read starts.
///
/// A live session and one a previous process left behind answer the same keys,
/// read off the same [`SessionHandle::info`], so the two cannot drift apart.
pub(super) fn session_output(
    handle: &SessionHandle,
    cursor: u64,
    limit: usize,
) -> Result<ToolExecutionOutput, ToolError> {
    let info = handle.info();
    let log_path = handle.log_path();
    let (output, next_cursor, truncated) =
        read_file_window(&log_path, cursor, limit, handle.is_running())?;
    let field = |key: &str| info.get(key).cloned().unwrap_or(Value::Null);
    let command = info
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let dropped = info
        .get("backpressureDropped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut model_text = output.clone();
    if truncated {
        model_text.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    if dropped {
        model_text.push_str("\n[output was dropped while the session outran its buffer]");
    }
    Ok(ToolExecutionOutput::new(model_text)
        .displayed_as(json!({"kind": "shell", "command": command}))
        .typed(json!({
            "sessionId": field("sessionId"),
            "command": command,
            "status": field("status"),
            "exitCode": field("exitCode"),
            "output": output,
            "nextCursor": next_cursor,
            "truncated": truncated,
            "outputPath": log_path.to_string_lossy(),
            "backpressureDropped": dropped,
        })))
}

pub(super) async fn kill_managed_session(
    shell: &SessionShell,
    session: &ManagedSession,
    status: SessionStatus,
) -> Result<(), ToolError> {
    let read = shell.terminals.interrupt(&session.terminal_id).await;
    let exit_code = match read {
        Ok(read) => {
            append_chunks(session, &read.chunks, read.backpressure_dropped);
            Some(exit_status(&read.state))
        }
        // A terminal the pump already released is a session that exited on its
        // own; its status is whatever the pump recorded.
        Err(_) => session.snapshot().1,
    };
    let _ = shell.terminals.release(&session.terminal_id).await;
    session.settle(status, exit_code);
    Ok(())
}
