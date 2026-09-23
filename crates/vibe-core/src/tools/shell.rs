//! The shell tool families.
//!
//! The reference publishes three of them, one per shell it knows how to drive:
//! `bash` on a POSIX host, and `git_bash` or `powershell` on Windows. Each
//! publishes two variants of its command tool and picks between them with a
//! rollout gate. The legacy one runs a single command to completion; the
//! managed one starts a session that outlives the call and is polled, fed and
//! listed through `<family>_output`, `<family>_stdin`, `<family>_sessions` and
//! `<family>_log_file`. Every variant and every session tool is built here on
//! [`TerminalManager`], the process abstraction this workspace already owns, so
//! the shell surface adds tool families rather than a second way to spawn a
//! child.
//!
//! Which family a host publishes is data, not a compilation target: a
//! [`HostShells`] value carries the platform and the executables the Windows
//! families need, so the surface a Windows operator sees is decided by the same
//! function on every host and can be proven from a POSIX one.
//!
//! Two invariants shape the module. Every command is analyzed by
//! [`analyze_shell`] before it runs, and a command the analysis does not permit
//! outright reaches the operator as an approval request; nothing executes at a
//! looser mode than the analysis returns, and an override the analysis of the
//! command text cannot see (`cwd`, `shell`, `env`) lowers that mode itself. And
//! every terminal this family opens is owned by a guard, so a dropped turn
//! terminates the process group instead of leaving it behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::policy::{PolicyGuardedTool, ToolGuard};
use crate::process::{
    ClientShellRequest, ClientToolIo, ProcessError, ProcessSpec, ProcessStream, TerminalManager,
    TerminalState,
};
use crate::shell::ShellConfig;
use crate::tools::config::{ShellCommandConfig, ToolConfigResolver};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolCondition, ToolError, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolRegistry,
};

mod decode;
mod document;
mod host;
mod policy;
mod session;
mod session_tools;
mod specs;

use decode::{render_stream_bytes, render_stream_chars};
use document::Document;
pub use host::{HostShells, ShellRollout};
use host::{ShellFamily, family_config, published_family};
use policy::{
    CommandWiring, command_argument, guarded_command, log_file_requirements, render_seconds,
    stdin_requirements, string_argument, timeout_argument,
};
use session::{
    SessionShell, SessionStatus, guarded_session, kill_managed_session, run_managed_command,
    session_handler,
};
use session_tools::{is_family_session_id, run_log_file, run_output, run_sessions, run_stdin};
use specs::{command_spec, log_file_spec, output_spec, sessions_spec, stdin_spec};

/// Reference `TerminalSessionManager.base_dir`, relative to the Vibe home.
const LOG_DIRECTORY: &str = "shell-tool";
/// Reference `TerminalSessionManager.sessions_dir`, relative to [`LOG_DIRECTORY`].
const SESSIONS_DIRECTORY: &str = "sessions";
/// How often the pump drains a managed session's terminal into its log.
const PUMP_INTERVAL: Duration = Duration::from_millis(25);
/// Reference `BaseTool.selection_priority`, carried by the legacy variant.
const LEGACY_SELECTION_PRIORITY: i32 = 0;
/// Reference `ExperimentalBash.selection_priority`, which is what makes the
/// managed variant win the family name when both are registered.
const MANAGED_SELECTION_PRIORITY: i32 = 10;

/// The shell tools, and the terminals and sessions they keep per Vibe session.
///
/// One instance serves every session, like [`crate::tools::builtins`]: the
/// managed sessions are keyed by session id so a re-registration after an agent
/// switch finds the sessions it left running.
/// What the host offers, re-read on demand.
///
/// The reference availability predicates probe the machine on every
/// publication, so the family a session publishes is a question asked again at
/// each turn rather than an answer frozen at registration.
pub type HostResolver = Arc<dyn Fn() -> HostShells + Send + Sync>;

#[derive(Clone)]
pub struct ShellTools {
    vibe_home: PathBuf,
    host: HostResolver,
    sessions: Arc<StdMutex<BTreeMap<String, Arc<SessionShell>>>>,
}

impl std::fmt::Debug for ShellTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellTools")
            .field("vibe_home", &self.vibe_home)
            .field("host", &(self.host)())
            .finish_non_exhaustive()
    }
}

impl ShellTools {
    #[must_use]
    pub fn new(vibe_home: impl Into<PathBuf>) -> Self {
        Self::with_host_resolver(vibe_home, Arc::new(HostShells::detect))
    }

    /// The same tools against a stated host, which is how a POSIX test drives
    /// the Windows families.
    #[must_use]
    pub fn with_host(vibe_home: impl Into<PathBuf>, host: HostShells) -> Self {
        Self::with_host_resolver(vibe_home, Arc::new(move || host.clone()))
    }

    /// The same tools against a host answered anew at every publication, which
    /// is what [`ShellTools::new`] installs and what lets a test move the
    /// machine under a running session.
    #[must_use]
    pub fn with_host_resolver(vibe_home: impl Into<PathBuf>, host: HostResolver) -> Self {
        Self {
            vibe_home: vibe_home.into(),
            host,
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
        }
    }

    /// Publishes this host's shell family for one session.
    ///
    /// The legacy variant registers first, matching the reference rollout gate,
    /// which keeps `legacy` available wherever the managed rollout is off. When
    /// the managed rollout is on, the managed variant registers too and wins
    /// the family name by selection priority, and the four session tools join
    /// it. A host with no family to publish registers nothing.
    ///
    /// A client that hosts its own terminal is offered neither the managed
    /// variant nor the four session tools: reference `BaseTool` declares
    /// `local_managed_shell_only`, `ToolManager._is_tool_available` refuses
    /// every class carrying it once
    /// `local_managed_shell_runtime_enabled` is false, and the reference
    /// runtime computes that flag as "the connected client publishes no
    /// `terminal` tool". `ExperimentalBash` and its four session siblings
    /// declare the flag, and the two Windows families inherit it through
    /// `ExperimentalGitBash` and `ExperimentalWindowsShell`, while the
    /// `GitBash` and `WindowsShell` command classes do not, which is why the
    /// family name survives the withholding on every platform and only the
    /// managed variant behind it disappears.
    pub fn register(
        &self,
        session_id: &str,
        working_directory: &Path,
        registry: &ToolRegistry,
        client_io: Option<ClientToolIo>,
        guard: &ToolGuard,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let ToolGuard {
            policy,
            approval,
            config,
            scratchpad,
        } = guard;
        let host = (self.host)();
        // Reference `_is_enabled_for_shell_rollout` asks the session
        // configuration at every availability check, so the rollout is read
        // here rather than frozen when the family was constructed: a rollout
        // that arrives after startup, which is what the experiments layer
        // writes, reaches the next registration.
        let rollout = ShellRollout::from_config(config);
        let Some((family, rolled_out)) = published_family(&host, rollout) else {
            return Ok(Vec::new());
        };
        // The gate reads the same capability `delegated_command` reads, so what
        // the client hosts is asked once rather than answered by a second
        // source of truth. No client at all withholds nothing: nothing hosts a
        // terminal for this session.
        let managed = rolled_out
            && !client_io
                .as_ref()
                .is_some_and(ClientToolIo::supports_terminal);
        let Some(shell_config) = family_config(family, &host) else {
            return Ok(Vec::new());
        };
        // The three shell lists are composed from the interpreter this family
        // drives, not from the host, so the resolver follows the family. The
        // settings cache is shared, so an operator's `tools.<family>` entry
        // still reaches every tool published below.
        // Reference `uses_posix_shell` is false for a Windows host whose `bash`
        // tool falls back to `cmd.exe`, which composes the Windows lists.
        let config = config.clone().with_posix_shell(
            family.uses_posix_shell() && shell_config.flavor != crate::shell::ShellFlavor::Cmd,
        );
        let shell = self.session_shell(session_id, family)?;
        let platform = host.platform;
        let working_directory = working_directory.to_path_buf();
        // Reference `git_bash_shell_available` and `powershell_shell_available`
        // are re-read on every publication, so a family whose interpreter is
        // uninstalled mid-session drops out of the surface at the next turn.
        // The POSIX family has no such probe: reference `Bash.is_available` is
        // the inherited default.
        let condition: Option<ToolCondition> = match family {
            ShellFamily::Bash => None,
            ShellFamily::GitBash | ShellFamily::PowerShell => {
                let resolver = self.host.clone();
                let tool_config = config.clone();
                Some(Arc::new(move || {
                    published_family(&resolver(), ShellRollout::from_config(&tool_config))
                        .is_some_and(|(published, _)| published == family)
                }))
            }
        };
        let publish = |spec, handler| match &condition {
            Some(condition) => registry.register_conditional(spec, handler, condition.clone()),
            None => registry.register(spec, handler),
        };
        let mut outcomes = vec![publish(
            command_spec(family, false),
            guarded_command(CommandWiring {
                family,
                shell: shell.clone(),
                shell_config: shell_config.clone(),
                tool_config: config.clone(),
                working_directory: working_directory.clone(),
                platform,
                policy: policy.clone(),
                approval: approval.clone(),
                managed: false,
                client_io: client_io.clone(),
                scratchpad: scratchpad.clone(),
            }),
        )?];
        if !managed {
            return Ok(outcomes);
        }
        let flavor = shell_config.flavor;
        outcomes.push(publish(
            command_spec(family, true),
            guarded_command(CommandWiring {
                family,
                shell: shell.clone(),
                shell_config,
                tool_config: config.clone(),
                working_directory,
                platform,
                policy: policy.clone(),
                approval: approval.clone(),
                managed: true,
                client_io,
                scratchpad: scratchpad.clone(),
            }),
        )?);
        // The three polling tools are configured `always` upstream and produce
        // no granular requirement, so the guard in front of them is exactly
        // what reads that permission and the lists beside it.
        outcomes.push(publish(
            output_spec(family),
            guarded_session(
                family.tool_name("output"),
                policy,
                approval,
                session_handler(
                    shell.clone(),
                    config.clone(),
                    family.tool_name("output"),
                    run_output,
                ),
            ),
        )?);
        let stdin_shell = shell.clone();
        outcomes.push(publish(
            stdin_spec(family),
            Arc::new(PolicyGuardedTool::new(
                family.tool_name("stdin"),
                policy.clone(),
                approval.clone(),
                Arc::new(move |invocation| {
                    Ok(stdin_requirements(
                        &stdin_shell,
                        flavor,
                        &invocation.arguments,
                    ))
                }),
                session_handler(
                    shell.clone(),
                    config.clone(),
                    family.tool_name("stdin"),
                    run_stdin,
                ),
            )),
        )?);
        outcomes.push(publish(
            sessions_spec(family),
            guarded_session(
                family.tool_name("sessions"),
                policy,
                approval,
                session_handler(
                    shell.clone(),
                    config.clone(),
                    family.tool_name("sessions"),
                    run_sessions,
                ),
            ),
        )?);
        outcomes.push(publish(
            log_file_spec(family),
            Arc::new(PolicyGuardedTool::new(
                family.tool_name("log_file"),
                policy.clone(),
                approval.clone(),
                Arc::new(|invocation| Ok(log_file_requirements(&invocation.arguments))),
                session_handler(
                    shell,
                    config.clone(),
                    family.tool_name("log_file"),
                    run_log_file,
                ),
            )),
        )?);
        Ok(outcomes)
    }

    /// Terminates every process this session started and forgets its state.
    ///
    /// A managed session outlives the call that started it by design, so
    /// session teardown is the only place that can stop it.
    pub async fn close_session(&self, session_id: &str) -> Result<(), ToolError> {
        let Some(shell) = self.take_session_shell(session_id)? else {
            return Ok(());
        };
        // Reference `TerminalRuntime.close` resets every family without
        // clearing its logs: each running session is killed through the same
        // ladder a `kill` action takes, so its manifest records `killed` and
        // the status its process ended with rather than an orphan a later
        // process would report.
        let sessions = shell
            .managed
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in &sessions {
            let _ = kill_managed_session(&shell, session, SessionStatus::Killed).await;
        }
        shell.managed.lock().await.clear();
        shell
            .terminals
            .cleanup_all()
            .await
            .map(drop)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    fn session_shell(
        &self,
        session_id: &str,
        family: ShellFamily,
    ) -> Result<Arc<SessionShell>, ToolError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ToolError::Execution("the shell session lock is poisoned".to_owned()))?;
        Ok(sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                let shell = Arc::new(SessionShell {
                    family,
                    terminals: TerminalManager::default(),
                    managed: Mutex::new(BTreeMap::new()),
                    orphaned: StdMutex::new(BTreeMap::new()),
                    log_root: self.vibe_home.join(LOG_DIRECTORY),
                });
                // Reference `TerminalSessionManager.__init__` reads the
                // manifests as the family is built, so the first `sessions`
                // call of a fresh process already lists what the previous one
                // left running.
                shell.load_orphaned_manifests();
                shell
            })
            .clone())
    }

    fn take_session_shell(&self, session_id: &str) -> Result<Option<Arc<SessionShell>>, ToolError> {
        self.sessions
            .lock()
            .map_err(|_| ToolError::Execution("the shell session lock is poisoned".to_owned()))
            .map(|mut sessions| sessions.remove(session_id))
    }
}

// --------------------------------------------------------------------------
// The command tool
// --------------------------------------------------------------------------

/// Owns a terminal until the call gives it up.
///
/// A cancelled turn drops the tool future, which drops this guard, which
/// terminates the process group. Without it a long command would survive the
/// turn that started it.
struct TerminalGuard {
    terminals: TerminalManager,
    terminal_id: Option<String>,
}

impl TerminalGuard {
    fn new(terminals: TerminalManager, terminal_id: String) -> Self {
        Self {
            terminals,
            terminal_id: Some(terminal_id),
        }
    }

    /// Hands ownership back: the caller has terminated or handed off the
    /// terminal itself.
    fn disarm(&mut self) {
        self.terminal_id = None;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let Some(terminal_id) = self.terminal_id.take() else {
            return;
        };
        let terminals = self.terminals.clone();
        // Termination is asynchronous and `Drop` is not, so it runs as a task.
        // Outside a runtime there is nothing to spawn onto and nothing that
        // could have started a process either.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = terminals.interrupt(&terminal_id).await;
                let _ = terminals.release(&terminal_id).await;
            });
        }
    }
}

fn command_handler(
    shell: Arc<SessionShell>,
    config: ShellConfig,
    tool_config: ToolConfigResolver,
    tool_name: String,
    working_directory: PathBuf,
    managed: bool,
    client_io: Option<ClientToolIo>,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let shell = shell.clone();
            let config = config.clone();
            // Read per call, so a raised timeout or output budget applies to
            // the next command rather than to the next session.
            let settings: ShellCommandConfig = tool_config.view(&tool_name);
            let working_directory = working_directory.clone();
            let arguments = invocation.arguments.clone();
            let client = client_io.clone();
            let call_id = invocation.call_id.clone();
            Box::pin(async move {
                // A client terminal is one command's terminal: it is created,
                // waited on and released inside this call, which is why the
                // delegation covers the legacy variant and leaves the managed
                // sessions the model addresses later on this host.
                if !managed
                    && shell.family == ShellFamily::Bash
                    && let Some(delegated) = delegated_command(
                        client.as_ref(),
                        &working_directory,
                        &arguments,
                        &settings,
                        &output,
                        &call_id,
                    )
                    .await?
                {
                    return Ok(delegated);
                }
                if !managed && shell.family != ShellFamily::Bash {
                    return run_windows_fallback(
                        &shell,
                        &config,
                        &working_directory,
                        &arguments,
                        &settings,
                        client.as_ref(),
                        &call_id,
                    )
                    .await;
                }
                if managed {
                    run_managed_command(
                        &shell,
                        &config,
                        &working_directory,
                        &arguments,
                        &settings,
                        &output,
                    )
                    .await
                } else {
                    run_legacy_command(&shell, &config, &working_directory, &arguments, &settings)
                        .await
                }
            })
        },
    )
}

/// Runs one command on a client terminal, or `None` when the client hosts none.
async fn delegated_command(
    client: Option<&ClientToolIo>,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
    output: &ToolOutputSink,
    call_id: &str,
) -> Result<Option<ToolExecutionOutput>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_terminal()) else {
        return Ok(None);
    };
    let command = command_argument(arguments)?;
    let timeout = legacy_timeout(arguments, settings);
    let limit = settings
        .max_output_bytes
        .min(output.remaining_bytes().max(1));
    let result = client
        .run_shell(ClientShellRequest {
            tool_call_id: Some(call_id.to_owned()),
            command: command.clone(),
            args: None,
            env: None,
            cwd: working_directory.to_string_lossy().into_owned(),
            output_byte_limit: u64::try_from(limit).unwrap_or(u64::MAX),
            timeout: Duration::from_secs(timeout),
        })
        .await
        .map_err(|error| {
            ToolError::Execution(format!("the client terminal failed: {error}: `{command}`"))
        })?;
    // Reference `Bash.run` slices the client's two strings to
    // `max_output_bytes` characters, as it slices its own.
    let cut = |text: String| match text.char_indices().nth(limit) {
        Some((end, _)) => text[..end].to_owned(),
        None => text,
    };
    command_output(
        &command,
        cut(result.stdout),
        cut(result.stderr),
        result.returncode,
    )
    .map(Some)
}

async fn run_legacy_command(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let timeout = legacy_timeout(arguments, settings);
    // Reference `spawn_shell_command` reads `$SHELL` at every call on a POSIX
    // host, so a session whose environment changed runs the shell it now names.
    let mut config = config.clone();
    if shell.family == ShellFamily::Bash && config.flavor == crate::shell::ShellFlavor::Posix {
        config.executable = host::legacy_posix_shell();
    }
    // Reference `Bash.run` slices each decoded stream to `max_output_bytes`
    // characters, so the reader keeps enough bytes for that many characters of
    // the widest encoding.
    let limit = settings.max_output_bytes;
    let terminal_id = shell
        .terminals
        .run(process_spec(
            shell.family,
            &config,
            working_directory,
            &command,
            None,
            limit.saturating_mul(4),
            false,
        ))
        .await
        .map_err(process_error)?;
    let mut guard = TerminalGuard::new(shell.terminals.clone(), terminal_id.clone());
    // Nothing feeds this command, so its standard input reads EOF rather than
    // blocking forever on a prompt no one answers.
    shell
        .terminals
        .close_stdin(&terminal_id)
        .await
        .map_err(process_error)?;
    let waited = tokio::time::timeout(
        Duration::from_secs(timeout),
        shell.terminals.wait(&terminal_id),
    )
    .await;
    let Ok(read) = waited else {
        shell
            .terminals
            .interrupt(&terminal_id)
            .await
            .map_err(process_error)?;
        let _ = shell.terminals.release(&terminal_id).await;
        guard.disarm();
        return Err(ToolError::Execution(format!(
            "the command timed out after {timeout}s and its process group was terminated: \
             `{command}`"
        )));
    };
    let read = read.map_err(process_error)?;
    guard.disarm();
    let _ = shell.terminals.release(&terminal_id).await;

    // Reference `_run_command` cuts each stream to the window and publishes no
    // field saying so, so a truncated result is silent here too.
    let stdout = render_stream_chars(&read.chunks, ProcessStream::Stdout, limit);
    let stderr = render_stream_chars(&read.chunks, ProcessStream::Stderr, limit);
    let status = exit_status(&read.state);
    command_output(&command, stdout, stderr, status)
}

/// Runs one command through the Git Bash or PowerShell tool that stands in
/// when the managed sessions are off.
///
/// Reference `GitBash.run` and `WindowsShell.run`: the call's `cwd`, `env` and
/// `shell` apply, the shell must resolve to the family's interpreter, the wait
/// is `timeout`, else `timeout_seconds`, else the default, capped at
/// `max_timeout_seconds`, each stream is cut to `max_output_bytes` bytes before
/// it is decoded, and the result names the shell that ran. A client hosting a
/// terminal runs the resolved interpreter with the family's variables and the
/// call's overrides.
async fn run_windows_fallback(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
    client: Option<&ClientToolIo>,
    call_id: &str,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let timeout = timeout_argument(arguments, settings);
    let cwd = string_argument(arguments, "cwd")
        .filter(|cwd| !cwd.is_empty())
        .map_or_else(
            || working_directory.to_path_buf(),
            |cwd| crate::shell::resolve_path(&working_directory.join(cwd)),
        );
    let resolved = host::resolve_session_shell(
        shell.family,
        config,
        string_argument(arguments, "shell"),
        settings.shell.as_deref(),
    )?;
    let executable = resolved.executable.to_string_lossy().into_owned();
    let limit = settings.max_output_bytes;
    let timed_out = || {
        ToolError::Execution(format!(
            "the command timed out after {}s: `{command}`",
            render_seconds(timeout)
        ))
    };
    if let Some(client) = client.filter(|client| client.supports_terminal()) {
        let mut environment = shell
            .family
            .environment(false)
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        if let Some(Value::Object(overrides)) = arguments.get("env") {
            for (key, value) in overrides {
                if let Some(value) = value.as_str() {
                    environment.insert(key.clone(), value.to_owned());
                }
            }
        }
        let result = client
            .run_shell(ClientShellRequest {
                tool_call_id: Some(call_id.to_owned()),
                command: executable.clone(),
                args: Some(
                    resolved
                        .arguments
                        .iter()
                        .cloned()
                        .chain(std::iter::once(command.clone()))
                        .collect(),
                ),
                env: Some(environment.into_iter().collect()),
                cwd: cwd.to_string_lossy().into_owned(),
                output_byte_limit: u64::try_from(limit).unwrap_or(u64::MAX),
                timeout,
            })
            .await
            .map_err(|error| {
                ToolError::Execution(format!("the client terminal failed: {error}: `{command}`"))
            })?;
        let cut = |text: String| match text.char_indices().nth(limit) {
            Some((end, _)) => text[..end].to_owned(),
            None => text,
        };
        return command_output_with_shell(
            &command,
            &executable,
            cut(result.stdout),
            cut(result.stderr),
            result.returncode,
        );
    }
    let terminal_id = shell
        .terminals
        .run(process_spec(
            shell.family,
            &resolved,
            &cwd,
            &command,
            arguments.get("env"),
            limit,
            false,
        ))
        .await
        .map_err(process_error)?;
    let mut guard = TerminalGuard::new(shell.terminals.clone(), terminal_id.clone());
    shell
        .terminals
        .close_stdin(&terminal_id)
        .await
        .map_err(process_error)?;
    let Ok(read) = tokio::time::timeout(timeout, shell.terminals.wait(&terminal_id)).await else {
        shell
            .terminals
            .interrupt(&terminal_id)
            .await
            .map_err(process_error)?;
        let _ = shell.terminals.release(&terminal_id).await;
        guard.disarm();
        return Err(timed_out());
    };
    let read = read.map_err(process_error)?;
    guard.disarm();
    let _ = shell.terminals.release(&terminal_id).await;
    let stdout = render_stream_bytes(&read.chunks, ProcessStream::Stdout, limit);
    let stderr = render_stream_bytes(&read.chunks, ProcessStream::Stderr, limit);
    command_output_with_shell(
        &command,
        &executable,
        stdout,
        stderr,
        exit_status(&read.state),
    )
}

/// Reference `Bash.run`'s wait: `args.timeout or default_timeout`, so a zero
/// means the default, and no ceiling applies to the legacy variant.
fn legacy_timeout(arguments: &Value, settings: &ShellCommandConfig) -> u64 {
    arguments["timeout"]
        .as_i64()
        .filter(|seconds| *seconds != 0)
        .map_or(settings.default_timeout, |seconds| {
            u64::try_from(seconds).unwrap_or(0)
        })
}

/// What one finished command reports, whether this host ran it or a client did.
///
/// Reference `completed_shell_result`: a non-zero status is a tool failure
/// rather than a result, and a success publishes `CapturedShellResult` in its
/// declared order, `command`, `shell`, `exit_code`, `stdout` and `stderr`,
/// followed by the computed `returncode` alias. The legacy tools pass no shell,
/// so the field is the model's empty default.
fn command_output(
    command: &str,
    stdout: String,
    stderr: String,
    status: i32,
) -> Result<ToolExecutionOutput, ToolError> {
    command_output_with_shell(command, "", stdout, stderr, status)
}

/// [`command_output`] naming the shell that ran, which the Git Bash and
/// PowerShell fallbacks publish.
fn command_output_with_shell(
    command: &str,
    shell: &str,
    stdout: String,
    stderr: String,
    status: i32,
) -> Result<ToolExecutionOutput, ToolError> {
    if status != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {status}: `{command}`\nstderr:\n{stderr}\n\
             stdout:\n{stdout}"
        )));
    }
    Ok(Document::new()
        .field("command", command)
        .field("shell", shell)
        .field("exit_code", status)
        .field("stdout", stdout)
        .field("stderr", stderr)
        .field("returncode", status)
        .into_output(json!({"kind": "shell", "command": command})))
}

fn process_spec(
    family: ShellFamily,
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: Option<&Value>,
    max_output_bytes: usize,
    managed: bool,
) -> ProcessSpec {
    let mut spec = ProcessSpec::new(&config.executable, working_directory);
    spec.arguments = config
        .arguments
        .iter()
        .cloned()
        .chain(std::iter::once(command.to_owned()))
        .collect();
    // Both streams share one budget in the reader, so the spec carries what the
    // two rendered streams may need together. A managed session is different:
    // reference `_reader_loop` appends every byte to the session log, which is
    // what the polling tools page through, so nothing bounds its capture but
    // the disk.
    spec.max_output_bytes = if managed {
        usize::MAX
    } else {
        max_output_bytes.saturating_mul(2)
    };
    // A managed session outlives its call and is fed control keys, which only a
    // terminal turns into signals, so it asks for one; the legacy variant runs
    // one command to completion and needs none.
    spec.terminal = managed;
    // The family's own variables go in first: the reference merges the call's
    // overrides over them, so a call may still ask for a pager it will read.
    spec.unset_environment = family.unset_environment(managed);
    for (key, value) in family.environment(managed) {
        spec.environment.insert(key, value);
    }
    if let Some(Value::Object(overrides)) = environment {
        for (key, value) in overrides {
            if let Some(value) = value.as_str() {
                spec.environment.insert(key.clone(), value.to_owned());
            }
        }
    }
    spec
}

fn process_error(error: ProcessError) -> ToolError {
    ToolError::Execution(error.to_string())
}

/// The status a finished terminal reports, the way Python's
/// `Popen.returncode` reports it: the exit code, or the negated number of the
/// signal that ended the child.
fn exit_status(state: &TerminalState) -> i32 {
    match state {
        TerminalState::Exited { code, signal, .. }
        | TerminalState::Interrupted { code, signal } => {
            code.or_else(|| signal.map(|signal| -signal)).unwrap_or(-1)
        }
        TerminalState::Running => 0,
        TerminalState::Failed { .. } => -1,
    }
}

#[cfg(test)]
mod session_parity_tests;
#[cfg(test)]
mod tests;
