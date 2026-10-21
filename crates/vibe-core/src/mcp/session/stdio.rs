//! The stdio transport: a server this client launches and speaks to over its
//! standard streams, one JSON-RPC message per line.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Inbound, SessionError};
use crate::child::{ChildGroup, Rung};

/// SDK `PROCESS_TERMINATION_TIMEOUT`: how long a stdio server has to exit once
/// its input closes before it is terminated.
const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);

/// SDK `DEFAULT_INHERITED_ENV_VARS`: the only variables a stdio server inherits
/// from this process, beside the ones its entry declares.
#[cfg(not(windows))]
const INHERITED_ENVIRONMENT: &[&str] = &["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];
#[cfg(windows)]
const INHERITED_ENVIRONMENT: &[&str] = &[
    "APPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "USERNAME",
    "USERPROFILE",
];

pub(super) struct StdioChannel {
    child: ChildGroup,
    stdin: Option<ChildStdin>,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

impl StdioChannel {
    pub(super) fn spawn(
        program: &str,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        working_directory: Option<&Path>,
        sender: mpsc::UnboundedSender<Inbound>,
    ) -> Result<Self, SessionError> {
        let program = stdio_program(program)?;
        let mut command = Command::new(&program);
        command
            .args(arguments)
            .env_clear()
            .envs(inherited_environment())
            .envs(environment);
        if let Some(working_directory) = working_directory {
            command.current_dir(working_directory);
        }
        let (child, pipes) = ChildGroup::spawn(&mut command).map_err(|error| {
            SessionError::Transport(launch_failure(&program, arguments, &error))
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (pipes.stdin, pipes.stdout, pipes.stderr)
        else {
            return Err(SessionError::Transport(
                "the stdio server started without its pipes".to_owned(),
            ));
        };
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // SDK `stdio_client` hands a line that is not JSON-RPC to the
                // session as an exception, which the client session ignores.
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if sender.send(Inbound::Message(value)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Inbound::Closed);
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            reader,
            stderr: tokio::spawn(drain(stderr)),
        })
    }

    pub(super) async fn write(&mut self, message: &Value) -> Result<(), SessionError> {
        let stdin = self.stdin.as_mut().ok_or(SessionError::Closed)?;
        let mut line = serde_json::to_vec(message)
            .map_err(|error| SessionError::Transport(error.to_string()))?;
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|_| SessionError::Closed)?;
        stdin.flush().await.map_err(|_| SessionError::Closed)
    }

    pub(super) async fn close(mut self) {
        // Closing input is the protocol's shutdown signal; SDK `stdio_client`
        // then waits for the exit and terminates the tree only after its
        // grace period.
        self.stdin.take();
        let _ = self
            .child
            .shut_down(PROCESS_TERMINATION_TIMEOUT, Rung::Wait)
            .await;
        let _ = self
            .child
            .reap_group(PROCESS_TERMINATION_TIMEOUT, false)
            .await;
        self.reader.abort();
        if tokio::time::timeout(PROCESS_TERMINATION_TIMEOUT, &mut self.stderr)
            .await
            .is_err()
        {
            self.stderr.abort();
        }
    }
}

impl Drop for StdioChannel {
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr.abort();
    }
}

/// The program a stdio entry launches.
///
/// SDK `get_windows_executable_command` resolves a bare name through
/// `shutil.which` on Windows, which is what starts `npx` as the `npx.cmd`
/// shim npm installs; `CreateProcess` alone only tries `npx.exe`. A name with
/// a directory or an extension is used as written, and off Windows the spawn
/// searches `PATH` itself.
pub(super) fn stdio_program(program: &str) -> Result<PathBuf, SessionError> {
    let written = Path::new(program);
    if !cfg!(windows) || written.components().count() > 1 || written.extension().is_some() {
        return Ok(written.to_path_buf());
    }
    crate::platform::find_executable(program).ok_or_else(|| {
        SessionError::Transport(format!(
            "cannot launch `{program}`: none of {} is on PATH",
            crate::platform::executable_names(program).join(", ")
        ))
    })
}

/// Why `program` did not start, naming the argument std refused to hand a
/// batch script.
///
/// `CreateProcess` runs a `.bat` or `.cmd` file through `cmd.exe`, so std
/// escapes its arguments for that parser and refuses, with `InvalidInput`,
/// one it cannot escape (a line break would end the command line early).
fn launch_failure(program: &Path, arguments: &[String], error: &io::Error) -> String {
    let batch = program.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
    });
    if batch && error.kind() == io::ErrorKind::InvalidInput {
        let refused = arguments
            .iter()
            .find(|argument| argument.contains(['\r', '\n', '\0']));
        return match refused {
            Some(argument) => format!(
                "cannot launch `{}`: the batch script cannot receive the argument {argument:?} \
                 safely",
                program.display()
            ),
            None => format!(
                "cannot launch `{}`: the batch script cannot receive its arguments safely \
                 ({error})",
                program.display()
            ),
        };
    }
    format!("cannot launch `{}`: {error}", program.display())
}

fn inherited_environment() -> Vec<(&'static str, String)> {
    INHERITED_ENVIRONMENT
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.starts_with("()"))
                .map(|value| (*name, value))
        })
        .collect()
}

async fn drain(mut stderr: ChildStderr) {
    let mut sink = tokio::io::sink();
    let _ = tokio::io::copy(&mut stderr, &mut sink).await;
}
