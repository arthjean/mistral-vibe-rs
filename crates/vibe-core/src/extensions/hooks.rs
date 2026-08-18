//! Hooks: the programs an operator runs around a tool call or a turn.
//!
//! A hook reads its invocation on stdin, answers with JSON on stdout, and is
//! believed only if it exits cleanly. It may rewrite the tool's arguments,
//! append context, raise a notice, or deny the call outright. A hook is under
//! no obligation to read its stdin, which is why a broken pipe there is not a
//! failure: reference `_run_process` swallows it and reads the answer off
//! stdout and the exit status instead.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::time::timeout;

use super::{ExtensionError, HookKind, HookSpec, MAX_HOOK_OUTPUT_BYTES};
use crate::text::{matches_wildcard, truncate_utf8};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInvocation {
    pub kind: HookKind,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub payload: Value,
    pub output_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookNotice {
    pub hook: String,
    pub warning: bool,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookChainResult {
    pub invocation: HookInvocation,
    pub denied: Option<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Clone)]
pub struct HookManager {
    hooks: Arc<Mutex<Vec<HookSpec>>>,
    working_directory: PathBuf,
}

impl HookManager {
    #[must_use]
    pub fn new(hooks: Vec<HookSpec>, working_directory: PathBuf) -> Self {
        Self {
            hooks: Arc::new(Mutex::new(hooks)),
            working_directory,
        }
    }

    pub fn reload(&self, hooks: Vec<HookSpec>) -> Result<(), ExtensionError> {
        *self
            .hooks
            .lock()
            .map_err(|_| ExtensionError::StatePoisoned)? = hooks;
        Ok(())
    }

    pub async fn run(
        &self,
        mut invocation: HookInvocation,
    ) -> Result<HookChainResult, ExtensionError> {
        let hooks = self
            .hooks
            .lock()
            .map_err(|_| ExtensionError::StatePoisoned)?
            .clone();
        let mut notices = Vec::new();
        let mut denied = None;
        for hook in hooks.into_iter().filter(|hook| {
            hook.kind == invocation.kind
                && hook.matcher.as_deref().is_none_or(|matcher| {
                    invocation
                        .tool_name
                        .as_deref()
                        .is_some_and(|name| matches_wildcard(matcher, name))
                })
        }) {
            // Reference `hooks/manager.py` opens `hook_span` around the whole
            // run of one hook, retries included, so a hook that succeeded on
            // its second attempt is one span rather than two.
            let execution = crate::tracing::hook_span(
                crate::tracing::HookSpan {
                    hook_name: &hook.name,
                    hook_type: hook.kind.label(),
                    tool_name: invocation.tool_name.as_deref(),
                    tool_call_id: invocation.tool_call_id.as_deref(),
                },
                async {
                    let mut attempt = 0_u8;
                    loop {
                        let result =
                            execute_hook(&hook, &invocation, &self.working_directory).await;
                        if result.is_ok() || attempt >= hook.retries {
                            break result;
                        }
                        attempt = attempt.saturating_add(1);
                    }
                },
            )
            .await;
            match execution {
                Ok(response) => {
                    if let Some(message) = response.system_message {
                        notices.push(HookNotice {
                            hook: hook.name.clone(),
                            warning: false,
                            content: message,
                        });
                    }
                    if let Some(tool_input) = response.tool_input {
                        invocation.payload = tool_input;
                    }
                    if let Some(additional_context) = response.additional_context {
                        if !invocation.output_text.is_empty() {
                            invocation.output_text.push('\n');
                        }
                        invocation.output_text.push_str(&additional_context);
                    }
                    if response.decision == HookDecision::Deny {
                        denied =
                            Some(response.reason.unwrap_or_else(|| {
                                format!("hook `{}` denied execution", hook.name)
                            }));
                        break;
                    }
                }
                Err(error) => {
                    notices.push(HookNotice {
                        hook: hook.name.clone(),
                        warning: true,
                        content: error.to_string(),
                    });
                    if hook.strict {
                        denied = Some(format!("strict hook `{}` failed", hook.name));
                        break;
                    }
                }
            }
        }
        Ok(HookChainResult {
            invocation,
            denied,
            notices,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookDecision {
    Allow,
    Deny,
}

struct HookResponse {
    decision: HookDecision,
    reason: Option<String>,
    system_message: Option<String>,
    tool_input: Option<Value>,
    additional_context: Option<String>,
}

async fn execute_hook(
    hook: &HookSpec,
    invocation: &HookInvocation,
    working_directory: &Path,
) -> Result<HookResponse, ExtensionError> {
    let stdin = serde_json::to_vec(invocation).map_err(ExtensionError::Json)?;
    let mut command = Command::new(&hook.program);
    command
        .args(&hook.args)
        .current_dir(working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| ExtensionError::Process {
        program: hook.program.clone(),
        source,
    })?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| ExtensionError::HookProtocol("stdin pipe is unavailable".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExtensionError::HookProtocol("stdout pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExtensionError::HookProtocol("stderr pipe is unavailable".to_owned()))?;
    let stdout_task = tokio::spawn(drain_bounded(stdout, MAX_HOOK_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(drain_bounded(stderr, MAX_HOOK_OUTPUT_BYTES));
    // A hook is under no obligation to read its stdin, and one that does not
    // may well have exited before this write lands, which closes the read end
    // and breaks the pipe. Reference `_run_process` swallows `BrokenPipeError`
    // and `ConnectionResetError` around the whole write for exactly that
    // reason: the hook's own output and exit status are what answer for it.
    // Treating the broken pipe as a failure instead makes such a hook fail or
    // succeed depending on how loaded the machine is.
    let written = async {
        child_stdin.write_all(&stdin).await?;
        child_stdin.write_all(b"\n").await
    }
    .await;
    drop(child_stdin);
    if let Err(source) = written
        && !matches!(
            source.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    {
        return Err(ExtensionError::Process {
            program: hook.program.clone(),
            source,
        });
    }
    let status = match timeout(Duration::from_millis(hook.timeout_ms), child.wait()).await {
        Ok(result) => result.map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?,
        Err(_) => {
            child
                .kill()
                .await
                .map_err(|source| ExtensionError::Process {
                    program: hook.program.clone(),
                    source,
                })?;
            let _ = child.wait().await;
            return Err(ExtensionError::HookTimeout(hook.name.clone()));
        }
    };
    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|error| ExtensionError::HookProtocol(error.to_string()))?
        .map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|error| ExtensionError::HookProtocol(error.to_string()))?
        .map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?;
    if stdout_truncated || stderr_truncated {
        return Err(ExtensionError::HookOutputLimit(hook.name.clone()));
    }
    if !status.success() {
        return Err(ExtensionError::HookFailed {
            name: hook.name.clone(),
            status: status.code(),
            stderr: bounded_stderr(&stderr),
        });
    }
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(HookResponse {
            decision: HookDecision::Allow,
            reason: None,
            system_message: None,
            tool_input: None,
            additional_context: None,
        });
    }
    let value: Value = serde_json::from_slice(&stdout)
        .map_err(|error| ExtensionError::HookProtocol(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| ExtensionError::HookProtocol("response must be an object".to_owned()))?;
    let decision = match object
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("allow")
    {
        "allow" => HookDecision::Allow,
        "deny" => HookDecision::Deny,
        value => {
            return Err(ExtensionError::HookProtocol(format!(
                "unknown decision `{value}`"
            )));
        }
    };
    Ok(HookResponse {
        decision,
        reason: object
            .get("reason")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        system_message: object
            .get("system_message")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        tool_input: object
            .get("hook_specific_output")
            .and_then(Value::as_object)
            .and_then(|output| output.get("tool_input"))
            .cloned(),
        additional_context: object
            .get("hook_specific_output")
            .and_then(Value::as_object)
            .and_then(|output| output.get("additional_context"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

async fn drain_bounded(
    mut stream: impl AsyncRead + Unpin,
    maximum: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = maximum.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated))
}

fn bounded_stderr(bytes: &[u8]) -> String {
    truncate_utf8(&String::from_utf8_lossy(bytes), 1024).to_owned()
}
