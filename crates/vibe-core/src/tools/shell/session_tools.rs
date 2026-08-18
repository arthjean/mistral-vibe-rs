//! The four tools a managed session is addressed through.
//!
//! `<family>_output` polls a session's log, `<family>_stdin` feeds it,
//! `<family>_sessions` lists, inspects, kills and resets, and
//! `<family>_log_file` reads or writes a log by path. All four answer for a
//! session that is still running and for one a previous process left behind,
//! which is why they read through [`SessionHandle`] rather than through a live
//! terminal.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::tools::{ToolError, ToolExecutionOutput, ToolOutputSink};

use super::decode::{read_file_window, skip_utf8_continuation_prefix};
use super::host::ShellFamily;
use super::policy::{byte_limit, string_argument};
use super::session::{
    ManagedSession, SessionHandle, SessionLimits, SessionShell, SessionStatus,
    kill_managed_session, managed_session, session_handle, session_output,
};
use super::specs::CONTROL_KEYS;
use super::{PUMP_INTERVAL, process_error};

// --------------------------------------------------------------------------
// <family>_output, _stdin, _sessions and _log_file
// --------------------------------------------------------------------------

pub(super) async fn run_output(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
    limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let session_id = string_argument(&arguments, "session_id")
        .unwrap_or_default()
        .to_owned();
    let cursor = arguments["cursor"].as_u64().unwrap_or(0);
    let limit = byte_limit(&arguments, &output, limits.max_inline_bytes);
    // Reference `read_output` answers an orphan from its manifest and its log,
    // which is what makes a build a previous process left running readable
    // rather than lost, and it answers it without waiting: nothing is still
    // writing to that log.
    let handle = session_handle(&shell, &session_id).await?;
    if let SessionHandle::Live(session) = &handle {
        let wait = arguments["wait_seconds"]
            .as_f64()
            .unwrap_or(0.0)
            .clamp(0.0, limits.max_poll_seconds);
        let deadline = Instant::now() + Duration::from_secs_f64(wait);
        // Reference `read_output` waits for the session to exit, for a full
        // window to accumulate, or for the deadline. Returning on the first
        // byte instead would answer a poll of an interactive session with the
        // echo of what was just written to it rather than with what the program
        // then said.
        while Instant::now() < deadline
            && session.is_running()
            && log_size(&session.log_path).saturating_sub(cursor) < limit as u64
        {
            tokio::time::sleep(PUMP_INTERVAL).await;
        }
    }
    session_output(&handle, cursor, limit)
}

pub(super) fn log_size(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default()
}

pub(super) async fn run_stdin(
    shell: Arc<SessionShell>,
    arguments: Value,
    _output: ToolOutputSink,
    _limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let session_id = string_argument(&arguments, "session_id")
        .unwrap_or_default()
        .to_owned();
    // The payload is decoded before the session is even looked up, so a
    // malformed one never reaches a process.
    let bytes = stdin_bytes(&arguments)?;
    let session = managed_session(&shell, &session_id).await?;
    if !session.is_running() {
        return Err(ToolError::Execution(format!(
            "session `{session_id}` has exited and no longer reads input"
        )));
    }
    shell
        .terminals
        .write(&session.terminal_id, &bytes)
        .await
        .map_err(process_error)?;
    Ok(ToolExecutionOutput::new(format!(
        "Wrote {} bytes to session {session_id}",
        bytes.len()
    ))
    .displayed_as(json!({"kind": "shell", "command": session.command}))
    .typed(json!({
        "sessionId": session_id,
        "bytesWritten": bytes.len(),
        "status": session.snapshot().0.as_str(),
    })))
}

/// The bytes one stdin call writes.
///
/// The reference model accepts exactly one of the three inputs and rejects
/// anything else, so there is no precedence to apply.
pub(super) fn stdin_bytes(arguments: &Value) -> Result<Vec<u8>, ToolError> {
    let text = string_argument(arguments, "text");
    let control = arguments
        .get("control")
        .and_then(Value::as_array)
        .filter(|keys| !keys.is_empty());
    let encoded = string_argument(arguments, "bytes_base64");
    let supplied = usize::from(text.is_some())
        + usize::from(control.is_some())
        + usize::from(encoded.is_some());
    if supplied != 1 {
        return Err(ToolError::SchemaViolation {
            path: "/".to_owned(),
            message: "supply exactly one of text, control or bytes_base64".to_owned(),
        });
    }
    if let Some(text) = text {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(keys) = control {
        let mut bytes = Vec::new();
        for (index, key) in keys.iter().enumerate() {
            let name = key.as_str().unwrap_or_default();
            let Some((_, sequence)) = CONTROL_KEYS.iter().find(|(known, _)| *known == name) else {
                return Err(ToolError::SchemaViolation {
                    path: format!("/control/{index}"),
                    message: format!("`{name}` is not a control key"),
                });
            };
            bytes.extend_from_slice(sequence);
        }
        return Ok(bytes);
    }
    let encoded = encoded.unwrap_or_default();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| ToolError::SchemaViolation {
            path: "/bytes_base64".to_owned(),
            message: format!("is not valid base64: {error}"),
        })
}

pub(super) async fn run_sessions(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
    limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = string_argument(&arguments, "action").unwrap_or("list");
    let limit = byte_limit(&arguments, &output, limits.max_inline_bytes);
    match action {
        "list" => {
            let sessions = shell.managed.lock().await;
            let mut infos = sessions
                .values()
                .map(|session| session.info())
                .collect::<Vec<_>>();
            // Reference `list_sessions` appends the orphans a live session does
            // not shadow, so a client that restarted still sees what it left.
            infos.extend(shell.orphans().into_iter().filter(|manifest| {
                manifest
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_none_or(|id| !sessions.contains_key(id))
            }));
            infos.sort_by(|left, right| {
                created_at(left)
                    .cmp(&created_at(right))
                    .then_with(|| session_id_of(left).cmp(&session_id_of(right)))
            });
            Ok(ToolExecutionOutput::new(format!(
                "{} {} sessions",
                infos.len(),
                shell.family.name()
            ))
            .displayed_as(json!({
                "kind": "shell",
                "command": shell.family.tool_name("sessions list"),
            }))
            .typed(json!({"action": "list", "sessions": infos})))
        }
        "inspect" => {
            // Reference `inspect_session` positions the window at the end of
            // the log rather than at its start: an inspection reports what a
            // session is doing now, not how it began.
            let handle = required_handle(&shell, &arguments, "inspect").await?;
            let info = handle.info();
            let log_path = handle.log_path();
            let cursor = skip_utf8_continuation_prefix(
                &log_path,
                log_size(&log_path).saturating_sub(limit as u64),
            );
            let (output, next_cursor, truncated) =
                read_file_window(&log_path, cursor, limit, handle.is_running())?;
            let command = info
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok(ToolExecutionOutput::new(output.clone())
                .displayed_as(json!({"kind": "shell", "command": command}))
                .typed(json!({
                    "action": "inspect",
                    "session": info,
                    "output": output,
                    "nextCursor": next_cursor,
                    "truncated": truncated,
                })))
        }
        "kill" => {
            // A session is owned by the Vibe session, not by the turn that
            // started it, which is what the reference does: any turn may
            // stop any session of the family.
            let session = required_session(&shell, &arguments, "kill").await?;
            kill_managed_session(&shell, &session, SessionStatus::Killed).await?;
            shell.managed.lock().await.remove(&session.id);
            Ok(
                ToolExecutionOutput::new(format!("Killed session {}", session.id))
                    .displayed_as(json!({"kind": "shell", "command": session.command}))
                    .typed(json!({"action": "kill", "session": session.info()})),
            )
        }
        "reset" => {
            let sessions = shell
                .managed
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut killed = Vec::new();
            for session in &sessions {
                if session.is_running() {
                    kill_managed_session(&shell, session, SessionStatus::Killed).await?;
                }
                killed.push(session.info());
            }
            shell.managed.lock().await.clear();
            if arguments["clear_logs"].as_bool().unwrap_or(false) {
                // Reference `reset` clears the orphans with the logs, because
                // the manifests it would read them back from are what it just
                // deleted.
                for session in &sessions {
                    let _ = std::fs::remove_file(&session.log_path);
                    let _ = std::fs::remove_file(&session.manifest_path);
                }
                for manifest in shell.orphans() {
                    if let Some(id) = manifest.get("sessionId").and_then(Value::as_str) {
                        let directory = shell.sessions_directory();
                        let _ = std::fs::remove_file(directory.join(format!("{id}.log")));
                        let _ = std::fs::remove_file(directory.join(format!("{id}.json")));
                    }
                }
                if let Ok(mut orphaned) = shell.orphaned.lock() {
                    orphaned.clear();
                }
            }
            Ok(ToolExecutionOutput::new(format!(
                "Stopped {} {} sessions",
                killed.len(),
                shell.family.name()
            ))
            .displayed_as(json!({
                "kind": "shell",
                "command": shell.family.tool_name("sessions reset"),
            }))
            .typed(json!({"action": "reset", "sessions": killed})))
        }
        other => Err(ToolError::Execution(format!(
            "unknown {} action `{other}`; use `list`, `inspect`, `kill` or `reset`",
            shell.family.tool_name("sessions")
        ))),
    }
}

async fn required_session(
    shell: &SessionShell,
    arguments: &Value,
    action: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    managed_session(shell, required_session_id(arguments, action)?).await
}

async fn required_handle(
    shell: &SessionShell,
    arguments: &Value,
    action: &str,
) -> Result<SessionHandle, ToolError> {
    session_handle(shell, required_session_id(arguments, action)?).await
}

fn required_session_id<'a>(arguments: &'a Value, action: &str) -> Result<&'a str, ToolError> {
    string_argument(arguments, "session_id").ok_or_else(|| ToolError::SchemaViolation {
        path: "/session_id".to_owned(),
        message: format!("is required by the `{action}` action"),
    })
}

/// When a listed session was created, which is the order the reference lists
/// them in. A manifest that lost the field sorts first rather than failing.
fn created_at(info: &Value) -> u128 {
    info.get("createdAtMs")
        .and_then(Value::as_str)
        .and_then(|stamp| stamp.parse().ok())
        .unwrap_or_default()
}

fn session_id_of(info: &Value) -> String {
    info.get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(super) async fn run_log_file(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
    limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = string_argument(&arguments, "action").unwrap_or_default();
    let path = resolve_log_path(&shell, &arguments)?;
    match action {
        "read" => {
            let offset = arguments["offset"].as_u64().unwrap_or(0);
            let limit = byte_limit(&arguments, &output, limits.max_inline_bytes);
            // Reference `read_log_file` trims a cut character only while the
            // session behind the path is still writing to it.
            let running = is_running_log(&shell, &path).await;
            let (content, next_cursor, truncated) =
                read_file_window(&path, offset, limit, running)?;
            Ok(ToolExecutionOutput::new(content.clone())
                .displayed_as(json!({
                    "kind": "shell",
                    "command": shell.family.tool_name("log_file read"),
                }))
                .typed(json!({
                    "action": "read",
                    "path": path.to_string_lossy(),
                    "content": content,
                    "nextCursor": next_cursor,
                    "truncated": truncated,
                })))
        }
        "write" | "append" => {
            refuse_live_session_log(&shell, &path).await?;
            let content = string_argument(&arguments, "content").unwrap_or_default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ToolError::Execution(format!(
                        "`{}` cannot be created: {error}",
                        parent.display()
                    ))
                })?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(action == "append")
                .truncate(action == "write")
                .open(&path)
                .map_err(|error| {
                    ToolError::Execution(format!("`{}` cannot be written: {error}", path.display()))
                })?;
            file.write_all(content.as_bytes()).map_err(|error| {
                ToolError::Execution(format!("`{}` cannot be written: {error}", path.display()))
            })?;
            Ok(ToolExecutionOutput::new(format!(
                "Wrote {} bytes to {}",
                content.len(),
                path.display()
            ))
            .displayed_as(json!({
                "kind": "shell",
                "command": shell.family.tool_name(&format!("log_file {action}")),
            }))
            .typed(json!({
                "action": action,
                "path": path.to_string_lossy(),
                "bytesWritten": content.len(),
            })))
        }
        other => Err(ToolError::Execution(format!(
            "unknown {} action `{other}`; use `read`, `write` or `append`",
            shell.family.tool_name("log_file")
        ))),
    }
}

/// The file a `<family>_log_file` call addresses.
///
/// A session id resolves to that session's own log. A relative path is joined
/// to the shell-tool directory and refused before any filesystem access when it
/// climbs out of it or names another family's session file.
pub(super) fn resolve_log_path(
    shell: &SessionShell,
    arguments: &Value,
) -> Result<PathBuf, ToolError> {
    if let Some(session_id) = string_argument(arguments, "session_id") {
        // A session id names a file inside the session directory, so it is held
        // to the same rule as a relative path: one component, this family's.
        if !is_family_session_id(shell.family, session_id) {
            return Err(ToolError::Execution(format!(
                "the log path must name a {} session file",
                shell.family.name()
            )));
        }
        return Ok(shell.sessions_directory().join(format!("{session_id}.log")));
    }
    let Some(relative) = string_argument(arguments, "relative_path") else {
        return Err(ToolError::SchemaViolation {
            path: "/relative_path".to_owned(),
            message: "is required when session_id is absent".to_owned(),
        });
    };
    let candidate = Path::new(relative);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ToolError::Execution(
            "the log path escapes the session log directory".to_owned(),
        ));
    }
    let resolved = shell.log_root.join(candidate);
    // A file directly under `sessions/` belongs to a shell family, and this
    // tool answers only for its own.
    if resolved.parent() == Some(shell.sessions_directory().as_path())
        && !resolved
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".log"))
            .is_some_and(|name| is_family_session_id(shell.family, name))
    {
        return Err(ToolError::Execution(format!(
            "the log path must name a {} session file",
            shell.family.name()
        )));
    }
    Ok(resolved)
}

/// Whether `candidate` is one plain name belonging to `family`.
pub(super) fn is_family_session_id(family: ShellFamily, candidate: &str) -> bool {
    let mut components = Path::new(candidate).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && candidate.starts_with(&format!("{}_", family.name()))
}

/// Whether `path` is the log of a session still writing to it.
async fn is_running_log(shell: &SessionShell, path: &Path) -> bool {
    shell
        .managed
        .lock()
        .await
        .values()
        .any(|session| session.log_path == path && session.is_running())
}

async fn refuse_live_session_log(shell: &SessionShell, path: &Path) -> Result<(), ToolError> {
    if is_running_log(shell, path).await {
        return Err(ToolError::Execution(format!(
            "a live session log cannot be written; use {} or wait for the session to exit",
            shell.family.tool_name("stdin")
        )));
    }
    Ok(())
}
