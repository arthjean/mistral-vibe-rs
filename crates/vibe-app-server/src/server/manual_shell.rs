//! `session/shellCommand`: a command the user runs with `!` outside any turn.
//!
//! Reference `vibe/app_server/_shell.py` and `_shell_requests.py`. The command
//! runs in the session's workspace behind a `shell` effect entry with no turn,
//! its output streams into that entry, and its result joins the conversation
//! as injected context the next turn reads. The injected message carries the
//! whole run, which is what a reloaded session projects the entry back from.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use vibe_core::events::{EffectCallDisplay, ManualShellRecord, ToolEffectKind};
use vibe_core::process::{ProcessSpec, ProcessStream, TerminalManager, TerminalState};
use vibe_core::tools::config::ShellCommandConfig;

use super::*;

/// The tool name a manual command's effect publishes (reference
/// `MANUAL_SHELL_TOOL_NAME`).
const MANUAL_SHELL_TOOL_NAME: &str = "shell";
/// How much of each stream the model reads when no `bash` limit is set.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16_000;
/// How long a command runs when the request names no timeout.
pub(super) const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0;
/// How often a running command is polled for output and for an interrupt.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The command a session is running, which blocks anything else from
/// running there until it settles (reference `SessionExecution`).
#[derive(Debug, Clone)]
pub(crate) struct ShellOperation {
    pub(crate) id: String,
    pub(crate) interrupted: Arc<AtomicBool>,
}

/// Reference `shell_effect_detail`.
pub(super) fn shell_effect_detail(command: &str) -> EffectDetail {
    EffectDetail {
        tool_name: MANUAL_SHELL_TOOL_NAME.to_owned(),
        display: EffectCallDisplay {
            summary: format!("shell: {command}"),
            content: None,
            suffix: String::new(),
            verb: "Running".to_owned(),
            message: Some(command.to_owned()),
            settled_verb: "Ran".to_owned(),
            settled_message: Some(command.to_owned()),
            status_text: "Running command".to_owned(),
        },
        kind: ToolEffectKind::Shell,
        input: json!({"command": command}),
        child_session_id: None,
        remote: None,
    }
}

fn result_display(success: bool, message: &str) -> EffectResultDisplay {
    EffectResultDisplay {
        success,
        verb: String::new(),
        message: message.to_owned(),
        warnings: Vec::new(),
        approval_note: None,
        suffix: String::new(),
    }
}

fn whole_millis(duration_ms: &serde_json::Number) -> u64 {
    duration_ms
        .as_u64()
        .or_else(|| {
            duration_ms
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        })
        .unwrap_or_default()
}

/// Reference `shell_effect_state`: how a finished command settles its entry.
pub(super) fn shell_effect_state(record: &ManualShellRecord) -> PublicEffectState {
    let duration_ms = whole_millis(&record.duration_ms);
    if record.interrupted {
        let message = "Command interrupted";
        return PublicEffectState::Cancelled {
            reason: message.to_owned(),
            output_text: record.output_text.clone(),
            duration_ms,
            display: Some(result_display(false, message)),
            approval: vibe_core::events::EffectApproval::default(),
        };
    }
    let message = if record.timed_out {
        "Command timed out".to_owned()
    } else if record.exit_code != 0 {
        format!("Command exited with status {}", record.exit_code)
    } else {
        format!("Ran {}", record.command)
    };
    let display = result_display(record.exit_code == 0, &message);
    if record.timed_out || record.exit_code != 0 {
        return PublicEffectState::Failed {
            error: PublicError {
                message,
                code: None,
                details: Value::Null,
            },
            output: Value::Null,
            output_text: record.output_text.clone(),
            duration_ms,
            display,
            approval: vibe_core::events::EffectApproval::default(),
        };
    }
    PublicEffectState::Completed {
        output: json!({
            "stdout": record.stdout,
            "stderr": record.stderr,
            "output": record.output_text,
        }),
        output_text: record.output_text.clone(),
        duration_ms,
        display,
        approval: vibe_core::events::EffectApproval::default(),
    }
}

/// The effect entry a stored manual command projects back to, as reference
/// `_append_injected_history` rebuilds it.
pub(super) fn restored_shell_entry(
    session_id: &str,
    record: &ManualShellRecord,
) -> PublicHistoryEntry {
    PublicHistoryEntry::Effect {
        metadata: PublicEntryMetadata {
            id: record.operation_id.clone(),
            session_id: session_id.to_owned(),
            turn_id: None,
            created_at: record.created_at,
            updated_at: record.created_at,
            generation_status: PublicEntryGenerationStatus::Completed,
            related_entry_id: None,
        },
        title: "shell".to_owned(),
        detail: Box::new(shell_effect_detail(&record.command)),
        state: shell_effect_state(record),
        tool_call_id: String::new(),
    }
}

/// How much of a stream the model reads: the operator's `bash` limit when it
/// is a positive count (reference `manual_shell_output_limit`).
fn output_limit(session: &SessionRuntime) -> usize {
    let limit = session
        .policy
        .tool_config()
        .view::<ShellCommandConfig>("bash")
        .max_output_bytes;
    if limit > 0 {
        limit
    } else {
        DEFAULT_MAX_OUTPUT_BYTES
    }
}

/// Cuts `text` to `limit` characters, marking the cut.
fn capped(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}\n... [truncated]", &text[..cut]),
        None => text.to_owned(),
    }
}

/// The context the model reads after the user ran a command. The sections
/// follow reference `manual_shell_context`; the wording is this port's own.
pub(super) fn manual_shell_context(record: &ManualShellRecord, limit: usize) -> String {
    let stdout = capped(&record.stdout, limit);
    let stderr = capped(&record.stderr, limit);
    let mut sections = vec![
        "The user ran a `!` command by hand. Its result is context for you, not a request."
            .to_owned(),
        format!("Command: `{}`", record.command),
        format!("Working directory: `{}`", record.cwd),
    ];
    sections.push(if record.timed_out {
        "Status: timed out".to_owned()
    } else if record.interrupted {
        "Status: interrupted by the user".to_owned()
    } else {
        format!("Exit code: {}", record.exit_code)
    });
    if !stdout.is_empty() {
        sections.push(format!("Stdout:\n```text\n{}\n```", stdout.trim_end()));
    }
    if !stderr.is_empty() {
        sections.push(format!("Stderr:\n```text\n{}\n```", stderr.trim_end()));
    }
    if stdout.is_empty() && stderr.is_empty() {
        sections.push("Output:\n```text\n(no output)\n```".to_owned());
    }
    sections.join("\n\n")
}

/// Reference `resolve_workspace_cwd`: the directory a command runs in,
/// confined to the session's workspace.
pub(super) fn resolve_workspace_cwd(
    root: &Path,
    requested: Option<&str>,
) -> Result<PathBuf, ProtocolFault> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let requested = requested.map(|path| {
        let expanded = expand_user(path);
        expanded.canonicalize().unwrap_or(expanded)
    });
    let cwd = requested.unwrap_or_else(|| root.clone());
    if !cwd.is_dir() {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::InvalidParams,
            format!("Shell working directory does not exist: {}", cwd.display()),
        ));
    }
    if !cwd.starts_with(&root) {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::Forbidden,
            format!(
                "Shell working directory is outside the workspace: {}",
                cwd.display()
            ),
        ));
    }
    Ok(cwd)
}

/// `~` and `~/...` read against the home directory, as Python's
/// `Path.expanduser` does.
fn expand_user(path: &str) -> PathBuf {
    let home = || std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => home().map_or_else(
            || PathBuf::from(path),
            |home| PathBuf::from(home).join(rest.trim_start_matches(['/', '\\'])),
        ),
        _ => PathBuf::from(path),
    }
}

/// Decodes a stream as it arrives, holding back a character split across two
/// reads and replacing what is not UTF-8 (Python's incremental decoder with
/// `errors="replace"`).
#[derive(Default)]
struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = self.pending.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[length..];
                        }
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        text
    }

    fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

/// What running a command produced.
struct ShellRun {
    stdout: String,
    stderr: String,
    exit_code: i64,
    timed_out: bool,
    interrupted: bool,
}

/// The shell a manual command runs under: `$SHELL -c` on POSIX (reference
/// `create_subprocess_shell` with `executable=$SHELL`), `cmd.exe /d /c` on
/// Windows.
fn shell_spec(command: &str, cwd: &Path) -> ProcessSpec {
    let (program, arguments) = if cfg!(windows) {
        ("cmd.exe".to_owned(), vec!["/d".to_owned(), "/c".to_owned()])
    } else {
        (
            std::env::var("SHELL")
                .ok()
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(|| "/bin/sh".to_owned()),
            vec!["-c".to_owned()],
        )
    };
    let mut spec = ProcessSpec::new(program, cwd);
    spec.arguments = arguments.into_iter().chain([command.to_owned()]).collect();
    spec.max_output_bytes = usize::MAX;
    // Reference `_shell_environment`: a non-interactive child whose pagers
    // exit and whose output is UTF-8. `LC_ALL` would override `LC_CTYPE`.
    let environment: &[(&str, &str)] = if cfg!(windows) {
        &[
            ("CI", "true"),
            ("NONINTERACTIVE", "1"),
            ("NO_TTY", "1"),
            ("GIT_PAGER", "more"),
            ("PAGER", "more"),
        ]
    } else {
        spec.unset_environment = vec!["LC_ALL".to_owned()];
        &[
            ("CI", "true"),
            ("NONINTERACTIVE", "1"),
            ("NO_TTY", "1"),
            ("TERM", "dumb"),
            ("DEBIAN_FRONTEND", "noninteractive"),
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("LESS", "-FX"),
            ("LC_CTYPE", "C.UTF-8"),
        ]
    };
    for (key, value) in environment {
        spec.environment
            .insert((*key).to_owned(), (*value).to_owned());
    }
    spec
}

/// Runs `command` to completion, a timeout or an interrupt, handing every
/// decoded chunk to `observe` as it arrives (reference `ShellController.run`).
async fn run_shell(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    interrupted: &AtomicBool,
    mut observe: impl FnMut(String),
) -> Result<ShellRun, String> {
    let terminals = TerminalManager::default();
    let terminal_id = terminals
        .run(shell_spec(command, cwd))
        .await
        .map_err(|error| error.to_string())?;
    let _ = terminals.close_stdin(&terminal_id).await;
    let mut decoders = [StreamDecoder::default(), StreamDecoder::default()];
    let mut streams = [String::new(), String::new()];
    let mut consume = |read: vibe_core::process::ProcessRead, observe: &mut dyn FnMut(String)| {
        for chunk in read.chunks {
            let index = usize::from(chunk.stream == ProcessStream::Stderr);
            let text = decoders[index].feed(&chunk.bytes);
            if !text.is_empty() {
                streams[index].push_str(&text);
                observe(text);
            }
        }
        read.state
    };
    let started = Instant::now();
    let mut timed_out = false;
    let state = loop {
        if interrupted.load(Ordering::Acquire) || started.elapsed() >= timeout {
            timed_out = !interrupted.load(Ordering::Acquire);
            let read = terminals
                .interrupt(&terminal_id)
                .await
                .map_err(|error| error.to_string())?;
            break consume(read, &mut observe);
        }
        let read = terminals
            .read(&terminal_id)
            .await
            .map_err(|error| error.to_string())?;
        if matches!(consume(read, &mut observe), TerminalState::Running) {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        let read = terminals
            .wait(&terminal_id)
            .await
            .map_err(|error| error.to_string())?;
        break consume(read, &mut observe);
    };
    let _ = terminals.release(&terminal_id).await;
    for (index, decoder) in decoders.iter_mut().enumerate() {
        let text = decoder.finish();
        if !text.is_empty() {
            streams[index].push_str(&text);
            observe(text);
        }
    }
    let interrupted = interrupted.load(Ordering::Acquire) && !timed_out;
    let exit_code = match state {
        _ if timed_out || interrupted => 1,
        TerminalState::Exited { code, signal, .. }
        | TerminalState::Interrupted { code, signal } => code
            .map(i64::from)
            .or_else(|| signal.map(|signal| -i64::from(signal)))
            .unwrap_or_default(),
        TerminalState::Running => 0,
        TerminalState::Failed { message } => return Err(message),
    };
    let [stdout, stderr] = streams;
    Ok(ShellRun {
        stdout,
        stderr,
        exit_code,
        timed_out,
        interrupted,
    })
}

impl AppServer {
    /// Runs one manual command the connection reserved, publishing its entry
    /// through `notify` as it goes, and answers it once its result has joined
    /// the conversation.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_shell_command(
        &self,
        request_id: RequestId,
        session_id: String,
        operation_id: String,
        command: String,
        cwd: String,
        timeout_ms: u64,
        notify: impl Fn(Vec<u8>) + Send + Sync,
    ) -> DispatchBatch {
        let created_at = now_millis();
        let started = Instant::now();
        let interrupted =
            match self.start_shell_entry(&session_id, &operation_id, &command, created_at) {
                Ok((frame, interrupted)) => {
                    notify(frame);
                    interrupted
                }
                Err(fault) => return fault.into_batch(request_id),
            };
        let mut output_text = String::new();
        let run = run_shell(
            &command,
            Path::new(&cwd),
            Duration::from_millis(timeout_ms),
            &interrupted,
            |chunk| {
                output_text.push_str(&chunk);
                if let Some(frame) = self.append_shell_output(&session_id, &operation_id, &chunk) {
                    notify(frame);
                }
            },
        )
        .await;
        let duration_ms = serde_json::Number::from_f64(started.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or_else(|| serde_json::Number::from(0));
        let run = match run {
            Ok(run) => run,
            Err(message) => {
                let state = PublicEffectState::Failed {
                    error: PublicError {
                        message: message.clone(),
                        code: None,
                        details: Value::Null,
                    },
                    output: Value::Null,
                    output_text,
                    duration_ms: whole_millis(&duration_ms),
                    display: result_display(false, &message),
                    approval: vibe_core::events::EffectApproval::default(),
                };
                if let Ok(frame) = self.settle_shell_entry(&session_id, &operation_id, state) {
                    notify(frame);
                }
                return ProtocolFault::internal(message).into_batch(request_id);
            }
        };
        let record = ManualShellRecord {
            operation_id: operation_id.clone(),
            command,
            cwd,
            stdout: run.stdout,
            stderr: run.stderr,
            output_text,
            exit_code: run.exit_code,
            timed_out: run.timed_out,
            interrupted: run.interrupted,
            duration_ms,
            created_at,
        };
        match self.settle_shell_entry(&session_id, &operation_id, shell_effect_state(&record)) {
            Ok(frame) => notify(frame),
            Err(fault) => return fault.into_batch(request_id),
        }
        match self.inject_shell_result(&session_id, record) {
            Ok((last_event_id, deferred)) => DispatchBatch {
                outbound: vec![success_bytes(
                    request_id,
                    result_map([
                        ("accepted", json!(true)),
                        ("lastEventId", json!(last_event_id)),
                    ]),
                )],
                deferred: deferred.into_iter().collect(),
                close_after_flush: false,
            },
            Err(fault) => fault.into_batch(request_id),
        }
    }

    /// Answers an interrupt once the command it stops has settled, or at
    /// the latest after `SETTLE_WAIT`.
    pub async fn answer_shell_interrupt(
        &self,
        request_id: RequestId,
        session_id: String,
        operation_id: String,
    ) -> DispatchBatch {
        const SETTLE_WAIT: Duration = Duration::from_secs(10);
        let started = Instant::now();
        loop {
            let watermark = match self.lock_sessions() {
                Ok(sessions) => sessions.get(&session_id).map(|session| {
                    let running = session
                        .shell_operation
                        .as_ref()
                        .is_some_and(|operation| operation.id == operation_id);
                    (running, session.event_watermark)
                }),
                Err(error) => return internal_error_batch(request_id, &error),
            };
            match watermark {
                Some((true, _)) if started.elapsed() < SETTLE_WAIT => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Some((_, last_event_id)) => {
                    return success_batch(
                        request_id,
                        result_map([
                            ("accepted", json!(true)),
                            ("lastEventId", json!(last_event_id)),
                        ]),
                    );
                }
                None => return session_missing("Session not found").into_batch(request_id),
            }
        }
    }

    /// Frees a session a command reserved and never ran.
    pub fn release_shell_operation(&self, session_id: &str, operation_id: &str) {
        if let Ok(mut sessions) = self.lock_sessions()
            && let Some(session) = sessions.get_mut(session_id)
            && session
                .shell_operation
                .as_ref()
                .is_some_and(|operation| operation.id == operation_id)
        {
            session.shell_operation = None;
        }
    }

    /// Opens the command's entry and answers the frame that publishes it,
    /// with the flag an interrupt raises.
    fn start_shell_entry(
        &self,
        session_id: &str,
        operation_id: &str,
        command: &str,
        created_at: u64,
    ) -> Result<(Vec<u8>, Arc<AtomicBool>), ProtocolFault> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        let interrupted = session
            .shell_operation
            .as_ref()
            .filter(|operation| operation.id == operation_id)
            .map(|operation| Arc::clone(&operation.interrupted))
            .unwrap_or_default();
        let entry = PublicHistoryEntry::Effect {
            metadata: PublicEntryMetadata {
                id: operation_id.to_owned(),
                session_id: session.id.clone(),
                turn_id: None,
                created_at,
                updated_at: created_at,
                generation_status: PublicEntryGenerationStatus::InProgress,
                related_entry_id: None,
            },
            title: "shell".to_owned(),
            detail: Box::new(shell_effect_detail(command)),
            state: PublicEffectState::Running {
                output_text: String::new(),
            },
            tool_call_id: String::new(),
        };
        session_snapshot(session).history.push(entry.clone());
        let frame = shell_frame(session, "history/entryAdded", [("entry", json!(entry))]);
        Ok((frame, interrupted))
    }

    /// Appends a chunk of output to the command's entry.
    fn append_shell_output(
        &self,
        session_id: &str,
        operation_id: &str,
        chunk: &str,
    ) -> Option<Vec<u8>> {
        let mut sessions = self.lock_sessions().ok()?;
        let session = sessions.get_mut(session_id)?;
        let timestamp = now_millis();
        let (metadata, state) = shell_entry(session, operation_id)?;
        if let PublicEffectState::Running { output_text } = state {
            output_text.push_str(chunk);
        }
        metadata.updated_at = timestamp;
        Some(shell_frame(
            session,
            "history/entryUpdated",
            [
                ("entryId", json!(operation_id)),
                (
                    "patch",
                    json!([
                        {"op": "append", "path": "/state/outputText", "value": chunk},
                        {"op": "replace", "path": "/updatedAt", "value": timestamp},
                    ]),
                ),
            ],
        ))
    }

    /// Settles the command's entry in `state` and releases the session.
    fn settle_shell_entry(
        &self,
        session_id: &str,
        operation_id: &str,
        state: PublicEffectState,
    ) -> Result<Vec<u8>, ProtocolFault> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session
            .shell_operation
            .as_ref()
            .is_some_and(|operation| operation.id == operation_id)
        {
            session.shell_operation = None;
        }
        let timestamp = now_millis();
        let (metadata, current) = shell_entry(session, operation_id)
            .ok_or_else(|| ProtocolFault::internal("the shell entry is gone"))?;
        *current = state;
        metadata.generation_status = PublicEntryGenerationStatus::Completed;
        metadata.updated_at = timestamp;
        let state = json!(current);
        Ok(shell_frame(
            session,
            "history/entryUpdated",
            [
                ("entryId", json!(operation_id)),
                (
                    "patch",
                    json!([
                        {"op": "replace", "path": "/state", "value": state},
                        {"op": "replace", "path": "/generationStatus", "value": "completed"},
                        {"op": "replace", "path": "/updatedAt", "value": timestamp},
                    ]),
                ),
            ],
        ))
    }

    /// Hands a finished command to the conversation as injected context,
    /// saved with the run it reports, and answers the session's watermark.
    /// A session that saves nothing leaves the context to its driver.
    fn inject_shell_result(
        &self,
        session_id: &str,
        record: ManualShellRecord,
    ) -> Result<(u64, Option<DeferredWork>), ProtocolFault> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        let content = manual_shell_context(&record, output_limit(session));
        session.context.push(content.clone());
        session.updated_at = now_millis();
        let persisted = self.persist_injected_message(session, &content, None, Some(record))?;
        let deferred = (!persisted).then(|| DeferredWork::InjectContext {
            session_id: session_id.to_owned(),
            content,
            as_message: false,
            inject_invoked_skill: false,
        });
        Ok((session.event_watermark, deferred))
    }

    /// Appends injected content to the session's saved log, naming the
    /// session on disk first when nothing was written yet: a message under
    /// `message_id`, context as an injected turn, which a manual command's
    /// result carries its run with. Answers whether the session records
    /// itself at all.
    pub(super) fn persist_injected_message(
        &self,
        session: &SessionRuntime,
        content: &str,
        message_id: Option<&str>,
        manual_shell: Option<ManualShellRecord>,
    ) -> Result<bool, ProtocolFault> {
        if !self.workspace.persists_runtime_sessions() {
            return Ok(false);
        }
        let store = self.workspace.session_store();
        let now = now_millis();
        let storage =
            |error: vibe_core::storage::StorageError| ProtocolFault::internal(error.to_string());
        let mut metadata = match store.open(&session.id) {
            Ok(hydrated) => hydrated.metadata,
            Err(vibe_core::storage::StorageError::SessionNotFound(_)) => store
                .create(&session.id, &session.working_directory, None, now)
                .map_err(storage)?,
            Err(error) => return Err(storage(error)),
        };
        store
            .append_message(
                &mut metadata,
                &ModelMessage::User {
                    content: content.to_owned(),
                    injected: message_id.is_none(),
                    message_id: message_id.map(ToOwned::to_owned),
                    attachments: Vec::new(),
                    manual_shell: manual_shell.map(Box::new),
                    compaction_boundary: false,
                    input_text: None,
                    user_display_content: None,
                },
                now,
            )
            .map_err(storage)?;
        Ok(true)
    }
}

/// The session's projection, opened empty when nothing was projected yet.
pub(super) fn session_snapshot(session: &mut SessionRuntime) -> &mut ProjectionSnapshot {
    let session_id = session.id.clone();
    session.snapshot.get_or_insert_with(|| ProjectionSnapshot {
        session_id,
        turn_id: None,
        handoff_cause: None,
        watermark: 0,
        lifecycle: LifecycleState::Completed,
        title: None,
        history: Vec::new(),
    })
}

fn shell_entry<'a>(
    session: &'a mut SessionRuntime,
    operation_id: &str,
) -> Option<(&'a mut PublicEntryMetadata, &'a mut PublicEffectState)> {
    session
        .snapshot
        .as_mut()?
        .history
        .iter_mut()
        .rev()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Effect {
                metadata, state, ..
            } if metadata.id == operation_id && metadata.turn_id.is_none() => {
                Some((metadata, state))
            }
            _ => None,
        })
}

/// A sequenced history notification of a manual command, which belongs to
/// no turn.
fn shell_frame<const N: usize>(
    session: &mut SessionRuntime,
    method: &str,
    fields: [(&str, Value); N],
) -> Vec<u8> {
    let event_id = next_event_id(session);
    let mut params = result_map([
        ("eventId", json!(event_id)),
        ("sessionId", json!(session.id)),
        ("turnId", Value::Null),
    ]);
    params.extend(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    params.insert("emittedAt".to_owned(), json!(now_millis()));
    encode_notification(method, params)
}
