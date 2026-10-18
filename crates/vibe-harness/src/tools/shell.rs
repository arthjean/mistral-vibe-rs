//! `file_system.bash`: one command through the session's shell, bounded in
//! time and in output.
//!
//! Reference `harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_shell_tools.py`:
//! the shell is the one the session's command environment resolves (see
//! [`HostShells::command_shell`]), the environment is told no terminal is
//! attached, each stream keeps its first 16 000 bytes, and a command past its
//! timeout loses its whole process group.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use vibe_core::platform::BackgroundCommand as _;
use vibe_core::tools::shell::HostShells;

use crate::state::ToolResult;

const MAX_OUTPUT_BYTES: usize = 16_000;
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

/// Where and how a command runs.
#[derive(Debug, Clone)]
pub struct ShellContext {
    pub cwd: PathBuf,
    /// What the host offers, which decides the command environment the shell
    /// tool and `process.*` share.
    pub host: HostShells,
    /// The configured shell, which wins over the search.
    pub shell: Option<String>,
    /// Variables the session adds on top of the process environment.
    pub env: BTreeMap<String, String>,
}

/// Runs a `file_system.bash` call.
pub async fn execute(arguments: &Value, context: &ShellContext) -> ToolResult {
    match run(arguments, context).await {
        Ok(structured) => ToolResult::Success {
            content: Vec::new(),
            structured_content: Some(structured),
            meta: None,
        },
        Err(message) => ToolResult::failed("tool_failed", message),
    }
}

async fn run(arguments: &Value, context: &ShellContext) -> Result<Value, String> {
    let command = match arguments.get("command") {
        Some(Value::String(command)) if !command.is_empty() => command.clone(),
        Some(Value::String(command)) => {
            return Err(validation_error(
                "command",
                "String should have at least 1 character",
                "string_too_short",
                &json!(command),
            ));
        }
        _ => {
            return Err(validation_error(
                "command",
                "Field required",
                "missing",
                arguments,
            ));
        }
    };
    let timeout = match arguments.get("timeout_seconds") {
        None => DEFAULT_TIMEOUT_SECONDS,
        Some(value) => value
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| {
                validation_error(
                    "timeout_seconds",
                    "Input should be greater than 0",
                    "greater_than",
                    value,
                )
            })?,
    };
    let env = shell_env(&context.env);
    let shell = context.host.command_shell(context.shell.as_deref(), &env)?;
    let mut process = tokio::process::Command::new(&shell.executable);
    process
        .in_background()
        .args(&shell.arguments)
        .arg(&command)
        .current_dir(&context.cwd)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().map_err(|error| io_message(&error))?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let gathered = async {
        let (out, err, status) =
            tokio::join!(read_bounded(stdout), read_bounded(stderr), child.wait());
        (out, err, status)
    };
    let outcome = tokio::time::timeout(Duration::from_secs(timeout), gathered).await;
    kill_group(pid);
    let ((stdout, stdout_truncated), (stderr, stderr_truncated), status) =
        outcome.map_err(|_| {
            format!(
                "Command timed out after {timeout}s: {}",
                crate::tool_projection::python_repr(&command)
            )
        })?;
    let status = status.map_err(|error| io_message(&error))?;
    Ok(json!({
        "command": command,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "returncode": return_code(status),
        "was_truncated": stdout_truncated || stderr_truncated,
    }))
}

/// `process.returncode or 0`: a signal reads as its negative number.
fn return_code(status: std::process::ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return i64::from(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -i64::from(signal);
        }
    }
    0
}

async fn read_bounded<R: AsyncRead + Unpin>(reader: Option<R>) -> (Vec<u8>, bool) {
    let Some(mut reader) = reader else {
        return (Vec::new(), false);
    };
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 4096];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            truncated = true;
        }
    }
    (kept, truncated)
}

fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = vibe_core::pty::kill_tree(pid);
    }
}

/// The environment a command sees: no terminal, no pager, no prompt.
#[must_use]
pub fn shell_env(extra: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = vibe_core::platform::unicode_vars().collect();
    env.insert("CI".to_owned(), "true".to_owned());
    env.insert("NONINTERACTIVE".to_owned(), "1".to_owned());
    env.insert("NO_TTY".to_owned(), "1".to_owned());
    env.extend(
        extra
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    if cfg!(windows) {
        env.insert("GIT_PAGER".to_owned(), "more".to_owned());
        env.insert("PAGER".to_owned(), "more".to_owned());
        return env;
    }
    env.remove("LC_ALL");
    for (key, value) in [
        ("TERM", "dumb"),
        ("DEBIAN_FRONTEND", "noninteractive"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        ("LESS", "-FX"),
        ("LC_CTYPE", "C.UTF-8"),
    ] {
        env.insert(key.to_owned(), value.to_owned());
    }
    env
}

fn io_message(error: &std::io::Error) -> String {
    let code = error.raw_os_error().unwrap_or(0);
    let text = error.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text).to_owned();
    format!("[Errno {code}] {text}")
}

/// A pydantic validation failure as `str(exc)` renders one.
fn validation_error(field: &str, message: &str, kind: &str, input: &Value) -> String {
    format!(
        "1 validation error for BashArgs\n{field}\n  {message} [type={kind}, input_value={}, input_type={}]\n    For further information visit https://errors.pydantic.dev/2.12/v/{kind}",
        crate::tool_projection::python_repr_value(input),
        python_type(input),
    )
}

fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}
