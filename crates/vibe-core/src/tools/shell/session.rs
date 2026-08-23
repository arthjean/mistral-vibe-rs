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
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::UtcTimestamp;
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
use super::document::Document;
use super::host::{ShellFamily, windows_shell_arguments};
use super::{
    PUMP_INTERVAL, SESSIONS_DIRECTORY, command_argument, exit_status, is_family_session_id,
    process_error, process_spec, string_argument, timeout_argument,
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
                .get("session_id")
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
                metadata["updated_at"] = Value::String(now_iso());
                if let Err(error) = write_manifest(&path, &metadata) {
                    // Reference `_load_orphaned_manifests` swallows the write
                    // and keeps the record it read, so one unwritable manifest
                    // never hides the orphans beside it. The reason is carried
                    // on the record the scan answers with rather than lost.
                    metadata["reader_error"] = Value::String(error);
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

/// What `reader_error` names when this port's reader had to drop output rather
/// than fall behind the process producing it.
pub(super) const DROPPED_OUTPUT: &str = "output was dropped while the session outran its buffer";

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
    updated_at: String,
}

pub(super) struct ManagedSession {
    pub(super) id: String,
    pub(super) terminal_id: String,
    pub(super) command: String,
    working_directory: String,
    shell: String,
    pub(super) log_path: PathBuf,
    pub(super) manifest_path: PathBuf,
    /// Reference `SessionInfo.created_at`, which `_session_info_locked` renders
    /// through `_now_iso` rather than publishing the epoch seconds it holds.
    created_at: String,
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

    /// Reference `SessionInfo`: eleven snake_case fields, the two stamps
    /// rendered as ISO-8601 instants in UTC, and the two optional ones present
    /// and null rather than omitted.
    pub(super) fn info(&self) -> Value {
        let (status, exit_code, dropped) = self.snapshot();
        let updated_at = self.state.lock().map_or_else(
            |_| self.created_at.clone(),
            |state| state.updated_at.clone(),
        );
        // The reference declares no field for a reader that fell behind: its
        // own reader reports every failure through `reader_error`, so output
        // this session had to drop is named there rather than through a field
        // the reference does not publish.
        let reader_error = self
            .reader_error
            .clone()
            .or_else(|| dropped.then(|| DROPPED_OUTPUT.to_owned()));
        json!({
            "session_id": self.id,
            "command": self.command,
            "cwd": self.working_directory,
            "shell": self.shell,
            "pty_backend": self.pty_backend,
            "status": status.as_str(),
            "exit_code": exit_code,
            "output_path": self.log_path.to_string_lossy(),
            "created_at": self.created_at,
            "updated_at": updated_at,
            "reader_error": reader_error,
        })
    }

    pub(super) fn is_running(&self) -> bool {
        self.snapshot().0 == SessionStatus::Running
    }

    pub(super) fn settle(&self, status: SessionStatus, exit_code: Option<i32>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status;
            state.exit_code = exit_code;
            state.updated_at = now_iso();
        }
        self.save_manifest();
    }

    /// Records what a later process needs to report this session as orphaned.
    ///
    /// Reference `_save_manifest` writes it beside the log at every state
    /// change, so a client that dies between two of them still leaves a
    /// manifest describing the session as it last was.
    pub(super) fn save_manifest(&self) {
        let _ = write_manifest(&self.manifest_path, &self.info());
    }
}

/// Writes one manifest the way reference `_save_manifest` writes it:
/// `json.dumps(metadata, indent=2, sort_keys=True)`, no trailing newline.
///
/// This workspace builds `serde_json` without `preserve_order`, so a map is
/// sorted by key and the pretty writer indents by two spaces with `": "`
/// between a key and its value, which is what Python emits for those two
/// arguments. The one difference is escaping: Python's default `ensure_ascii`
/// writes a non-ASCII character as `\uXXXX` where this writes it as UTF-8, and
/// both parse back to the same string.
fn write_manifest(path: &Path, metadata: &Value) -> Result<(), String> {
    let rendered = serde_json::to_vec_pretty(metadata).map_err(|error| error.to_string())?;
    std::fs::write(path, rendered).map_err(|error| error.to_string())
}

/// Reference `_now_iso`: the current instant in UTC, rendered the way Python's
/// `datetime.isoformat` renders it.
pub(super) fn now_iso() -> String {
    UtcTimestamp::now().to_iso8601()
}

pub(super) async fn run_managed_command(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
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
    // One window and one handle for every exit of this call, so the four
    // answers below cannot drift apart.
    //
    // Reference `ExperimentalBash.run` bounds every one of its reads with
    // `self.config.max_output_bytes`, where the three polling tools bound theirs
    // with `max_inline_bytes`. The two defaults differ, 16 000 against 30 000,
    // so a command that prints 20 000 bytes truncates here and does not through
    // a poll. Nothing narrows it further: this call publishes no `max_bytes`
    // argument, and the turn's streaming budget bounds what a tool emits, which
    // a managed command never does.
    let limit = settings.max_output_bytes;
    let handle = SessionHandle::Live(session.clone());
    let background = arguments["background"].as_bool().unwrap_or(false);
    if background {
        let (document, display) = managed_command_document(&handle, true, limit)?;
        return Ok(document.into_output(display));
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
            // A soft timeout leaves the session running, and the reference
            // reports it as a backgrounded one: the model polls it with the
            // family's output tool instead of losing the work.
            let (document, display) = managed_command_document(&handle, true, limit)?;
            return Ok(document.into_output(display));
        }
        kill_managed_session(shell, &session, SessionStatus::TimedOut).await?;
        let (document, _) = managed_command_document(&handle, false, limit)?;
        return Err(ToolError::Execution(format!(
            "the command timed out after {timeout}s and its process group was terminated: \
             `{command}`\nsession_id: {}\noutput:\n{}",
            session.id,
            document.model_text()
        )));
    }
    let (document, display) = managed_command_document(&handle, false, limit)?;
    let code = document
        .get("returncode")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    // Reference `_result_from_session` succeeds only when both halves agree:
    // the session reached `completed` and its return code is zero. A session
    // that was killed on its way out carries whatever code the kill produced,
    // which is zero often enough that the code alone would report it as a
    // success.
    let settled = document
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status == SessionStatus::Completed.as_str());
    if !settled || code != 0 {
        let reported = document
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or(SessionStatus::Running.as_str())
            .to_owned();
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {code}: `{command}`\nsession_id: {}\nstatus: \
             {reported}\noutput:\n{}",
            session.id,
            document.model_text()
        )));
    }
    Ok(document.into_output(display))
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
    let created_at = now_iso();
    let session = Arc::new(ManagedSession {
        id,
        terminal_id,
        command: command.to_owned(),
        working_directory: working_directory.to_string_lossy().into_owned(),
        shell: config.executable.to_string_lossy().into_owned(),
        log_path,
        manifest_path,
        created_at: created_at.clone(),
        pty_backend: backend.pty,
        reader_error: backend.degraded,
        state: StdMutex::new(SessionState {
            status: SessionStatus::Running,
            exit_code: None,
            backpressure_dropped: false,
            updated_at: created_at,
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
    let now = UtcTimestamp::now();
    let mut suffix = [0_u8; 4];
    // Reference `_new_session_id` takes its eight hexadecimal characters from
    // `uuid4().hex[:8]`, so the stamp is not what separates two sessions minted
    // inside the same second: a collision would let one read another's log, and
    // the suffix carries real entropy rather than a counter.
    if getrandom::fill(&mut suffix).is_err() {
        suffix = (now.micros_since_epoch() as u32).to_le_bytes();
    }
    format!(
        "{}_{}_{}",
        family.name(),
        compact_stamp(now),
        suffix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Reference `_new_session_id` stamps `datetime.now(tz=UTC).strftime`
/// `"%Y%m%d_%H%M%S"`. That is the ISO instant the manifest already carries with
/// its separators removed, so the two spellings are derived from one value and
/// cannot name different seconds.
fn compact_stamp(instant: UtcTimestamp) -> String {
    let iso = instant.to_iso8601();
    let slice = |range: std::ops::Range<usize>| iso.get(range).unwrap_or_default();
    format!(
        "{}{}{}_{}{}{}",
        slice(0..4),
        slice(5..7),
        slice(8..10),
        slice(11..13),
        slice(14..16),
        slice(17..19)
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
                    .get("output_path")
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

/// One session's identity and state, in the order reference `SessionInfo`
/// declares its fields.
///
/// A live session and one a previous process left behind are described from the
/// same [`SessionHandle::info`], so the two cannot drift apart, and the order is
/// applied here rather than at each of the tools that embed a session.
pub(super) fn session_document(info: &Value) -> Document {
    let field = |key: &str| info.get(key).cloned().unwrap_or(Value::Null);
    Document::new()
        .field("session_id", field("session_id"))
        .field("command", field("command"))
        .field("cwd", field("cwd"))
        .field("shell", field("shell"))
        .field("pty_backend", field("pty_backend"))
        .field("status", field("status"))
        .field("exit_code", field("exit_code"))
        .field("output_path", field("output_path"))
        .field("created_at", field("created_at"))
        .field("updated_at", field("updated_at"))
        .field("reader_error", field("reader_error"))
}

/// One read of a session's log, and the session it was read from.
struct SessionWindow {
    info: Value,
    log_path: PathBuf,
    output: String,
    next_cursor: u64,
    truncated: bool,
}

impl SessionWindow {
    fn read(handle: &SessionHandle, cursor: u64, limit: usize) -> Result<Self, ToolError> {
        let log_path = handle.log_path();
        let (output, next_cursor, truncated) =
            read_file_window(&log_path, cursor, limit, handle.is_running())?;
        Ok(Self {
            info: handle.info(),
            log_path,
            output,
            next_cursor,
            truncated,
        })
    }

    fn field(&self, key: &str) -> Value {
        self.info.get(key).cloned().unwrap_or(Value::Null)
    }

    fn command(&self) -> String {
        self.field("command")
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn display(&self) -> Value {
        json!({"kind": "shell", "command": self.command()})
    }
}

/// What the family's command tool answers once a session has been started.
///
/// Reference `_result_from_session` reads the session once and fills the whole
/// result from that one read: `output` is what the log held, `stdout` is the
/// same bytes with the terminal's line endings normalized, `stderr` is empty
/// because a managed session multiplexes both streams onto one log, and
/// `returncode` falls back to zero while `exit_code` stays null for a session
/// that has not exited.
pub(super) fn managed_command_document(
    handle: &SessionHandle,
    background: bool,
    limit: usize,
) -> Result<(Document, Value), ToolError> {
    let window = SessionWindow::read(handle, 0, limit)?;
    let exit_code = window.field("exit_code");
    let document = Document::new()
        .field("command", window.command())
        .field("session_id", window.field("session_id"))
        .field("status", window.field("status"))
        .field("exit_code", exit_code.clone())
        .field("shell", window.field("shell"))
        .field("background", background)
        .field("output", window.output.clone())
        .field("next_cursor", window.next_cursor)
        .field("truncated", window.truncated)
        .field(
            "output_path",
            window.log_path.to_string_lossy().into_owned(),
        )
        .field("stdout", window.output.replace("\r\n", "\n"))
        .field("stderr", "")
        .field("returncode", exit_code.as_i64().unwrap_or(0));
    Ok((document, window.display()))
}

/// What the family's output tool answers, which is the session's state and the
/// window it just read rather than anything about the call that started it.
pub(super) fn session_poll_document(
    handle: &SessionHandle,
    cursor: u64,
    limit: usize,
) -> Result<(Document, Value), ToolError> {
    let window = SessionWindow::read(handle, cursor, limit)?;
    let document = Document::new()
        .field("session_id", window.field("session_id"))
        .field("status", window.field("status"))
        .field("exit_code", window.field("exit_code"))
        .field("output", window.output.clone())
        .field("next_cursor", window.next_cursor)
        .field("truncated", window.truncated)
        .field(
            "output_path",
            window.log_path.to_string_lossy().into_owned(),
        );
    Ok((document, window.display()))
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
