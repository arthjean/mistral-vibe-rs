//! A managed session: a process that outlives the call that started it.
//!
//! The legacy variant runs one command to completion and answers with what it
//! printed. A managed one starts a process the model then polls, feeds and
//! kills through the family's four session tools, so the state has to survive
//! the call: a log on disk the reader pumps into, a manifest beside it so a
//! later process can describe a session this one left running, and an in-memory
//! entry while the terminal is still open.
//!
//! What the tools say about a session, in either life, is
//! [`super::session_info`]'s.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::policy::{ApprovalAgent, PermissionContext, PermissionStore, PolicyGuardedTool};
use crate::process::{ProcessChunk, TerminalManager, TerminalState};
use crate::pycompat::{self, Dumps};
use crate::shell::ShellConfig;
use crate::sync;
use crate::time::UtcTimestamp;
use crate::tools::config::{
    ShellCommandConfig, ShellInlineConfig, ShellOutputConfig, ToolConfigResolver,
};
use crate::tools::{
    OwnedToolHandlerFuture, ToolError, ToolExecutionOutput, ToolHandler, ToolInvocation,
    ToolOutputSink,
};

use super::decode::read_file_window;
use super::host::{ShellFamily, resolve_session_shell};
use super::policy::{ShellCallArguments, render_seconds};
use super::session_info::{SessionHandle, SessionInfo, SessionWindow};
use super::{
    PUMP_INTERVAL, SESSIONS_DIRECTORY, exit_status, is_family_session_id, process_error,
    process_spec,
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
    /// Variables every command this session runs starts from, under the
    /// family's own and a call's overrides: the library search paths a
    /// unified session's plugins publish.
    pub(super) base_environment: Arc<StdMutex<BTreeMap<String, String>>>,
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
        *sync::lock(&self.orphaned) = orphaned;
    }

    /// The manifest recorded for `session_id`, if it names an orphan.
    pub(super) fn orphan(&self, session_id: &str) -> Option<Value> {
        sync::lock(&self.orphaned).get(session_id).cloned()
    }

    /// Every orphan's manifest, by session id.
    pub(super) fn orphans(&self) -> Vec<(String, Value)> {
        sync::lock(&self.orphaned)
            .iter()
            .map(|(id, manifest)| (id.clone(), manifest.clone()))
            .collect()
    }

    pub(super) fn forget_orphan(&self, session_id: &str) {
        sync::lock(&self.orphaned).remove(session_id);
    }
}

// --------------------------------------------------------------------------
// Managed sessions
// --------------------------------------------------------------------------

/// What `reader_error` names when this port's reader had to drop output rather
/// than fall behind the process producing it.
pub(super) const DROPPED_OUTPUT: &str = "output was dropped while the session outran its buffer";

/// Reference `Status`, the states a managed session reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
        let state = sync::lock(&self.state);
        (state.status, state.exit_code, state.backpressure_dropped)
    }

    /// Reference `_session_info_locked`.
    pub(super) fn info(&self) -> SessionInfo {
        let (status, exit_code, dropped, updated_at) = {
            let state = sync::lock(&self.state);
            (
                state.status,
                state.exit_code,
                state.backpressure_dropped,
                state.updated_at.clone(),
            )
        };
        // The reference declares no field for a reader that fell behind: its
        // own reader reports every failure through `reader_error`, so output
        // this session had to drop is named there rather than through a field
        // the reference does not publish.
        let reader_error = self
            .reader_error
            .clone()
            .or_else(|| dropped.then(|| DROPPED_OUTPUT.to_owned()));
        SessionInfo {
            session_id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.working_directory.clone(),
            shell: self.shell.clone(),
            pty_backend: self.pty_backend.map(str::to_owned),
            status,
            exit_code: exit_code.map(i64::from),
            output_path: self.log_path.to_string_lossy().into_owned(),
            created_at: self.created_at.clone(),
            updated_at,
            reader_error,
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.snapshot().0 == SessionStatus::Running
    }

    /// Records the status a kill is about to produce before the kill runs, so
    /// the reader that sees the process exit does not report it as completed.
    /// Reference `kill` sets the status under the session's condition first.
    fn mark(&self, status: SessionStatus) {
        let mut state = sync::lock(&self.state);
        state.status = status;
        state.updated_at = now_iso();
    }

    /// Records how the process ended without touching its status, for a
    /// session a kill already marked.
    fn record_exit(&self, exit_code: Option<i32>) {
        let mut state = sync::lock(&self.state);
        if state.exit_code.is_none() {
            state.exit_code = exit_code;
        }
    }

    pub(super) fn settle(&self, status: SessionStatus, exit_code: Option<i32>) {
        {
            let mut state = sync::lock(&self.state);
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
        let _ = write_manifest(&self.manifest_path, &self.info().typed());
    }
}

/// Writes one manifest the way reference `_save_manifest` writes it:
/// `json.dumps(metadata, indent=2, sort_keys=True)`, no trailing newline.
fn write_manifest(path: &Path, metadata: &Value) -> Result<(), String> {
    let rendered = pycompat::dumps(
        metadata,
        Dumps {
            sort_keys: true,
            indent: Some(2),
            ..Dumps::DEFAULT
        },
    );
    std::fs::write(path, rendered).map_err(|error| error.to_string())
}

/// Reference `_now_iso`: the current instant in UTC, rendered the way Python's
/// `datetime.isoformat` renders it.
pub(super) fn now_iso() -> String {
    UtcTimestamp::now().to_iso8601()
}

pub(super) async fn run_managed_command(
    shell: &Arc<SessionShell>,
    config: &ShellConfig,
    working_directory: &Path,
    call: &ShellCallArguments,
    settings: &ShellCommandConfig,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = &call.command;
    let requested_directory = call
        .cwd
        .as_ref()
        .map_or_else(|| working_directory.to_path_buf(), PathBuf::from);
    // Reference `ExperimentalBash.run` resolves the shell before the session
    // exists: the call's override, then the tool's configured shell, then the
    // family's own default, and an override that does not resolve refuses the
    // call rather than falling back.
    let config = resolve_session_shell(
        shell.family,
        config,
        call.shell.as_deref(),
        settings.shell.as_deref(),
    )?;
    let session = start_managed_session(
        shell,
        &config,
        &requested_directory,
        command,
        &call.environment,
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
    // a poll.
    let limit = settings.max_output_bytes;
    let handle = SessionHandle::Live(session.clone());
    if call.background {
        let window = SessionWindow::read(&handle, 0, limit)?;
        return Ok(window.command_document(true).into_output(window.display()));
    }
    let hard_timeout = call.is_hard_timeout();
    let timeout = call.wait(settings);
    let deadline = Instant::now() + timeout;
    // Reference `_kill_on_abort`: until the session exits or is handed to the
    // background, it belongs to this call, so a cancelled turn stops it rather
    // than leaving a PTY behind.
    let mut abort = AbortKill::new(shell.clone(), session.clone());
    let mut stream = ForegroundStream::new(limit);
    while session.is_running() && Instant::now() < deadline {
        tokio::time::sleep(PUMP_INTERVAL).await;
        stream.pump(&session, output);
    }
    stream.pump(&session, output);
    if session.is_running() {
        if !hard_timeout {
            // A soft timeout leaves the session running, and the reference
            // reports it as a backgrounded one: the model polls it with the
            // family's output tool instead of losing the work.
            abort.disarm();
            let window = SessionWindow::read(&handle, 0, limit)?;
            return Ok(window.command_document(true).into_output(window.display()));
        }
        kill_managed_session(shell, &session, SessionStatus::TimedOut).await?;
        abort.disarm();
        // The kill can push out a last line after the drain above.
        stream.pump(&session, output);
        let window = SessionWindow::read(&handle, 0, limit)?;
        return Err(ToolError::Execution(format!(
            "the command timed out after {}s and its process group was terminated: \
             `{command}`\nsession_id: {}\noutput:\n{}",
            render_seconds(timeout),
            session.id,
            window.command_document(false).model_text()
        )));
    }
    abort.disarm();
    let window = SessionWindow::read(&handle, 0, limit)?;
    let document = window.command_document(false);
    let code = window.returncode();
    // Reference `_result_from_session` succeeds only when both halves agree:
    // the session reached `completed` and its return code is zero. A session
    // that was killed on its way out carries whatever code the kill produced,
    // which is zero often enough that the code alone would report it as a
    // success.
    let status = window.info.status;
    if status != SessionStatus::Completed || code != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {code}: `{command}`\nsession_id: {}\nstatus: \
             {}\noutput:\n{}",
            session.id,
            status.as_str(),
            document.model_text()
        )));
    }
    Ok(document.into_output(window.display()))
}

/// Reference `_ForegroundStream`: while a foreground command runs, what it
/// prints reaches the client as it arrives.
///
/// One budget caps the whole stream, the same `max_output_bytes` the result is
/// read under, and each piece is clipped to a whole character, so a stream
/// never carries more than the result it precedes.
struct ForegroundStream {
    cursor: u64,
    budget: usize,
    limit: usize,
}

impl ForegroundStream {
    fn new(limit: usize) -> Self {
        Self {
            cursor: 0,
            budget: limit,
            limit,
        }
    }

    /// Emits what the log gained since the last pump.
    fn pump(&mut self, session: &ManagedSession, output: &ToolOutputSink) {
        let Ok((text, next_cursor, _)) = read_file_window(
            &session.log_path,
            self.cursor,
            self.limit,
            session.is_running(),
        ) else {
            return;
        };
        self.cursor = next_cursor;
        let budget = self.budget.min(output.remaining_bytes());
        if budget == 0 || text.is_empty() {
            return;
        }
        let message = clip_to_bytes(&text, budget);
        if message.is_empty() {
            return;
        }
        self.budget = self.budget.saturating_sub(message.len());
        let _ = output.emit(message);
    }
}

/// Reference `_clip_to_bytes`: the longest prefix of `text` that fits in
/// `limit` bytes without splitting a character.
fn clip_to_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Kills a foreground session when the call that owns it is dropped.
///
/// A cancelled turn drops the tool future, which drops this guard, which
/// settles the session as killed and stops its process group, exactly as the
/// reference's shielded kill does. A call that hands the session back, to its
/// exit or to the background, disarms it first.
struct AbortKill {
    shell: Arc<SessionShell>,
    session: Option<Arc<ManagedSession>>,
}

impl AbortKill {
    fn new(shell: Arc<SessionShell>, session: Arc<ManagedSession>) -> Self {
        Self {
            shell,
            session: Some(session),
        }
    }

    fn disarm(&mut self) {
        self.session = None;
    }
}

impl Drop for AbortKill {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        if !session.is_running() {
            return;
        }
        let shell = self.shell.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = kill_managed_session(&shell, &session, SessionStatus::Killed).await;
            });
        }
    }
}

pub(super) async fn start_managed_session(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: &[(String, String)],
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
            shell,
            config,
            working_directory,
            command,
            environment,
            max_output_bytes,
            true,
        ))
        .await
        .map_err(process_error)?;
    // The terminal started, so it has a name; reading it is not allowed to turn
    // a live managed session into one reporting no backend at all.
    let backend = shell
        .terminals
        .backend(&terminal_id)
        .await
        .map_err(process_error)?;
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
        pty_backend: backend,
        reader_error: None,
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
                // A terminal released under the pump belongs to a kill, which
                // settles the session itself; one that vanished otherwise
                // leaves a session nothing will settle.
                if session.is_running() {
                    session.settle(SessionStatus::Killed, None);
                }
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
                    let code = Some(exit_status(&final_read.state));
                    if session.is_running() {
                        session.settle(SessionStatus::Completed, code);
                    } else {
                        session.record_exit(code);
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
    if dropped {
        sync::lock(&session.state).backpressure_dropped = true;
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
    // Reference `_new_session_id` takes its eight hexadecimal characters from
    // `uuid4().hex[:8]`, so the stamp is not what separates two sessions minted
    // inside the same second: a collision would let one read another's log, and
    // the suffix carries real entropy rather than a counter.
    format!(
        "{}_{}_{}",
        family.name(),
        compact_stamp(now),
        crate::session_id::random_hex::<4>()
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

/// Wraps one managed-family handler, which all share the same shape: a session
/// store, the call arguments, and the turn's output budget.
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

pub(super) async fn kill_managed_session(
    shell: &SessionShell,
    session: &ManagedSession,
    status: SessionStatus,
) -> Result<(), ToolError> {
    // Reference `kill` answers a session that already settled with the record
    // it holds rather than killing it again.
    if !session.is_running() {
        return Ok(());
    }
    session.mark(status);
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
