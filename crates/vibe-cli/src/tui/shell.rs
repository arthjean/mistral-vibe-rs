use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_app_server::client::{ClientError, PublicDispatch, PublicHistoryEntry, PublishedField};
use vibe_core::process::{ProcessChunk, ProcessStream};

use super::hydration::history_entry;
use super::render::clean_output;
use super::state::{TranscriptEntry, TuiState};
use super::transcript::MANUAL_SHELL_TOOL_NAME;
use super::{CliError, InteractiveRuntime};

const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Reference `ShellRunParams.timeout_seconds` falls back to thirty seconds
/// when the terminal names none, which it never does
/// (`vibe/app_server/_shell_requests.py`).
const MANUAL_SHELL_TIMEOUT: Duration = Duration::from_secs(30);

/// Reference `DEFAULT_MAX_OUTPUT_BYTES`, the `bash` tool's default, which
/// bounds what the model reads of a manual command when the configuration
/// names no other limit (`manual_shell_output_limit`).
const DEFAULT_CONTEXT_LIMIT: usize = 16_000;

pub(super) struct ActiveShell {
    command: String,
    operation_id: String,
    transcript_id: String,
    working_directory: PathBuf,
    chunks: Vec<ProcessChunk>,
    started: Instant,
    last_poll: Instant,
    timed_out: bool,
    /// The `session/shellCommand` a unified session runs the command
    /// under, which answers once the command settled. The server then
    /// publishes the entry and hands the result to the model itself.
    unified: Option<tokio::task::JoinHandle<Result<PublicDispatch, ClientError>>>,
}

impl ActiveShell {
    pub(super) fn new(
        command: impl Into<String>,
        operation_id: impl Into<String>,
        transcript_id: impl Into<String>,
        working_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            command: command.into(),
            operation_id: operation_id.into(),
            transcript_id: transcript_id.into(),
            working_directory: working_directory.into(),
            chunks: Vec::new(),
            started: Instant::now(),
            last_poll: Instant::now(),
            timed_out: false,
            unified: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ShellRead {
    chunks: Vec<ProcessChunk>,
    state: ShellState,
}

impl ShellRead {
    #[cfg(test)]
    pub(super) fn running(cursor: u64, text: impl Into<Vec<u8>>) -> Self {
        Self {
            chunks: vec![ProcessChunk {
                cursor,
                stream: ProcessStream::Stdout,
                bytes: text.into(),
            }],
            state: ShellState::Running,
        }
    }

    #[cfg(test)]
    pub(super) fn interrupted() -> Self {
        Self {
            chunks: Vec::new(),
            state: ShellState::Interrupted,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ShellState {
    Running,
    Exited {
        #[serde(default)]
        code: Option<i32>,
        success: bool,
    },
    Interrupted,
    Failed {
        message: String,
    },
}

pub(super) async fn start_shell(
    input: &str,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> Result<bool, CliError> {
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before running shell commands");
        return Ok(false);
    };
    let command = input
        .trim_start()
        .strip_prefix('!')
        .map(str::trim)
        .unwrap_or_default();
    if command.is_empty() {
        state.push_diagnostic("No command provided after '!'");
        return Ok(true);
    }
    let operation_id = format!("manual-shell-{}", vibe_core::clock::now_nanos());
    let unified = if runtime.service.is_unified(&runtime.session_id) {
        // Reference `ShellResources.run` on either backend.
        let params = json!({
            "sessionId": runtime.session_id,
            "command": command,
            "operationId": operation_id,
            "action": "run",
            "timeoutSeconds": MANUAL_SHELL_TIMEOUT.as_secs_f64(),
        });
        match runtime
            .service
            .begin_public_call("session/shellCommand", params)
        {
            Ok(pending) => Some(tokio::spawn(pending.complete())),
            Err(error) => {
                state.push_diagnostic(format!("Command failed: {error}"));
                return Ok(true);
            }
        }
    } else {
        let params = shell_params(&runtime.session_id, &operation_id, command);
        if let Err(error) = runtime.service.public_call_async("shell/run", params).await {
            state.push_diagnostic(format!("Command failed: {error}"));
            return Ok(true);
        }
        None
    };
    state.waiting = true;
    let mut shell = ActiveShell::new(command, operation_id, String::new(), working_directory);
    shell.unified = unified;
    let Some(entry) = shell_entry(&runtime.session_id, &shell, &ShellState::Running) else {
        state.push_diagnostic("Command failed: the shell entry could not be projected");
        return Ok(true);
    };
    shell.transcript_id = state.append_local(entry);
    runtime.shell = Some(shell);
    Ok(true)
}

pub(super) async fn finish_shell(runtime: Option<&mut InteractiveRuntime>, state: &mut TuiState) {
    let Some(runtime) = runtime else {
        return;
    };
    let Some(shell) = runtime.shell.take() else {
        return;
    };
    if shell.last_poll.elapsed() < SHELL_POLL_INTERVAL {
        runtime.shell = Some(shell);
        return;
    }
    if shell.unified.is_some() {
        follow_unified_shell(runtime, state, shell).await;
        return;
    }
    if shell.started.elapsed() >= MANUAL_SHELL_TIMEOUT {
        runtime.shell = Some(shell);
        stop_shell(runtime, state, true).await;
        return;
    }
    let params = shell_params(&runtime.session_id, &shell.operation_id, &shell.command);
    let dispatch = match runtime.service.public_call_async("shell/run", params).await {
        Ok(dispatch) => dispatch,
        Err(error) => {
            state.push_diagnostic(format!("Command failed: {error}"));
            retain_shell(&mut runtime.shell, state, shell);
            return;
        }
    };
    let Some(read) = shell_read(&dispatch.result) else {
        state.push_diagnostic("Shell response did not contain typed process output");
        retain_shell(&mut runtime.shell, state, shell);
        return;
    };
    settle_read(runtime, state, shell, read);
}

/// Draws the entry a unified session publishes for the command, and lets
/// the command go once its request answered.
async fn follow_unified_shell(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    mut shell: ActiveShell,
) {
    let published = runtime
        .service
        .published_state(&runtime.session_id, PublishedField::History)
        .and_then(|history| {
            history
                .as_array()?
                .iter()
                .find(|entry| entry["id"] == shell.operation_id.as_str())
                .cloned()
        })
        .and_then(|entry| serde_json::from_value::<PublicHistoryEntry>(entry).ok());
    if let Some(entry) = published {
        let _ = state.replace_local(&shell.transcript_id, history_entry(entry));
    }
    let Some(request) = shell.unified.take_if(|request| request.is_finished()) else {
        retain_shell(&mut runtime.shell, state, shell);
        return;
    };
    state.waiting = false;
    match request.await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => state.push_diagnostic(format!("Command failed: {error}")),
        Err(error) => state.push_diagnostic(format!("Command failed: {error}")),
    }
}

pub(super) async fn interrupt_shell(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    stop_shell(runtime, state, false).await;
}

/// Stops the running command, on the operator's request or once it has run
/// past [`MANUAL_SHELL_TIMEOUT`].
async fn stop_shell(runtime: &mut InteractiveRuntime, state: &mut TuiState, timed_out: bool) {
    let Some(mut shell) = runtime.shell.take() else {
        return;
    };
    if shell.unified.is_some() {
        // The run answers once the interrupt settled it.
        let result = runtime
            .service
            .public_call_async(
                "session/shellCommand",
                json!({
                    "sessionId": runtime.session_id,
                    "operationId": shell.operation_id,
                    "action": "interrupt",
                }),
            )
            .await;
        state.prompt_queue.pause();
        if let Err(error) = result {
            state.push_diagnostic(format!("Shell interruption failed: {error}"));
        }
        retain_shell(&mut runtime.shell, state, shell);
        return;
    }
    shell.timed_out |= timed_out;
    let result = runtime
        .service
        .public_call_async(
            "shell/interrupt",
            json!({
                "sessionId": runtime.session_id,
                "operationId": shell.operation_id,
            }),
        )
        .await;
    if !timed_out {
        state.prompt_queue.pause();
    }
    match result {
        Ok(dispatch) => {
            if let Some(read) = shell_read(&dispatch.result) {
                settle_read(runtime, state, shell, read);
            } else {
                state.push_diagnostic("Shell interruption did not return typed process output");
                retain_shell(&mut runtime.shell, state, shell);
            }
        }
        Err(error) => {
            state.push_diagnostic(format!("Shell interruption failed: {error}"));
            retain_shell(&mut runtime.shell, state, shell);
        }
    }
}

/// Applies a read and, once the command has settled, hands its result to the
/// model the way reference `_dispatch_shell_command` does: through
/// `session/context/inject`, as context rather than as a message.
fn settle_read(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    shell: ActiveShell,
    read: ShellRead,
) {
    let session_id = runtime.session_id.clone();
    let Some(context) = apply_shell_read(&session_id, &mut runtime.shell, state, shell, read)
    else {
        return;
    };
    let limit = runtime
        .workspace
        .tool_config()
        .resolve("bash")
        .value("max_output_bytes")
        .and_then(toml::Value::as_integer)
        .and_then(|limit| usize::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_CONTEXT_LIMIT);
    if let Err(error) = runtime
        .service
        .inject_context(&session_id, &context.render(limit))
    {
        state.push_diagnostic(format!("Command failed: {error}"));
    }
}

/// What the model is told about a manual command once it has settled.
pub(super) struct ShellContext {
    command: String,
    working_directory: PathBuf,
    status: String,
    stdout: String,
    stderr: String,
}

impl ShellContext {
    /// The sections reference `manual_shell_context` reports, worded as the
    /// app-server words them for a `session/shellCommand`: the command, where
    /// it ran, how it ended, and each stream as the command wrote it, capped
    /// at `limit` characters.
    fn render(&self, limit: usize) -> String {
        let stdout = capped(&self.stdout, limit);
        let stderr = capped(&self.stderr, limit);
        let mut sections = vec![
            "The user ran a `!` command by hand. Its result is context for you, not a request."
                .to_owned(),
            format!("Command: `{}`", self.command),
            format!("Working directory: `{}`", self.working_directory.display()),
            self.status.clone(),
        ];
        for (label, text) in [("Stdout", &stdout), ("Stderr", &stderr)] {
            if !text.is_empty() {
                sections.push(format!("{label}:\n```text\n{}\n```", text.trim_end()));
            }
        }
        if stdout.is_empty() && stderr.is_empty() {
            sections.push("Output:\n```text\n(no output)\n```".to_owned());
        }
        sections.join("\n\n")
    }
}

/// Reference `_cap_output`: `text` cut to `limit` characters, the cut marked.
fn capped(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}\n... [truncated]", &text[..cut]),
        None => text.to_owned(),
    }
}

pub(super) fn apply_shell_read(
    session_id: &str,
    shell_slot: &mut Option<ActiveShell>,
    state: &mut TuiState,
    mut shell: ActiveShell,
    read: ShellRead,
) -> Option<ShellContext> {
    shell.chunks.extend(read.chunks);
    if let Some(entry) = shell_entry(session_id, &shell, &read.state) {
        let _ = state.replace_local(&shell.transcript_id, entry);
    }
    let status = match &read.state {
        ShellState::Running => {
            retain_shell(shell_slot, state, shell);
            return None;
        }
        // A command that never started reaches the model as nothing, which is
        // where the reference's raised failure leaves it.
        ShellState::Failed { .. } => {
            state.waiting = false;
            return None;
        }
        ShellState::Interrupted if shell.timed_out => "Status: timed out".to_owned(),
        ShellState::Interrupted => "Status: interrupted by the user".to_owned(),
        ShellState::Exited { code, success } => {
            format!("Exit code: {}", exit_status(*code, *success))
        }
    };
    state.waiting = false;
    Some(ShellContext {
        stdout: decoded(&shell.chunks, ProcessStream::Stdout),
        stderr: decoded(&shell.chunks, ProcessStream::Stderr),
        command: shell.command,
        working_directory: shell.working_directory,
        status,
    })
}

fn retain_shell(
    shell_slot: &mut Option<ActiveShell>,
    state: &mut TuiState,
    mut shell: ActiveShell,
) {
    shell.last_poll = Instant::now();
    *shell_slot = Some(shell);
    state.waiting = true;
}

fn shell_params(session_id: &str, operation_id: &str, command: &str) -> Value {
    json!({
        "sessionId": session_id,
        "operationId": operation_id,
        "command": command,
    })
}

fn shell_read(result: &std::collections::BTreeMap<String, Value>) -> Option<ShellRead> {
    serde_json::from_value(result.get("shell")?.get("output")?.clone()).ok()
}

/// Reference `ShellRunResponse.exit_code`: the process's own code, or 1 for a
/// failure that reported none.
fn exit_status(code: Option<i32>, success: bool) -> i32 {
    code.unwrap_or(if success { 0 } else { 1 })
}

/// The `shell` effect entry reference `_ManualShellEffect` projects for one
/// manual command: `shell_effect_detail` for the call and
/// `shell_effect_state` for its settled result
/// (`vibe/app_server/_shell.py`).
fn shell_entry(
    session_id: &str,
    shell: &ActiveShell,
    process_state: &ShellState,
) -> Option<TranscriptEntry> {
    let command = &shell.command;
    let output = sanitized_process_output(&shell.chunks);
    let duration_ms = u64::try_from(shell.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let failed = |message: String| {
        json!({
            "status": "failed",
            "error": {"message": message},
            "outputText": output,
            "durationMs": duration_ms,
            "display": {"success": false, "message": message},
        })
    };
    let effect_state = match process_state {
        ShellState::Running => json!({"status": "running", "outputText": output}),
        ShellState::Interrupted if shell.timed_out => failed("Command timed out".to_owned()),
        ShellState::Interrupted => json!({
            "status": "cancelled",
            "reason": "Command interrupted",
            "outputText": output,
            "durationMs": duration_ms,
            "display": {"success": false, "message": "Command interrupted"},
        }),
        ShellState::Exited { code, success } => match exit_status(*code, *success) {
            0 => json!({
                "status": "completed",
                "output": {"stdout": output, "stderr": "", "output": output},
                "outputText": output,
                "durationMs": duration_ms,
                "display": {"success": true, "message": format!("Ran {command}")},
            }),
            status => failed(format!("Command exited with status {status}")),
        },
        ShellState::Failed { message } => failed(message.clone()),
    };
    let now = vibe_core::clock::now_millis();
    let running = matches!(process_state, ShellState::Running);
    let entry = json!({
        "type": "effect",
        "id": shell.operation_id,
        "sessionId": session_id,
        "createdAt": now,
        "updatedAt": now,
        "generationStatus": if running { "in_progress" } else { "completed" },
        "title": MANUAL_SHELL_TOOL_NAME,
        "detail": {
            "kind": "shell",
            "toolName": MANUAL_SHELL_TOOL_NAME,
            "display": {
                "summary": format!("{MANUAL_SHELL_TOOL_NAME}: {command}"),
                "verb": "Running",
                "message": command,
                "settledVerb": "Ran",
                "settledMessage": command,
                "statusText": "Running command",
            },
            "input": {"command": command},
        },
        "state": effect_state,
    });
    serde_json::from_value::<PublicHistoryEntry>(entry)
        .ok()
        .map(history_entry)
}

/// The captured output in cursor order, decoded and cleaned as every command
/// output is ([`clean_output`]).
fn sanitized_process_output(chunks: &[ProcessChunk]) -> String {
    clean_output(&String::from_utf8_lossy(&in_cursor_order(chunks.iter())))
}

/// What the command wrote to `stream`, decoded and otherwise untouched, which
/// is how the model reads it.
fn decoded(chunks: &[ProcessChunk], stream: ProcessStream) -> String {
    let bytes = in_cursor_order(chunks.iter().filter(|chunk| chunk.stream == stream));
    String::from_utf8_lossy(&bytes).into_owned()
}

fn in_cursor_order<'a>(chunks: impl Iterator<Item = &'a ProcessChunk>) -> Vec<u8> {
    let mut chunks = chunks.collect::<Vec<_>>();
    chunks.sort_by_key(|chunk| chunk.cursor);
    chunks
        .into_iter()
        .flat_map(|chunk| chunk.bytes.iter().copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::{EntryStatus, TranscriptKind};
    use crate::tui::transcript::{Region, region};

    fn running_shell(state: &mut TuiState, command: &str) -> ActiveShell {
        let mut shell = ActiveShell::new(command, "operation", String::new(), "/workspace");
        let entry = shell_entry("session", &shell, &ShellState::Running).expect("entry projects");
        shell.transcript_id = state.append_local(entry);
        shell
    }

    fn read(state: ShellState, bytes: &[u8]) -> ShellRead {
        ShellRead {
            chunks: if bytes.is_empty() {
                Vec::new()
            } else {
                vec![ProcessChunk {
                    cursor: 0,
                    stream: ProcessStream::Stdout,
                    bytes: bytes.to_vec(),
                }]
            },
            state,
        }
    }

    #[test]
    fn shell_output_strips_terminal_control_sequences() {
        let chunks = vec![ProcessChunk {
            cursor: 0,
            stream: ProcessStream::Stdout,
            bytes: b"plain \x1b[31mred\x1b[0m\n\x1b]0;title\x07done".to_vec(),
        }];

        assert_eq!(sanitized_process_output(&chunks), "plain red\ndone");
    }

    /// Reference `shell_effect_detail` and `shell_effect_state`: the manual
    /// command is a `shell` effect whose settled header reads `Ran <command>`.
    #[test]
    fn a_manual_command_projects_the_reference_shell_effect() {
        let mut state = TuiState::new("session");
        let shell = running_shell(&mut state, "printf ok");
        let entry = &state.entries[0];
        assert_eq!(entry.kind, TranscriptKind::Effect);
        assert_eq!(entry.status, EntryStatus::Streaming);
        let Some(PublicHistoryEntry::Effect { detail, .. }) = entry.source.server() else {
            panic!("the manual command is an effect entry");
        };
        assert_eq!(detail.tool_name, MANUAL_SHELL_TOOL_NAME);
        assert_eq!(detail.display.summary, "shell: printf ok");
        assert_eq!(detail.display.status_text, "Running command");

        let mut slot = None;
        let context = apply_shell_read(
            "session",
            &mut slot,
            &mut state,
            shell,
            read(
                ShellState::Exited {
                    code: Some(0),
                    success: true,
                },
                b"ok",
            ),
        )
        .expect("a settled command reaches the model");
        assert_eq!(state.entries[0].status, EntryStatus::Completed);
        let Region::Effect(effect) = region(&state.entries[0]) else {
            panic!("the settled command renders as an effect");
        };
        assert_eq!(effect.header_text(), "Ran printf ok");
        let rendered = context.render(DEFAULT_CONTEXT_LIMIT);
        assert!(rendered.contains("Command: `printf ok`"));
        assert!(rendered.contains("Exit code: 0"));
        assert!(rendered.contains("Stdout:\n```text\nok\n```"));
        assert!(!rendered.contains("Stderr:"));
    }

    /// Reference `set_stream_message`: a running manual command shows what its
    /// output last appended, and settling drops it.
    #[test]
    fn a_running_command_streams_what_its_output_last_appended() {
        let mut state = TuiState::new("session");
        let shell = running_shell(&mut state, "tail -f log.txt");
        let id = shell.transcript_id.clone();
        let mut slot = None;
        let _ = apply_shell_read(
            "session",
            &mut slot,
            &mut state,
            shell,
            read(ShellState::Running, b"a logged line\n"),
        );
        assert_eq!(
            state.effect_streams.get(&id).map(String::as_str),
            Some("a logged line\n")
        );

        let shell = slot.take().expect("a running command keeps its owner");
        let _ = apply_shell_read(
            "session",
            &mut slot,
            &mut state,
            shell,
            read(ShellState::Interrupted, b""),
        );
        assert!(state.effect_streams.is_empty());
    }

    #[test]
    fn failed_timed_out_and_interrupted_commands_settle_as_the_reference_does() {
        let cases = [
            (
                ShellState::Exited {
                    code: Some(2),
                    success: false,
                },
                false,
                EntryStatus::Failed,
                "Command exited with status 2",
            ),
            (
                ShellState::Interrupted,
                true,
                EntryStatus::Failed,
                "Command timed out",
            ),
            (
                ShellState::Interrupted,
                false,
                EntryStatus::Cancelled,
                "Command interrupted",
            ),
        ];
        for (process_state, timed_out, status, message) in cases {
            let mut state = TuiState::new("session");
            let mut shell = running_shell(&mut state, "false");
            shell.timed_out = timed_out;
            let mut slot = None;
            let context = apply_shell_read(
                "session",
                &mut slot,
                &mut state,
                shell,
                read(process_state, b""),
            );
            assert!(context.is_some());
            assert_eq!(state.entries[0].status, status, "{message}");
            let Some(PublicHistoryEntry::Effect { state: effect, .. }) =
                state.entries[0].source.server()
            else {
                panic!("the command stays an effect entry");
            };
            let display = serde_json::to_value(effect).expect("state serializes");
            assert_eq!(display["display"]["message"], message);
        }
    }

    /// Reference `manual_shell_context`: each stream in a section of its own,
    /// as written and capped on its own, and one placeholder when both are
    /// empty.
    #[test]
    fn the_model_reads_each_stream_raw_and_capped() {
        let context = ShellContext {
            command: "yes".to_owned(),
            working_directory: PathBuf::from("/workspace"),
            status: "Exit code: 0".to_owned(),
            stdout: "y\n".repeat(10),
            stderr: "warning\n".to_owned(),
        };
        let rendered = context.render(4);
        assert!(rendered.contains("Stdout:\n```text\ny\ny\n\n... [truncated]\n```"));
        assert!(rendered.contains("Stderr:\n```text\nwarn\n... [truncated]\n```"));
        assert!(!rendered.contains("(no output)"));
        let empty = ShellContext {
            stdout: String::new(),
            stderr: String::new(),
            ..context
        };
        assert!(
            empty
                .render(4)
                .contains("Output:\n```text\n(no output)\n```")
        );
    }

    #[test]
    fn a_settled_command_splits_its_streams_for_the_model() {
        let mut state = TuiState::new("session");
        let shell = running_shell(&mut state, "make");
        let chunk = |cursor, stream, bytes: &[u8]| ProcessChunk {
            cursor,
            stream,
            bytes: bytes.to_vec(),
        };
        let mut slot = None;
        let context = apply_shell_read(
            "session",
            &mut slot,
            &mut state,
            shell,
            ShellRead {
                chunks: vec![
                    chunk(2, ProcessStream::Stdout, b"two\n"),
                    chunk(1, ProcessStream::Stderr, b"\x1b[1mwarn\x1b[0m\n"),
                    chunk(0, ProcessStream::Stdout, b"one\n"),
                ],
                state: ShellState::Exited {
                    code: Some(0),
                    success: true,
                },
            },
        )
        .expect("a settled command reaches the model");
        assert_eq!(context.stdout, "one\ntwo\n");
        assert_eq!(context.stderr, "\x1b[1mwarn\x1b[0m\n");
    }

    #[test]
    fn late_output_cannot_revive_a_settled_command() {
        let mut state = TuiState::new("session");
        let mut shell = running_shell(&mut state, "printf test");
        shell.chunks.push(ProcessChunk {
            cursor: 0,
            stream: ProcessStream::Stdout,
            bytes: b"partial".to_vec(),
        });
        let entry =
            shell_entry("session", &shell, &ShellState::Interrupted).expect("entry projects");
        state
            .replace_local(&shell.transcript_id, entry)
            .expect("entry exists");
        let cancelled = state.entries[0].clone();
        shell.chunks.push(ProcessChunk {
            cursor: 1,
            stream: ProcessStream::Stdout,
            bytes: b" late".to_vec(),
        });
        let late = shell_entry("session", &shell, &ShellState::Running).expect("entry projects");
        state
            .replace_local(&shell.transcript_id, late)
            .expect("entry exists");
        assert_eq!(state.entries[0], cancelled);
        assert_eq!(state.entries[0].status, EntryStatus::Cancelled);
    }

    #[test]
    fn running_shell_state_keeps_the_single_owner() {
        let mut state = TuiState::new("session");
        let shell = running_shell(&mut state, "test");
        let mut slot = None;

        retain_shell(&mut slot, &mut state, shell);

        assert!(slot.is_some());
        assert!(state.waiting);
        let shell = slot.take().expect("shell owner is retained");
        let context = apply_shell_read(
            "session",
            &mut slot,
            &mut state,
            shell,
            read(ShellState::Running, b"partial"),
        );
        assert!(context.is_none());
        assert!(slot.is_some());
        assert!(state.waiting);
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn terminal_shell_state_releases_the_owner() {
        let mut state = TuiState::new("session");
        state.waiting = true;
        let shell = running_shell(&mut state, "true");
        let mut slot = None;

        apply_shell_read(
            "session",
            &mut slot,
            &mut state,
            shell,
            read(
                ShellState::Exited {
                    code: Some(0),
                    success: true,
                },
                b"",
            ),
        );

        assert!(slot.is_none());
        assert!(!state.waiting);
        assert_eq!(state.entries[0].status, EntryStatus::Completed);
    }
}
