//! Background processes: commands a program starts with `process.start` and
//! keeps talking to after the call returns.
//!
//! Reference `harness/runtimes/python/.../vibe/_processes/` and
//! `_process_actions.py`: each process runs under the shell its session's
//! command environment resolves (`<shell> -lc <command>` on POSIX, Git Bash's
//! `-c` or PowerShell's `-Command` on Windows) on a terminal of its own, in a
//! session and process group of its own, and every byte it writes is kept
//! under a logical cursor counted from its start. A session lists its
//! processes in its public state, and each one that ends leaves a notification
//! the model reads before its next call.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use vibe_core::clock::now_millis;
use vibe_core::sync::lock;
use vibe_core::text::sha256_hex;
use vibe_core::tools::shell::CommandEnvironment;

pub use self::precheck::precheck;
use self::precheck::start_directory;
use self::process::{Lifecycle, Output, Process};
use crate::state::ToolResult;
use crate::tools::shell::ShellContext;

mod precheck;
mod process;

/// The most a page returns, whatever `maxBytes` asks.
const MAX_PAGE: usize = 64_000;
/// How long the output of an exited process keeps draining.
const DRAIN_WAIT: Duration = Duration::from_secs(2);
/// How long a write gives the process to react before it is answered.
const WRITE_SETTLE: Duration = Duration::from_millis(50);

/// What every process's environment holds unless the call says otherwise:
/// reference `_POSIX_ENV_DEFAULTS`, which Git Bash shares.
const DEFAULT_ENVIRONMENT: [(&str, &str); 7] = [
    ("TERM", "xterm-256color"),
    ("COLUMNS", "120"),
    ("LINES", "40"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("LESS", "-FX"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];
/// Reference `_POWERSHELL_ENV_DEFAULTS`.
const POWERSHELL_ENVIRONMENT: [(&str, &str); 2] = [("GIT_PAGER", "more"), ("PAGER", "more")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Completed,
    Failed,
    Stopped,
    Orphaned,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Orphaned => "orphaned",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::Orphaned,
        }
    }
}

/// A `process.*` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Start,
    Output,
    Write,
    List,
    Stop,
}

impl Operation {
    /// The operation `process.<name>` runs.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "start" => Self::Start,
            "output" => Self::Output,
            "write" => Self::Write,
            "list" => Self::List,
            "stop" => Self::Stop,
            _ => return None,
        })
    }
}

/// Called whenever a process starts or ends.
pub type Listener = Arc<dyn Fn() + Send + Sync>;

/// The processes of one session.
#[derive(Default)]
pub struct ProcessManager {
    processes: Mutex<Vec<Arc<Process>>>,
    /// The notifications of processes that ended, waiting for the model.
    notifications: Mutex<Vec<Value>>,
    listener: Mutex<Option<Listener>>,
    /// The starts the session has not yet moved past, each with whether its
    /// process already ended: an end is announced only once its start is
    /// acknowledged (see [`ProcessManager::acknowledge_start`]).
    unacknowledged: Mutex<BTreeMap<String, bool>>,
}

impl std::fmt::Debug for ProcessManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessManager")
            .finish_non_exhaustive()
    }
}

/// A tool failure in the reference's words.
pub(super) fn failure(code: &str, message: &str, details: Value) -> ToolResult {
    ToolResult::Failure {
        content: Vec::new(),
        code: code.to_owned(),
        message: message.to_owned(),
        details: Some(details),
        retryable: false,
        meta: None,
    }
}

fn success(value: Value) -> ToolResult {
    ToolResult::Success {
        content: Vec::new(),
        structured_content: Some(value),
        meta: None,
    }
}

fn unknown(process_id: &str) -> ToolResult {
    failure(
        "unknown_process",
        "Unknown background process",
        json!({"processId": process_id}),
    )
}

/// Reference `_process_id`: stable for one session, action and call.
#[must_use]
pub fn process_id(session_id: &str, action_id: &str, call_id: &str) -> String {
    let session = sha256_hex(session_id.as_bytes());
    let mut seed = b"process\0".to_vec();
    for part in [session_id, action_id, call_id] {
        seed.extend_from_slice(part.as_bytes());
        seed.push(0);
    }
    format!("process-{}-{}", &session[..12], &sha256_hex(&seed)[..24])
}

/// Reference `_merge_process_environment`: the environment's defaults under
/// the inherited variables, then the call's own over both. A Windows
/// environment compares names without case, as its variables are.
fn process_environment(
    environment: CommandEnvironment,
    explicit: Option<&serde_json::Map<String, Value>>,
) -> BTreeMap<String, String> {
    let windows = environment != CommandEnvironment::Unix;
    let same = |left: &str, right: &str| {
        if windows {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let mut merged: BTreeMap<String, String> = vibe_core::platform::unicode_vars().collect();
    let defaults: &[(&str, &str)] = if environment == CommandEnvironment::PowerShell {
        &POWERSHELL_ENVIRONMENT
    } else {
        &DEFAULT_ENVIRONMENT
    };
    for (key, value) in defaults {
        if !merged.keys().any(|existing| same(existing, key)) {
            merged.insert((*key).to_owned(), (*value).to_owned());
        }
    }
    for (key, value) in explicit.into_iter().flatten() {
        merged.retain(|existing, _| !same(existing, key));
        merged.insert(key.clone(), value.as_str().unwrap_or_default().to_owned());
    }
    merged
}

impl ProcessManager {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Who hears of every start and end.
    pub fn listen(&self, listener: Listener) {
        *lock(&self.listener) = Some(listener);
    }

    /// Lets the end of the process `process_id` be published.
    ///
    /// Reference `_commit_terminal_snapshot` holds a process's end behind the
    /// barrier its start opens once the engine has applied the start's result
    /// (`_acknowledge_process_result_applied`), so a process that ends at once
    /// is still published after the call that started it. An end held here is
    /// announced now; an unknown or already acknowledged start is a no-op.
    pub fn acknowledge_start(&self, process_id: &str) {
        let ended = lock(&self.unacknowledged).remove(process_id);
        if ended == Some(true) {
            self.announce();
        }
    }

    fn announce(&self) {
        let listener = lock(&self.listener).clone();
        if let Some(listener) = listener {
            listener();
        }
    }

    fn find(&self, process_id: &str) -> Option<Arc<Process>> {
        lock(&self.processes)
            .iter()
            .find(|process| process.id == process_id)
            .cloned()
    }

    /// The public `backgroundProcesses` list, oldest first.
    #[must_use]
    pub fn public(&self) -> Vec<Value> {
        let processes = lock(&self.processes);
        let mut listed: Vec<&Arc<Process>> = processes.iter().collect();
        listed
            .sort_by(|left, right| (left.created_at, &left.id).cmp(&(right.created_at, &right.id)));
        listed
            .into_iter()
            .map(|process| {
                let (status, exit_code) = process.state();
                json!({
                    "processId": process.id,
                    "command": process.command,
                    "status": status.as_str(),
                    "exitCode": exit_code,
                    "createdAt": vibe_core::time::format_iso_millis_z(process.created_at),
                })
            })
            .collect()
    }

    /// Takes the notifications of processes that ended since the last call.
    #[must_use]
    pub fn take_notifications(&self) -> Vec<Value> {
        std::mem::take(&mut *lock(&self.notifications))
    }

    /// Processes a stored session listed as running belong to no one now.
    pub fn restore(&self, listed: &[Value]) {
        let mut processes = lock(&self.processes);
        for item in listed {
            let Some(id) = item["processId"].as_str() else {
                continue;
            };
            let status = Status::parse(item["status"].as_str().unwrap_or_default());
            let (status, exit_code) = if status == Status::Running {
                (Status::Orphaned, None)
            } else {
                (status, item["exitCode"].as_i64())
            };
            processes.push(Arc::new(Process {
                id: id.to_owned(),
                command: item["command"].as_str().unwrap_or_default().to_owned(),
                created_at: item["createdAt"]
                    .as_str()
                    .and_then(vibe_core::time::parse_iso_millis)
                    .unwrap_or_default(),
                lifecycle: Mutex::new(Lifecycle {
                    status,
                    exit_code,
                    stopping: false,
                }),
                changed: Condvar::new(),
                output: Mutex::new(Output::finished()),
                writer: Mutex::new(None),
                group: None,
                killer: Mutex::new(None),
                directory: None,
            }));
        }
    }

    /// `process.start`.
    pub fn start(
        self: &Arc<Self>,
        process_id: String,
        arguments: &Value,
        workspace: &Path,
        session_dir: Option<&Path>,
        shell: &ShellContext,
    ) -> ToolResult {
        if let Some(refused) = precheck(Operation::Start, arguments, workspace) {
            return refused;
        }
        let command = arguments["command"].as_str().unwrap_or_default();
        if let Some(existing) = self.find(&process_id) {
            let (status, _) = existing.state();
            return success(json!({"processId": process_id, "status": status.as_str()}));
        }
        let cwd = start_directory(arguments, workspace);
        let environment = process_environment(
            shell.host.command_environment(),
            arguments["env"].as_object(),
        );
        let start_failed = |stage: &str| {
            failure(
                "process_start_failed",
                "Background process could not be started",
                json!({"processId": process_id, "stage": stage}),
            )
        };
        let shell = match shell
            .host
            .command_shell(shell.shell.as_deref(), &environment)
        {
            Ok(shell) => shell,
            // The reference's message carries no reason; naming the shell that
            // was tried is this port's addition (`docs/parity.md`). The POSIX
            // reason is the reference backend's sentence, which names neither.
            Err(reason) => {
                let configured = shell.shell.as_deref().filter(|value| !value.is_empty());
                let reason = match (shell.host.command_environment(), configured) {
                    (CommandEnvironment::Unix, Some(configured)) => {
                        format!("{reason}: {configured}")
                    }
                    (CommandEnvironment::Unix, None) => format!("{reason}; tried zsh, bash, sh"),
                    _ => reason,
                };
                return failure(
                    "process_start_failed",
                    &format!("Background process could not be started: {reason}"),
                    json!({"processId": process_id, "stage": "shell_resolution"}),
                );
            }
        };
        let rows = environment
            .get("LINES")
            .and_then(|value| value.parse().ok())
            .unwrap_or(40);
        let cols = environment
            .get("COLUMNS")
            .and_then(|value| value.parse().ok())
            .unwrap_or(120);
        let Ok(pair) = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) else {
            return start_failed("spawn");
        };
        let mut builder = CommandBuilder::new(&shell.executable);
        builder.args(&shell.arguments);
        builder.arg(command);
        builder.cwd(&cwd);
        builder.env_clear();
        for (key, value) in &environment {
            builder.env(key, value);
        }
        let Ok(mut child) = pair.slave.spawn_command(builder) else {
            return start_failed("spawn");
        };
        drop(pair.slave);
        let (Ok(reader), Ok(writer)) = (pair.master.try_clone_reader(), pair.master.take_writer())
        else {
            let _ = child.kill();
            return start_failed("post_spawn");
        };
        let (mut reader, writer) = vibe_core::pty::pseudoconsole_streams(reader, writer);
        let directory = session_dir.map(|dir| dir.join("processes").join(&process_id));
        if let Some(directory) = &directory {
            let _ = std::fs::create_dir_all(directory);
        }
        let process = Arc::new(Process {
            id: process_id.clone(),
            command: command.to_owned(),
            created_at: now_millis(),
            lifecycle: Mutex::new(Lifecycle {
                status: Status::Running,
                exit_code: None,
                stopping: false,
            }),
            changed: Condvar::new(),
            output: Mutex::new(Output::default()),
            writer: Mutex::new(Some(writer)),
            group: child.process_id().and_then(|pid| i32::try_from(pid).ok()),
            killer: Mutex::new(Some(child.clone_killer())),
            directory,
        });
        process.persist_output_state();
        lock(&self.processes).push(Arc::clone(&process));
        lock(&self.unacknowledged).insert(process_id.clone(), false);
        // The reader keeps every byte; the waiter settles the process once
        // its root exits and its terminal has drained.
        {
            let process = Arc::clone(&process);
            std::thread::spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => process.append(&buffer[..read]),
                    }
                }
                lock(&process.output).finished = true;
                process.changed.notify_all();
            });
        }
        {
            let manager = Arc::clone(self);
            let process = Arc::clone(&process);
            let mut master = Some(pair.master);
            std::thread::spawn(move || {
                let exit = child.wait();
                // A pseudoconsole reaches end of file once it is closed, not
                // when its root exits, and closing it ends whatever the root
                // left attached to it; a POSIX terminal drains first.
                if cfg!(windows) {
                    drop(master.take());
                }
                let deadline = Instant::now() + DRAIN_WAIT;
                while Instant::now() < deadline && !lock(&process.output).finished {
                    std::thread::sleep(Duration::from_millis(10));
                }
                drop(master);
                let code = exit.ok().map(|status| {
                    if status.signal().is_some() {
                        -i64::from(signal_number(status.signal().unwrap_or_default()))
                    } else {
                        i64::from(status.exit_code())
                    }
                });
                manager.finish(&process, code);
            });
        }
        self.announce();
        success(json!({"processId": process_id, "status": "running"}))
    }

    /// Records how `process` ended and leaves its notification.
    fn finish(&self, process: &Arc<Process>, code: Option<i64>) {
        let status = {
            let mut lifecycle = lock(&process.lifecycle);
            // A forced end on Windows exits with a plain code rather than a
            // signal, so there the stop under way is what marks it, as
            // reference `_finish` marks any end while stopping.
            let forced = cfg!(windows) || code.is_none_or(|code| code < 0);
            let (status, exit_code) = if lifecycle.stopping && forced {
                (Status::Stopped, None)
            } else if let Some(code) = code {
                (Status::Completed, Some(code))
            } else {
                (Status::Orphaned, None)
            };
            lifecycle.status = status;
            lifecycle.exit_code = exit_code;
            status
        };
        *lock(&process.writer) = None;
        process.persist_output_state();
        process.changed.notify_all();
        let (level, message) = match status {
            Status::Completed => (
                "info",
                format!("Background process {} completed.", process.id),
            ),
            Status::Failed => (
                "error",
                format!("Background process {} failed.", process.id),
            ),
            Status::Stopped => (
                "info",
                format!("Background process {} stopped.", process.id),
            ),
            _ => (
                "warning",
                format!(
                    "The App Server can no longer manage background process {} or confirm whether it is running.",
                    process.id
                ),
            ),
        };
        let mut source = json!({"type": "background_process", "process_id": process.id, "status": status.as_str()});
        if let Some(code) = process.state().1 {
            source["exit_code"] = json!(code);
        }
        lock(&self.notifications).push(json!({
            "id": format!("background-process:{}:terminal", process.id),
            "source": source,
            "level": level,
            "message": message,
        }));
        if let Some(ended) = lock(&self.unacknowledged).get_mut(&process.id) {
            *ended = true;
            return;
        }
        self.announce();
    }

    /// `process.output`, waiting up to `waitMs` (from the start only) for
    /// bytes past the cursor.
    pub fn output(&self, arguments: &Value) -> ToolResult {
        let process_id = arguments["processId"].as_str().unwrap_or_default();
        if let Some(refused) = precheck(Operation::Output, arguments, Path::new(".")) {
            return refused;
        }
        let wait = arguments["waitMs"].as_u64().unwrap_or(0);
        let Some(process) = self.find(process_id) else {
            return unknown(process_id);
        };
        let from_end = arguments["from"].as_str() == Some("end");
        let cursor = arguments["cursor"].as_u64().unwrap_or(0);
        let limit = usize::try_from(arguments["maxBytes"].as_u64().unwrap_or(16_000))
            .unwrap_or(MAX_PAGE)
            .min(MAX_PAGE);
        if !from_end && wait > 0 {
            process.wait_past(cursor, Duration::from_millis(wait));
        }
        let page = match process.page(from_end, cursor, limit) {
            Ok(page) => page,
            Err(details) => {
                return failure(
                    "invalid_cursor",
                    "Cursor is outside retained process output",
                    details,
                );
            }
        };
        let (status, exit_code) = process.state();
        success(json!({
            "processId": process.id,
            "status": status.as_str(),
            "exitCode": exit_code,
            "output": String::from_utf8_lossy(&page.bytes),
            "outputStartCursor": page.start,
            "nextCursor": page.next,
            "bytesAvailable": page.end,
            "hasMore": page.has_more,
            "truncatedBefore": page.truncated_before,
        }))
    }

    /// `process.write`.
    pub fn write(&self, arguments: &Value) -> ToolResult {
        let process_id = arguments["processId"].as_str().unwrap_or_default();
        if let Some(refused) = precheck(Operation::Write, arguments, Path::new(".")) {
            return refused;
        }
        let bytes: Vec<u8> = if let Some(text) = arguments["text"].as_str() {
            text.as_bytes().to_vec()
        } else if let Some(keys) = arguments["control"].as_array() {
            keys.iter()
                .filter_map(Value::as_str)
                .flat_map(|key| control_bytes(key).iter().copied())
                .collect()
        } else if let Some(encoded) = arguments["bytesBase64"].as_str() {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let Some(process) = self.find(process_id) else {
            return unknown(process_id);
        };
        let (status, _) = process.state();
        let not_running = || {
            failure(
                "process_not_running",
                "Background process is not running",
                json!({"processId": process.id, "status": process.state().0.as_str()}),
            )
        };
        if status != Status::Running {
            return not_running();
        }
        let mut guard = lock(&process.writer);
        let Some(writer) = guard.as_mut() else {
            return not_running();
        };
        if writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .is_err()
        {
            return failure(
                "process_io_failed",
                "Background process I/O failed",
                json!({"processId": process.id, "operation": "write"}),
            );
        }
        drop(guard);
        // The reference persists the process record after every write, long
        // enough for a process the input ended (an end of file, an interrupt)
        // to be seen ending before the write is answered.
        process.settle(WRITE_SETTLE);
        success(json!({"processId": process.id, "status": "running", "bytesWritten": bytes.len()}))
    }

    /// `process.list`.
    #[must_use]
    pub fn list(&self) -> ToolResult {
        let processes: Vec<Value> = self
            .public()
            .into_iter()
            .map(|item| {
                json!({
                    "processId": item["processId"],
                    "command": item["command"],
                    "status": item["status"],
                    "exitCode": item["exitCode"],
                    "outputPath": null,
                })
            })
            .collect();
        success(json!({"processes": processes}))
    }

    /// `process.stop`: the group is asked to end, then made to.
    pub fn stop(&self, arguments: &Value) -> ToolResult {
        let process_id = arguments["processId"].as_str().unwrap_or_default();
        let Some(process) = self.find(process_id) else {
            return unknown(process_id);
        };
        process.terminate();
        let (status, exit_code) = process.state();
        if status == Status::Running {
            return failure(
                "process_stop_failed",
                "Background process did not reach a terminal PTY-root state",
                json!({"processId": process.id, "status": "running", "exitCode": null}),
            );
        }
        success(json!({"processId": process.id, "status": status.as_str(), "exitCode": exit_code}))
    }

    /// Stops every running process, as closing the session does.
    pub fn shut_down(&self) {
        let processes = lock(&self.processes).clone();
        for process in processes {
            process.terminate();
        }
    }

    /// `session/backgroundProcess/output`: raw bytes, base64-encoded.
    ///
    /// # Errors
    ///
    /// The process is unknown, or the cursor lies past its output.
    pub fn read_raw(
        &self,
        process_id: &str,
        from_end: bool,
        cursor: u64,
        wait_ms: u64,
        max_bytes: usize,
    ) -> Result<Value, (String, String)> {
        let process = self.find(process_id).ok_or_else(|| {
            (
                "not_found".to_owned(),
                "Unknown background process".to_owned(),
            )
        })?;
        if !from_end && wait_ms > 0 {
            process.wait_past(cursor, Duration::from_millis(wait_ms));
        }
        let page = process
            .page(from_end, cursor, max_bytes.min(MAX_PAGE))
            .map_err(|_| {
                (
                    "invalid_params".to_owned(),
                    "Cursor is outside retained process output".to_owned(),
                )
            })?;
        let (status, _) = process.state();
        use base64::Engine;
        Ok(json!({
            "availability": "available",
            "processId": process.id,
            "outputBase64": base64::engine::general_purpose::STANDARD.encode(&page.bytes),
            "outputStartCursor": page.start,
            "nextCursor": page.next,
            "bytesAvailable": page.end,
            "hasMore": page.has_more,
            "truncatedBefore": page.truncated_before,
            "isFinal": status != Status::Running,
        }))
    }

    /// `session/backgroundProcess/stop`.
    ///
    /// # Errors
    ///
    /// The process is unknown.
    pub fn stop_raw(&self, process_id: &str) -> Result<Value, (String, String)> {
        match self.stop(&json!({"processId": process_id})) {
            ToolResult::Success {
                structured_content: Some(value),
                ..
            } => Ok(value),
            ToolResult::Failure { code, message, .. } if code == "unknown_process" => {
                let _ = code;
                Err(("not_found".to_owned(), message))
            }
            ToolResult::Failure { message, .. } => Err(("internal_error".to_owned(), message)),
            ToolResult::Success { .. } => {
                Err(("internal_error".to_owned(), "no result".to_owned()))
            }
        }
    }
}

/// The bytes a named control key sends.
fn control_bytes(key: &str) -> &'static [u8] {
    match key {
        "ctrl_c" => b"\x03",
        "ctrl_d" => b"\x04",
        "ctrl_z" => b"\x1a",
        "esc" => b"\x1b",
        "tab" => b"\x09",
        "enter" => b"\x0d",
        "backspace" => b"\x7f",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "left" => b"\x1b[D",
        "right" => b"\x1b[C",
        _ => b"",
    }
}

/// The number of the signal `portable-pty` names by its description.
fn signal_number(description: &str) -> i32 {
    let lowered = description.to_ascii_lowercase();
    [
        ("hangup", 1),
        ("interrupt", 2),
        ("quit", 3),
        ("abort", 6),
        ("killed", 9),
        ("segmentation", 11),
        ("broken pipe", 13),
        ("alarm", 14),
        ("terminated", 15),
    ]
    .iter()
    .find(|(name, _)| lowered.contains(name))
    .map_or(1, |(_, number)| *number)
}

#[cfg(test)]
mod processes_tests;
