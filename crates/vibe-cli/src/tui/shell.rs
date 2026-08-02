use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;

use super::state::{EntryStatus, TuiState};
use super::{CliError, InteractiveRuntime, append_local_notice};

const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) struct ActiveShell {
    command: String,
    operation_id: String,
    transcript_id: String,
    chunks: Vec<ShellChunk>,
    backpressure_dropped: bool,
    last_poll: Instant,
}

impl ActiveShell {
    pub(super) fn new(
        command: impl Into<String>,
        operation_id: impl Into<String>,
        transcript_id: impl Into<String>,
    ) -> Self {
        Self {
            command: command.into(),
            operation_id: operation_id.into(),
            transcript_id: transcript_id.into(),
            chunks: Vec::new(),
            backpressure_dropped: false,
            last_poll: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ShellRead {
    chunks: Vec<ShellChunk>,
    state: ShellState,
    backpressure_dropped: bool,
}

impl ShellRead {
    #[cfg(test)]
    pub(super) fn running(cursor: u64, text: impl Into<Vec<u8>>) -> Self {
        Self {
            chunks: vec![ShellChunk {
                cursor,
                bytes: text.into(),
            }],
            state: ShellState::Running,
            backpressure_dropped: false,
        }
    }

    #[cfg(test)]
    pub(super) fn interrupted() -> Self {
        Self {
            chunks: Vec::new(),
            state: ShellState::Interrupted { code: None },
            backpressure_dropped: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellChunk {
    cursor: u64,
    bytes: Vec<u8>,
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
    Interrupted {
        #[serde(default)]
        code: Option<i32>,
    },
    Failed {
        message: String,
    },
}

pub(super) async fn start_shell(
    input: &str,
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
    let operation_id = format!(
        "manual-shell-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    );
    let params = shell_params(&runtime.session_id, &operation_id, command);
    if let Err(error) = runtime.service.public_call_async("shell/run", params).await {
        state.push_diagnostic(error.to_string());
        return Ok(true);
    }
    state.waiting = true;
    let transcript_id = append_local_notice(
        state,
        &format!("Running shell command: `{command}`"),
        EntryStatus::Streaming,
    );
    runtime.shell = Some(ActiveShell::new(command, operation_id, transcript_id));
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
    let params = shell_params(&runtime.session_id, &shell.operation_id, &shell.command);
    let dispatch = match runtime.service.public_call_async("shell/run", params).await {
        Ok(dispatch) => dispatch,
        Err(error) => {
            state.push_diagnostic(format!("Shell command failed: {error}"));
            retain_shell(&mut runtime.shell, state, shell);
            return;
        }
    };
    let Some(read) = shell_read(&dispatch.result) else {
        state.push_diagnostic("Shell response did not contain typed process output");
        retain_shell(&mut runtime.shell, state, shell);
        return;
    };
    apply_shell_read(&mut runtime.shell, state, shell, read);
}

pub(super) async fn interrupt_shell(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(shell) = runtime.shell.take() else {
        return;
    };
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
    state.prompt_queue.pause();
    match result {
        Ok(dispatch) => {
            if let Some(read) = shell_read(&dispatch.result) {
                apply_shell_read(&mut runtime.shell, state, shell, read);
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

pub(super) fn apply_shell_read(
    shell_slot: &mut Option<ActiveShell>,
    state: &mut TuiState,
    mut shell: ActiveShell,
    read: ShellRead,
) {
    shell.chunks.extend(read.chunks);
    shell.backpressure_dropped |= read.backpressure_dropped;
    update_shell_entry(state, &shell, &read.state);
    if matches!(read.state, ShellState::Running) {
        retain_shell(shell_slot, state, shell);
    } else {
        state.waiting = false;
    }
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

fn shell_params(session_id: &str, operation_id: &str, command: &str) -> serde_json::Value {
    json!({
        "sessionId": session_id,
        "operationId": operation_id,
        "command": command,
    })
}

fn shell_read(result: &std::collections::BTreeMap<String, serde_json::Value>) -> Option<ShellRead> {
    serde_json::from_value(result.get("shell")?.get("output")?.clone()).ok()
}

fn update_shell_entry(state: &mut TuiState, shell: &ActiveShell, process_state: &ShellState) {
    let output = sanitized_process_output(&shell.chunks);
    let status = match process_state {
        ShellState::Exited { success: true, .. } => EntryStatus::Completed,
        ShellState::Running => EntryStatus::Streaming,
        ShellState::Interrupted { .. } => EntryStatus::Cancelled,
        ShellState::Exited { success: false, .. } | ShellState::Failed { .. } => {
            EntryStatus::Failed
        }
    };
    let rendered_output = if output.trim().is_empty() {
        "(no output)".to_owned()
    } else {
        output
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let failure = match process_state {
        ShellState::Exited {
            success: false,
            code,
        } => code.map_or_else(
            || "\n\nCommand exited unsuccessfully.".to_owned(),
            |code| format!("\n\nCommand exited with code {code}."),
        ),
        ShellState::Interrupted { code } => code.map_or_else(
            || "\n\nCommand was interrupted.".to_owned(),
            |code| format!("\n\nCommand was interrupted with code {code}."),
        ),
        ShellState::Failed { message } => format!("\n\n{message}"),
        _ => String::new(),
    };
    let truncation = if shell.backpressure_dropped {
        "\n\nOutput was truncated by process backpressure."
    } else {
        ""
    };
    let _ = state.update_local(
        &shell.transcript_id,
        format!(
            "### Shell\n\n`$ {}`\n\n{rendered_output}{failure}{truncation}",
            shell.command
        ),
        status,
    );
}

fn sanitized_process_output(chunks: &[ShellChunk]) -> String {
    let mut chunks = chunks.to_vec();
    chunks.sort_by_key(|chunk| chunk.cursor);
    let bytes = chunks
        .into_iter()
        .flat_map(|chunk| chunk.bytes)
        .collect::<Vec<_>>();
    let mut sanitized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            let byte = bytes[index];
            if byte == b'\n' || byte == b'\t' || byte >= b' ' {
                sanitized.push(byte);
            }
            index += 1;
            continue;
        }
        index += 1;
        match bytes.get(index).copied() {
            Some(b'[') => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            Some(_) => index += 1,
            None => {}
        }
    }
    String::from_utf8_lossy(&sanitized).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_output_strips_terminal_control_sequences() {
        let chunks = vec![ShellChunk {
            cursor: 0,
            bytes: b"plain \x1b[31mred\x1b[0m\n\x1b]0;title\x07done".to_vec(),
        }];

        assert_eq!(sanitized_process_output(&chunks), "plain red\ndone");
    }

    #[test]
    fn shell_chunks_patch_one_entry_and_late_output_cannot_revive_cancellation() {
        let mut state = TuiState::new("session");
        let transcript_id = append_local_notice(
            &mut state,
            "Running shell command: `printf test`",
            EntryStatus::Streaming,
        );
        let mut shell = ActiveShell {
            command: "printf test".to_owned(),
            operation_id: "operation".to_owned(),
            transcript_id,
            chunks: vec![ShellChunk {
                cursor: 0,
                bytes: b"partial".to_vec(),
            }],
            backpressure_dropped: false,
            last_poll: Instant::now(),
        };

        update_shell_entry(&mut state, &shell, &ShellState::Running);
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries[0].text.contains("partial"));
        assert_eq!(state.entries[0].status, EntryStatus::Streaming);

        update_shell_entry(&mut state, &shell, &ShellState::Interrupted { code: None });
        let cancelled = state.entries[0].text.clone();
        shell.chunks.push(ShellChunk {
            cursor: 1,
            bytes: b" late".to_vec(),
        });
        update_shell_entry(&mut state, &shell, &ShellState::Running);
        assert_eq!(state.entries[0].status, EntryStatus::Cancelled);
        assert_eq!(state.entries[0].text, cancelled);
    }

    #[test]
    fn running_or_unreadable_shell_state_keeps_the_single_owner() {
        let mut state = TuiState::new("session");
        let transcript_id = append_local_notice(
            &mut state,
            "Running shell command: `test`",
            EntryStatus::Streaming,
        );
        let shell = ActiveShell {
            command: "test".to_owned(),
            operation_id: "operation".to_owned(),
            transcript_id,
            chunks: Vec::new(),
            backpressure_dropped: false,
            last_poll: Instant::now(),
        };
        let mut slot = None;

        retain_shell(&mut slot, &mut state, shell);

        assert!(slot.is_some());
        assert!(state.waiting);
        let shell = slot.take().expect("shell owner is retained");
        apply_shell_read(
            &mut slot,
            &mut state,
            shell,
            ShellRead {
                chunks: vec![ShellChunk {
                    cursor: 0,
                    bytes: b"partial".to_vec(),
                }],
                state: ShellState::Running,
                backpressure_dropped: false,
            },
        );
        assert!(slot.is_some());
        assert!(state.waiting);
    }

    #[test]
    fn terminal_shell_state_releases_the_owner() {
        let mut state = TuiState::new("session");
        state.waiting = true;
        let transcript_id = append_local_notice(
            &mut state,
            "Running shell command: `true`",
            EntryStatus::Streaming,
        );
        let shell = ActiveShell {
            command: "true".to_owned(),
            operation_id: "operation".to_owned(),
            transcript_id,
            chunks: Vec::new(),
            backpressure_dropped: false,
            last_poll: Instant::now(),
        };
        let mut slot = None;

        apply_shell_read(
            &mut slot,
            &mut state,
            shell,
            ShellRead {
                chunks: Vec::new(),
                state: ShellState::Exited {
                    code: Some(0),
                    success: true,
                },
                backpressure_dropped: false,
            },
        );

        assert!(slot.is_none());
        assert!(!state.waiting);
        assert_eq!(state.entries[0].status, EntryStatus::Completed);
    }
}
