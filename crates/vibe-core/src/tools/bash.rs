//! The POSIX shell tool family.
//!
//! The reference publishes two `bash` variants and picks between them with a
//! rollout gate. The legacy one runs a single command to completion; the
//! managed one starts a session that outlives the call and is polled, fed and
//! listed through `bash_output`, `bash_stdin`, `bash_sessions` and
//! `bash_log_file`. Both variants and the four managed tools are built here on
//! [`TerminalManager`], the process abstraction this workspace already owns, so
//! the shell surface adds a tool family rather than a second way to spawn a
//! child.
//!
//! Two invariants shape the module. Every command is analysed by
//! [`analyze_shell`] before it runs, and a command the analysis does not permit
//! outright reaches the operator as an approval request; nothing executes at a
//! looser mode than the analysis returns, and an override the analysis of the
//! command text cannot see (`cwd`, `shell`, `env`) lowers that mode itself. And
//! every terminal this family opens is owned by a guard, so a dropped turn
//! terminates the process group instead of leaving it behind.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::platform::{Platform, parse_policy_path};
use crate::policy::{
    ApprovalAgent, PermissionMode, PermissionRequirement, PermissionStore, PolicyGuardedTool,
};
use crate::process::{
    ProcessChunk, ProcessError, ProcessSpec, ProcessStream, TerminalManager, TerminalState,
};
use crate::schema::{ObjectSchema, Property};
use crate::shell::{ShellAnalysis, ShellConfig, ShellFlavor, ShellPolicyContext, analyze_shell};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolHandler, ToolHandlerFuture, ToolInvocation, ToolOutputSink, ToolPresentationKind,
    ToolRegistry, ToolSource, ToolSpec,
};

/// Reference `BashToolConfig.default_timeout`.
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
/// Reference `ExperimentalBashToolConfig.max_timeout_seconds`.
const MAX_TIMEOUT_SECONDS: u64 = 600;
/// Reference `BashToolConfig.max_output_bytes`, applied per stream.
const MAX_OUTPUT_BYTES: usize = 16_000;
/// Reference `DEFAULT_INLINE_BYTES`, the managed family's read window.
const DEFAULT_INLINE_BYTES: usize = 30_000;
/// Reference `DEFAULT_MAX_POLL_SECONDS`.
const MAX_POLL_SECONDS: u64 = 300;
/// Reference `TerminalSessionManager.session_prefix` for this family.
const SESSION_PREFIX: &str = "bash";
/// Reference `TerminalSessionManager.base_dir`, relative to the Vibe home.
const LOG_DIRECTORY: &str = "shell-tool";
/// Reference `TerminalSessionManager.sessions_dir`, relative to [`LOG_DIRECTORY`].
const SESSIONS_DIRECTORY: &str = "sessions";
/// How often the pump drains a managed session's terminal into its log.
const PUMP_INTERVAL: Duration = Duration::from_millis(25);
/// Reference `BaseTool.selection_priority`, carried by the legacy variant.
const LEGACY_SELECTION_PRIORITY: i32 = 0;
/// Reference `ExperimentalBash.selection_priority`, which is what makes the
/// managed variant win the `bash` name when both are registered.
const MANAGED_SELECTION_PRIORITY: i32 = 10;

/// Reference `ControlKey`, in declaration order, paired with the bytes each one
/// writes. The schema publishes the names; `bash_stdin` writes the bytes.
const CONTROL_KEYS: [(&str, &[u8]); 40] = [
    ("ctrl_@", b"\x00"),
    ("ctrl_a", b"\x01"),
    ("ctrl_b", b"\x02"),
    ("ctrl_c", b"\x03"),
    ("ctrl_d", b"\x04"),
    ("ctrl_e", b"\x05"),
    ("ctrl_f", b"\x06"),
    ("ctrl_g", b"\x07"),
    ("ctrl_h", b"\x08"),
    ("ctrl_i", b"\x09"),
    ("tab", b"\x09"),
    ("ctrl_j", b"\x0a"),
    ("enter", b"\r"),
    ("return", b"\r"),
    ("ctrl_k", b"\x0b"),
    ("ctrl_l", b"\x0c"),
    ("ctrl_m", b"\r"),
    ("ctrl_n", b"\x0e"),
    ("ctrl_o", b"\x0f"),
    ("ctrl_p", b"\x10"),
    ("ctrl_q", b"\x11"),
    ("ctrl_r", b"\x12"),
    ("ctrl_s", b"\x13"),
    ("ctrl_t", b"\x14"),
    ("ctrl_u", b"\x15"),
    ("ctrl_v", b"\x16"),
    ("ctrl_w", b"\x17"),
    ("ctrl_x", b"\x18"),
    ("ctrl_y", b"\x19"),
    ("ctrl_z", b"\x1a"),
    ("esc", b"\x1b"),
    ("escape", b"\x1b"),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
];

/// Which `bash` variant the session publishes.
///
/// The reference resolves this from a remote experiment whose default variant
/// is `legacy`, and the Rust port has no experiment client, so the managed
/// family stays absent unless an operator asks for it. The environment
/// variable is that ask: it is the only local switch, and it mirrors how
/// `web_search` resolves its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellRollout {
    /// Reference `MANAGED_SHELL_TOOLS_LEGACY`, the default variant.
    #[default]
    Legacy,
    /// Reference `MANAGED_SHELL_TOOLS_MANAGED`.
    Managed,
}

impl ShellRollout {
    /// The variant `variable` selects, defaulting to [`ShellRollout::Legacy`]
    /// exactly as the reference experiment does when nothing resolves.
    #[must_use]
    pub fn from_environment(variable: &str) -> Self {
        match std::env::var(variable).ok().as_deref() {
            Some("1" | "true" | "managed") => Self::Managed,
            _ => Self::Legacy,
        }
    }

    /// Whether the managed family is published on this host.
    ///
    /// Reference `_experimental_bash_enabled` withholds it on Windows, where
    /// the `powershell_*` family covers the same ground.
    fn publishes_managed_family(self) -> bool {
        self == Self::Managed && !cfg!(windows)
    }
}

/// The shell tools, and the terminals and sessions they keep per Vibe session.
///
/// One instance serves every session, like [`crate::tools::builtins`]: the
/// managed sessions are keyed by session id so a re-registration after an agent
/// switch finds the sessions it left running.
#[derive(Clone)]
pub struct ShellTools {
    vibe_home: PathBuf,
    rollout: ShellRollout,
    sessions: Arc<StdMutex<BTreeMap<String, Arc<SessionShell>>>>,
}

impl std::fmt::Debug for ShellTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellTools")
            .field("vibe_home", &self.vibe_home)
            .field("rollout", &self.rollout)
            .finish_non_exhaustive()
    }
}

/// One Vibe session's shell state: the terminals it opened and the managed
/// sessions still addressable by the model.
struct SessionShell {
    terminals: TerminalManager,
    managed: Mutex<BTreeMap<String, Arc<ManagedSession>>>,
    log_root: PathBuf,
}

impl SessionShell {
    fn sessions_directory(&self) -> PathBuf {
        self.log_root.join(SESSIONS_DIRECTORY)
    }
}

impl ShellTools {
    #[must_use]
    pub fn new(vibe_home: impl Into<PathBuf>, rollout: ShellRollout) -> Self {
        Self {
            vibe_home: vibe_home.into(),
            rollout,
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
        }
    }

    /// Publishes the shell family for one session.
    ///
    /// The legacy variant is always registered, matching the reference rollout
    /// gate, which keeps `legacy` available on every non-Windows host. When the
    /// managed rollout is on, the managed variant registers too and wins the
    /// `bash` name by selection priority, and the four session tools join it.
    pub fn register(
        &self,
        session_id: &str,
        working_directory: &Path,
        registry: &ToolRegistry,
        policy: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let shell = self.session_shell(session_id)?;
        let platform = host_platform();
        let config = ShellConfig::default_for(platform);
        let working_directory = working_directory.to_path_buf();
        let mut outcomes = vec![registry.register(
            bash_spec(false),
            guarded_bash(BashWiring {
                shell: shell.clone(),
                config: config.clone(),
                working_directory: working_directory.clone(),
                platform,
                policy: policy.clone(),
                approval: approval.clone(),
                managed: false,
            }),
        )?];
        if !self.rollout.publishes_managed_family() {
            return Ok(outcomes);
        }
        outcomes.push(registry.register(
            bash_spec(true),
            guarded_bash(BashWiring {
                shell: shell.clone(),
                config,
                working_directory,
                platform,
                policy: policy.clone(),
                approval: approval.clone(),
                managed: true,
            }),
        )?);
        outcomes.push(registry.register(
            bash_output_spec(),
            session_handler(shell.clone(), run_bash_output),
        )?);
        outcomes.push(registry.register(
            bash_stdin_spec(),
            session_handler(shell.clone(), run_bash_stdin),
        )?);
        outcomes.push(registry.register(
            bash_sessions_spec(),
            session_handler(shell.clone(), run_bash_sessions),
        )?);
        let log_shell = shell.clone();
        outcomes.push(registry.register(
            bash_log_file_spec(),
            Arc::new(PolicyGuardedTool::new(
                "bash_log_file",
                policy,
                approval,
                Arc::new(move |invocation| {
                    log_file_requirements(&log_shell, &invocation.arguments)
                }),
                session_handler(shell, run_bash_log_file),
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
        shell.managed.lock().await.clear();
        shell
            .terminals
            .cleanup_all()
            .await
            .map(drop)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    fn session_shell(&self, session_id: &str) -> Result<Arc<SessionShell>, ToolError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ToolError::Execution("the shell session lock is poisoned".to_owned()))?;
        Ok(sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                Arc::new(SessionShell {
                    terminals: TerminalManager::default(),
                    managed: Mutex::new(BTreeMap::new()),
                    log_root: self.vibe_home.join(LOG_DIRECTORY),
                })
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

fn host_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Posix
    }
}

// --------------------------------------------------------------------------
// Specifications
// --------------------------------------------------------------------------

/// Directive coverage for `bash`, whose reference description this port must
/// cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool runs one shell command in the workspace | "Run one shell command in the working directory" |
/// | Prefer the file tools over shell equivalents for reading and searching | "reach for read_file and grep instead of cat and grep(1)" |
/// | Quote paths that carry spaces | "Quote every path that carries a space" |
/// | A command that needs approval is paused until the operator answers | "A command the policy does not allow outright waits for approval" |
/// | The timeout is optional and bounded | the `timeout` description |
/// | The managed variant can return a live session instead of waiting | the `background` description |
fn bash_spec(managed: bool) -> ToolSpec {
    let description = "Run one shell command in the working directory. Reach for read_file and \
                       grep instead of cat and grep(1), quote every path that carries a space, and \
                       expect that a command the policy does not allow outright waits for \
                       approval before it runs."
        .to_owned();
    let mut schema = ObjectSchema::new().required(
        "command",
        Property::string().described(if managed {
            "The shell command to run"
        } else {
            "The shell command to execute"
        }),
    );
    schema = schema.optional(
        "timeout",
        Property::integer()
            .described(if managed {
                "How long to wait, in seconds, before the process group is killed"
            } else {
                "How long to wait, in seconds, before the command is abandoned"
            })
            .with_default(Value::Null)
            .nullable(),
    );
    if managed {
        schema = schema
            .optional(
                "background",
                Property::boolean()
                    .described("Return a live session immediately instead of waiting for the exit")
                    .with_default(false),
            )
            .optional(
                "timeout_seconds",
                Property::number()
                    .constrained("minimum", 0)
                    .described("How long to wait in the foreground before the session is left running or killed")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "hard_timeout",
                Property::boolean()
                    .described("Kill the process group when timeout_seconds expires instead of leaving the session running")
                    .with_default(false),
            )
            .optional(
                "cwd",
                Property::string()
                    .described("Run somewhere other than the working directory")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "env",
                Property::map(Property::string())
                    .described("Environment variables added to the ones the session inherits")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "shell",
                Property::string()
                    .described("Run the command through another shell executable")
                    .with_default(Value::Null)
                    .nullable(),
            );
    }
    ToolSpec {
        name: SESSION_PREFIX.to_owned(),
        description,
        input_schema: schema.build(),
        output_schema: None,
        config: json!({
            "defaultTimeoutSeconds": DEFAULT_TIMEOUT_SECONDS,
            "maxOutputBytes": MAX_OUTPUT_BYTES,
        }),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: if managed {
            MANAGED_SELECTION_PRIORITY
        } else {
            LEGACY_SELECTION_PRIORITY
        },
    }
}

/// Directive coverage for `bash_output`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Poll a running or finished session for its output | "Read what a bash session has written" |
/// | Pass back the cursor to read only what is new | the `cursor` description |
/// | Waiting is optional and bounded | the `wait_seconds` description |
/// | A finished session still answers with its last output and status | "A session that has exited still answers" |
fn bash_output_spec() -> ToolSpec {
    ToolSpec {
        name: "bash_output".to_owned(),
        description: "Read what a bash session has written since a cursor. A session that has \
                      exited still answers, with its final output and its exit status."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required("session_id", Property::string())
            .optional(
                "cursor",
                Property::integer()
                    .constrained("minimum", 0)
                    .described("The next_cursor a previous read returned")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "wait_seconds",
                Property::number().constrained("minimum", 0).with_default(0),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: json!({"maxInlineBytes": DEFAULT_INLINE_BYTES, "maxPollSeconds": MAX_POLL_SECONDS}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for `bash_stdin`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Feed a running session's standard input | "Write to a running bash session" |
/// | Text is sent verbatim, newline included | the `text` description |
/// | Control keys are named rather than encoded | the `control` description |
/// | Raw bytes travel as base64 | the `bytes_base64` description |
/// | Exactly one of the three inputs is supplied | "Supply exactly one of text, control or bytes_base64" |
fn bash_stdin_spec() -> ToolSpec {
    ToolSpec {
        name: "bash_stdin".to_owned(),
        description: "Write to a running bash session. Supply exactly one of text, control or \
                      bytes_base64; supplying none or several is refused rather than resolved by \
                      precedence."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required("session_id", Property::string())
            .optional(
                "text",
                Property::string()
                    .described("Text sent exactly as written; end it with \\n to press Enter")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "control",
                Property::array(Property::string().constrained("enum", json!(control_key_names())))
                    .described(
                        "Named control sequences, for example ctrl_c, ctrl_d, esc, tab, enter or \
                         the arrow keys",
                    ),
            )
            .optional(
                "bytes_base64",
                Property::string()
                    .described("Raw bytes to write, base64 encoded")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for `bash_sessions`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | `list` reports this family's sessions | the `action` description |
/// | `inspect` and `kill` need a session id and act on that one session | the `action` and `session_id` descriptions |
/// | `reset` stops every session in the family | the `action` description |
/// | `clear_logs` only applies to `reset` | the `clear_logs` description |
fn bash_sessions_spec() -> ToolSpec {
    ToolSpec {
        name: "bash_sessions".to_owned(),
        description: "Inspect and stop bash sessions.".to_owned(),
        input_schema: ObjectSchema::new()
            .optional(
                "action",
                Property::string()
                    .constrained("enum", json!(["list", "inspect", "kill", "reset"]))
                    .described(
                        "`list` reports every session of this family, `inspect` reads the one \
                         named by session_id, `kill` stops exactly that one, and `reset` stops \
                         them all",
                    )
                    .with_default("list"),
            )
            .optional(
                "session_id",
                Property::string()
                    .described("Required by `inspect` and `kill`, ignored by `list` and `reset`")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "clear_logs",
                Property::boolean()
                    .described("Used by `reset` alone: also delete the stored session logs")
                    .with_default(false),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes of output to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: json!({"maxInlineBytes": DEFAULT_INLINE_BYTES}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for `bash_log_file`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Read, write or append a file in the shell-tool directory | "Read, write or append a file the bash sessions keep" |
/// | A session id names that session's own log | "session_id names that session's log" |
/// | A relative path stays inside the shell-tool directory | "a relative path stays inside the session log directory" |
/// | Reading resumes from an offset and is bounded | the `offset` and `max_bytes` descriptions |
fn bash_log_file_spec() -> ToolSpec {
    ToolSpec {
        name: "bash_log_file".to_owned(),
        description: "Read, write or append a file the bash sessions keep. session_id names that \
                      session's log, and a relative path stays inside the session log directory."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "action",
                Property::string().constrained("enum", json!(["read", "write", "append"])),
            )
            .optional(
                "session_id",
                Property::string().with_default(Value::Null).nullable(),
            )
            .optional(
                "relative_path",
                Property::string().with_default(Value::Null).nullable(),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 0)
                    .described("Where to start reading, in bytes")
                    .with_default(0),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "content",
                Property::string().with_default(Value::Null).nullable(),
            )
            .build(),
        output_schema: None,
        config: json!({"maxInlineBytes": DEFAULT_INLINE_BYTES}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

fn control_key_names() -> Vec<&'static str> {
    CONTROL_KEYS.iter().map(|(name, _)| *name).collect()
}

// --------------------------------------------------------------------------
// Policy
// --------------------------------------------------------------------------

/// Resolves one call's arguments to the analysis its command runs under.
type ShellAnalyser = Arc<dyn Fn(&Value) -> Result<ShellAnalysis, ToolError> + Send + Sync>;

/// Runs [`analyze_shell`] and routes the call by what it decides.
///
/// The reference resolves a command to `ALWAYS`, `ASK` or `NEVER` before it
/// runs; this reproduces that split on top of the workspace's own analyser. An
/// `Always` command executes directly, an `Ask` command goes through the
/// permission store, and a `Never` command is refused before a process exists.
struct ShellPolicyGuard {
    analysis: ShellAnalyser,
    guarded: Arc<PolicyGuardedTool>,
    inner: Arc<dyn ToolHandler>,
}

impl ToolHandler for ShellPolicyGuard {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a> {
        Box::pin(async move {
            let analysis = (self.analysis)(&invocation.arguments)?;
            match analysis.mode {
                PermissionMode::Always => self.inner.invoke(invocation, output).await,
                PermissionMode::Ask => self.guarded.invoke(invocation, output).await,
                // The rationale travels with the refusal: a model that learns
                // why can propose something else instead of retrying.
                PermissionMode::Never => Err(ToolError::Execution(format!(
                    "the command is refused by the shell policy: {}",
                    analysis.rationale.join("; ")
                ))),
            }
        })
    }
}

/// What one `bash` variant needs to reach a process under policy.
struct BashWiring {
    shell: Arc<SessionShell>,
    config: ShellConfig,
    working_directory: PathBuf,
    platform: Platform,
    policy: PermissionStore,
    approval: Arc<dyn ApprovalAgent>,
    managed: bool,
}

fn guarded_bash(wiring: BashWiring) -> Arc<dyn ToolHandler> {
    let BashWiring {
        shell,
        config,
        working_directory,
        platform,
        policy,
        approval,
        managed,
    } = wiring;
    let inner = bash_handler(shell, config.clone(), working_directory.clone(), managed);
    let requirement_root = working_directory.clone();
    let requirement_flavor = config.flavor;
    let guarded = Arc::new(PolicyGuardedTool::new(
        SESSION_PREFIX,
        policy,
        approval,
        Arc::new(move |invocation: &ToolInvocation| {
            let command = command_argument(&invocation.arguments)?;
            let analysis = analyse(requirement_flavor, platform, &requirement_root, &command);
            // Every analysed segment is named on its own, so approving one
            // command does not silently approve the rest of a chain.
            let mut requirements = analysis
                .commands
                .iter()
                .map(|node| PermissionRequirement::Shell {
                    command: std::iter::once(node.program.clone())
                        .chain(node.arguments.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" "),
                })
                .collect::<Vec<_>>();
            requirements.dedup();
            if requirements.is_empty() {
                requirements.push(PermissionRequirement::Shell { command });
            }
            if managed {
                requirements.extend(override_requirements(
                    &invocation.arguments,
                    &requirement_root,
                ));
            }
            Ok(requirements)
        }),
        inner.clone(),
    ));
    let analysis_root = working_directory;
    let analysis_flavor = config.flavor;
    Arc::new(ShellPolicyGuard {
        analysis: Arc::new(move |arguments: &Value| {
            let command = command_argument(arguments)?;
            let mut analysis = analyse(analysis_flavor, platform, &analysis_root, &command);
            // The command text is not all that runs: an override decides where
            // it runs, what interprets it and what it inherits, none of which
            // the analysis of the text can see. So a call carrying one stops
            // being allowed outright and reaches the operator instead.
            if managed && !override_requirements(arguments, &analysis_root).is_empty() {
                analysis.mode = analysis.mode.min(PermissionMode::Ask);
                analysis.rationale.push(
                    "the call overrides the working directory, the shell or the environment"
                        .to_owned(),
                );
            }
            Ok(analysis)
        }),
        guarded,
        inner,
    })
}

fn analyse(
    flavor: ShellFlavor,
    platform: Platform,
    working_directory: &Path,
    command: &str,
) -> ShellAnalysis {
    let Ok(root) = parse_policy_path(platform, &working_directory.to_string_lossy()) else {
        // A working directory the policy cannot parse is not a reason to run
        // unanalysed: the call falls back to asking.
        return ShellAnalysis {
            mode: PermissionMode::Ask,
            rationale: vec!["the working directory is not a policy path".to_owned()],
            commands: Vec::new(),
            path_operands: Vec::new(),
        };
    };
    analyze_shell(
        flavor,
        command,
        &ShellPolicyContext {
            platform,
            working_directory: root.clone(),
            roots: vec![root],
        },
    )
}

/// What the managed variant's overrides require on top of the command itself.
///
/// The reference resolves the permission of a managed call from more than the
/// command text: a custom shell and a custom environment each carry their own
/// requirement (`_build_context_permissions`), and a working directory outside
/// the session root carries an outside-directory one (`_collect_outside_dirs`).
/// None of the three is visible to an analysis of the command string, so an
/// allowlisted command would otherwise run somewhere else, under another
/// interpreter, with an environment the operator never saw.
fn override_requirements(arguments: &Value, root: &Path) -> Vec<PermissionRequirement> {
    let mut requirements = Vec::new();
    if let Some(directory) = string_argument(arguments, "cwd")
        && !is_inside(root, Path::new(directory))
    {
        requirements.push(PermissionRequirement::Write {
            path: PathBuf::from(directory),
        });
    }
    if let Some(shell) = string_argument(arguments, "shell") {
        requirements.push(PermissionRequirement::Shell {
            command: format!("shell override: {shell}"),
        });
    }
    if let Some(names) = environment_names(arguments) {
        requirements.push(PermissionRequirement::Shell {
            command: format!("env override: {names}"),
        });
    }
    requirements
}

/// The overridden variable names, sorted, or `None` when nothing is overridden.
fn environment_names(arguments: &Value) -> Option<String> {
    let overrides = arguments.get("env")?.as_object()?;
    if overrides.is_empty() {
        return None;
    }
    Some(overrides.keys().cloned().collect::<Vec<_>>().join(", "))
}

/// Whether `candidate` resolves inside `root`, answering `false` for anything
/// that cannot be resolved: an unresolvable directory is not a known-safe one.
fn is_inside(root: &Path, candidate: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    candidate
        .canonicalize()
        .is_ok_and(|resolved| resolved.starts_with(&root))
}

fn log_file_requirements(
    shell: &SessionShell,
    arguments: &Value,
) -> Result<Vec<PermissionRequirement>, ToolError> {
    let path = resolve_log_path(shell, arguments)?;
    Ok(vec![match string_argument(arguments, "action") {
        Some("read") => PermissionRequirement::Read { path },
        _ => PermissionRequirement::Write { path },
    }])
}

// --------------------------------------------------------------------------
// Arguments
// --------------------------------------------------------------------------

fn command_argument(arguments: &Value) -> Result<String, ToolError> {
    let command = arguments["command"].as_str().unwrap_or_default().trim();
    if command.is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/command".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    Ok(command.to_owned())
}

fn string_argument<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

/// The foreground wait, in seconds, bounded by the reference maximum.
fn timeout_argument(arguments: &Value) -> u64 {
    // The reference reads `args.timeout or default`, so a zero is falsy and
    // means the default rather than an instant timeout.
    let requested = arguments["timeout"]
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .or_else(|| {
            arguments["timeout_seconds"].as_f64().map(|seconds| {
                // A fractional wait rounds up: waiting less than asked would report
                // a timeout the operator did not request.
                seconds.ceil().max(0.0) as u64
            })
        })
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    requested.clamp(1, MAX_TIMEOUT_SECONDS)
}

fn byte_limit(arguments: &Value, sink: &ToolOutputSink) -> usize {
    let requested = arguments["max_bytes"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_INLINE_BYTES);
    requested
        .min(DEFAULT_INLINE_BYTES)
        .min(sink.remaining_bytes().max(1))
}

// --------------------------------------------------------------------------
// bash
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

fn bash_handler(
    shell: Arc<SessionShell>,
    config: ShellConfig,
    working_directory: PathBuf,
    managed: bool,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let shell = shell.clone();
            let config = config.clone();
            let working_directory = working_directory.clone();
            let arguments = invocation.arguments.clone();
            Box::pin(async move {
                if managed {
                    run_managed_bash(&shell, &config, &working_directory, &arguments, &output).await
                } else {
                    run_legacy_bash(&shell, &config, &working_directory, &arguments, &output).await
                }
            })
        },
    )
}

async fn run_legacy_bash(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let timeout = timeout_argument(arguments);
    let terminal_id = shell
        .terminals
        .run(process_spec(config, working_directory, &command, None))
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

    let limit = MAX_OUTPUT_BYTES.min(output.remaining_bytes().max(1));
    let (stdout, stdout_truncated) = render_stream(&read.chunks, ProcessStream::Stdout, limit);
    let (stderr, stderr_truncated) = render_stream(&read.chunks, ProcessStream::Stderr, limit);
    let truncated = stdout_truncated || stderr_truncated || read.backpressure_dropped;
    let status = exit_status(&read.state);
    if status != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {status}: `{command}`\nstderr:\n{stderr}\n\
             stdout:\n{stdout}"
        )));
    }
    let mut model_text = stdout.clone();
    if !stderr.is_empty() {
        model_text.push_str("\nstderr:\n");
        model_text.push_str(&stderr);
    }
    if truncated {
        model_text.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    Ok(ToolExecutionOutput {
        model_text,
        display: json!({"kind": "shell", "command": command}),
        typed_result: json!({
            "command": command,
            "stdout": stdout,
            "stderr": stderr,
            "returncode": status,
            "truncated": truncated,
        }),
        chunks: Vec::new(),
    })
}

fn process_spec(
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: Option<&Value>,
) -> ProcessSpec {
    let mut spec = ProcessSpec::new(&config.executable, working_directory);
    spec.arguments = config
        .arguments
        .iter()
        .cloned()
        .chain(std::iter::once(command.to_owned()))
        .collect();
    // Both streams share one budget in the reader, so the spec carries what the
    // two rendered streams may need together.
    spec.max_output_bytes = MAX_OUTPUT_BYTES.saturating_mul(2);
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

fn exit_status(state: &TerminalState) -> i32 {
    match state {
        TerminalState::Exited { code, .. } | TerminalState::Interrupted { code } => {
            code.unwrap_or(-1)
        }
        TerminalState::Running => 0,
        TerminalState::Failed { .. } => -1,
    }
}

/// The bytes one stream produced, decoded and bounded by `limit`.
fn render_stream(chunks: &[ProcessChunk], stream: ProcessStream, limit: usize) -> (String, bool) {
    let mut bytes = Vec::new();
    for chunk in chunks.iter().filter(|chunk| chunk.stream == stream) {
        bytes.extend_from_slice(&chunk.bytes);
    }
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

// --------------------------------------------------------------------------
// Managed sessions
// --------------------------------------------------------------------------

/// Reference `Status`, the states a managed session reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Running,
    Completed,
    Killed,
    TimedOut,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
        }
    }
}

#[derive(Debug)]
struct SessionState {
    status: SessionStatus,
    exit_code: Option<i32>,
    backpressure_dropped: bool,
}

struct ManagedSession {
    id: String,
    terminal_id: String,
    command: String,
    working_directory: String,
    shell: String,
    log_path: PathBuf,
    created_at_ms: u128,
    state: StdMutex<SessionState>,
}

impl ManagedSession {
    fn snapshot(&self) -> (SessionStatus, Option<i32>, bool) {
        self.state
            .lock()
            .map_or((SessionStatus::Running, None, false), |state| {
                (state.status, state.exit_code, state.backpressure_dropped)
            })
    }

    fn info(&self) -> Value {
        let (status, exit_code, dropped) = self.snapshot();
        json!({
            "sessionId": self.id,
            "command": self.command,
            "cwd": self.working_directory,
            "shell": self.shell,
            "status": status.as_str(),
            "exitCode": exit_code,
            "outputPath": self.log_path.to_string_lossy(),
            "createdAtMs": self.created_at_ms.to_string(),
            "backpressureDropped": dropped,
        })
    }

    fn is_running(&self) -> bool {
        self.snapshot().0 == SessionStatus::Running
    }

    fn settle(&self, status: SessionStatus, exit_code: Option<i32>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status;
            state.exit_code = exit_code;
        }
    }
}

async fn run_managed_bash(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let requested_directory = string_argument(arguments, "cwd")
        .map_or_else(|| working_directory.to_path_buf(), PathBuf::from);
    let executable = string_argument(arguments, "shell")
        .map_or_else(|| config.executable.clone(), PathBuf::from);
    let config = ShellConfig {
        executable,
        ..config.clone()
    };
    let session = start_managed_session(
        shell,
        &config,
        &requested_directory,
        &command,
        arguments.get("env"),
    )
    .await?;
    let background = arguments["background"].as_bool().unwrap_or(false);
    if background {
        return managed_output(&session, 0, byte_limit(arguments, output));
    }
    let hard_timeout =
        arguments["hard_timeout"].as_bool().unwrap_or(false) || arguments["timeout"].is_u64();
    let timeout = timeout_argument(arguments);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while session.is_running() && Instant::now() < deadline {
        tokio::time::sleep(PUMP_INTERVAL).await;
    }
    if session.is_running() {
        if !hard_timeout {
            // A soft timeout leaves the session running: the model polls it
            // with `bash_output` instead of losing the work.
            return managed_output(&session, 0, byte_limit(arguments, output));
        }
        kill_managed_session(shell, &session, SessionStatus::TimedOut).await?;
        let rendered = managed_output(&session, 0, byte_limit(arguments, output))?;
        return Err(ToolError::Execution(format!(
            "the command timed out after {timeout}s and its process group was terminated: \
             `{command}`\nsession_id: {}\noutput:\n{}",
            session.id, rendered.model_text
        )));
    }
    let rendered = managed_output(&session, 0, byte_limit(arguments, output))?;
    let status = rendered.typed_result["exitCode"].as_i64().unwrap_or(0);
    if status != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {status}: `{command}`\nsession_id: {}\noutput:\n{}",
            session.id, rendered.model_text
        )));
    }
    Ok(rendered)
}

async fn start_managed_session(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: Option<&Value>,
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
    let id = new_session_id();
    let log_path = sessions_directory.join(format!("{id}.log"));
    std::fs::write(&log_path, b"").map_err(|error| {
        ToolError::Execution(format!(
            "the session log `{}` cannot be created: {error}",
            log_path.display()
        ))
    })?;
    let terminal_id = shell
        .terminals
        .run(process_spec(
            config,
            working_directory,
            command,
            environment,
        ))
        .await
        .map_err(process_error)?;
    let session = Arc::new(ManagedSession {
        id,
        terminal_id,
        command: command.to_owned(),
        working_directory: working_directory.to_string_lossy().into_owned(),
        shell: config.executable.to_string_lossy().into_owned(),
        log_path,
        created_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default(),
        state: StdMutex::new(SessionState {
            status: SessionStatus::Running,
            exit_code: None,
            backpressure_dropped: false,
        }),
    });
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
/// of truth, which is what lets `bash_output` and `bash_log_file` answer for a
/// session long after it exited.
fn spawn_pump(terminals: TerminalManager, session: Arc<ManagedSession>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let Ok(read) = terminals.read(&session.terminal_id).await else {
                session.settle(SessionStatus::Killed, None);
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
                    if session.is_running() {
                        session.settle(
                            SessionStatus::Completed,
                            Some(exit_status(&final_read.state)),
                        );
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

fn append_chunks(session: &ManagedSession, chunks: &[ProcessChunk], dropped: bool) {
    if dropped && let Ok(mut state) = session.state.lock() {
        state.backpressure_dropped = true;
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

fn new_session_id() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let mut suffix = [0_u8; 4];
    // A collision would let one session read another's log, so the id carries
    // real entropy rather than a counter.
    if getrandom::fill(&mut suffix).is_err() {
        suffix = (stamp as u32).to_le_bytes();
    }
    format!(
        "{SESSION_PREFIX}_{stamp}_{}",
        suffix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Wraps one managed-family handler, which all share the same shape: a session
/// store, the call arguments, and the turn's output budget.
fn session_handler<F, Fut>(shell: Arc<SessionShell>, run: F) -> Arc<dyn ToolHandler>
where
    F: Fn(Arc<SessionShell>, Value, ToolOutputSink) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'static,
{
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let future = run(shell.clone(), invocation.arguments.clone(), output);
            Box::pin(future)
        },
    )
}

async fn managed_session(
    shell: &SessionShell,
    session_id: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    let sessions = shell.managed.lock().await;
    if let Some(session) = sessions.get(session_id) {
        return Ok(session.clone());
    }
    let active = sessions.keys().cloned().collect::<Vec<_>>();
    let listed = if active.is_empty() {
        "none".to_owned()
    } else {
        active.join(", ")
    };
    Err(ToolError::Execution(format!(
        "unknown session `{session_id}`; active sessions: {listed}"
    )))
}

/// Reads one session's log from `cursor` and reports where the next read starts.
fn managed_output(
    session: &ManagedSession,
    cursor: u64,
    limit: usize,
) -> Result<ToolExecutionOutput, ToolError> {
    let (output, next_cursor, truncated) = read_file_window(&session.log_path, cursor, limit)?;
    let (status, exit_code, dropped) = session.snapshot();
    let mut model_text = output.clone();
    if truncated {
        model_text.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    if dropped {
        model_text.push_str("\n[output was dropped while the session outran its buffer]");
    }
    Ok(ToolExecutionOutput {
        model_text,
        display: json!({"kind": "shell", "command": session.command}),
        typed_result: json!({
            "sessionId": session.id,
            "command": session.command,
            "status": status.as_str(),
            "exitCode": exit_code,
            "output": output,
            "nextCursor": next_cursor,
            "truncated": truncated,
            "outputPath": session.log_path.to_string_lossy(),
            "backpressureDropped": dropped,
        }),
        chunks: Vec::new(),
    })
}

/// Reads at most `limit` bytes of `path` starting at `cursor`.
fn read_file_window(
    path: &Path,
    cursor: u64,
    limit: usize,
) -> Result<(String, u64, bool), ToolError> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    let size = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if cursor >= size {
        return Ok((String::new(), size, false));
    }
    file.seek(SeekFrom::Start(cursor)).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    let mut buffer = vec![0_u8; limit];
    let read = file.read(&mut buffer).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    buffer.truncate(read);
    let next_cursor = cursor.saturating_add(read as u64);
    Ok((
        String::from_utf8_lossy(&buffer).into_owned(),
        next_cursor,
        size > next_cursor,
    ))
}

async fn kill_managed_session(
    shell: &SessionShell,
    session: &ManagedSession,
    status: SessionStatus,
) -> Result<(), ToolError> {
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

// --------------------------------------------------------------------------
// bash_output, bash_stdin, bash_sessions, bash_log_file
// --------------------------------------------------------------------------

async fn run_bash_output(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let session_id = string_argument(&arguments, "session_id")
        .unwrap_or_default()
        .to_owned();
    let session = managed_session(&shell, &session_id).await?;
    let cursor = arguments["cursor"].as_u64().unwrap_or(0);
    let wait = arguments["wait_seconds"]
        .as_f64()
        .unwrap_or(0.0)
        .clamp(0.0, MAX_POLL_SECONDS as f64);
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    while Instant::now() < deadline && session.is_running() && log_size(&session.log_path) <= cursor
    {
        tokio::time::sleep(PUMP_INTERVAL).await;
    }
    managed_output(&session, cursor, byte_limit(&arguments, &output))
}

fn log_size(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default()
}

async fn run_bash_stdin(
    shell: Arc<SessionShell>,
    arguments: Value,
    _output: ToolOutputSink,
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
    Ok(ToolExecutionOutput {
        model_text: format!("Wrote {} bytes to session {session_id}", bytes.len()),
        display: json!({"kind": "shell", "command": session.command}),
        typed_result: json!({
            "sessionId": session_id,
            "bytesWritten": bytes.len(),
            "status": session.snapshot().0.as_str(),
        }),
        chunks: Vec::new(),
    })
}

/// The bytes one `bash_stdin` call writes.
///
/// The reference model accepts exactly one of the three inputs and rejects
/// anything else, so there is no precedence to apply.
fn stdin_bytes(arguments: &Value) -> Result<Vec<u8>, ToolError> {
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

async fn run_bash_sessions(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = string_argument(&arguments, "action").unwrap_or("list");
    let limit = byte_limit(&arguments, &output);
    match action {
        "list" => {
            let sessions = shell.managed.lock().await;
            let infos = sessions
                .values()
                .map(|session| session.info())
                .collect::<Vec<_>>();
            Ok(ToolExecutionOutput {
                model_text: format!("{} bash sessions", infos.len()),
                display: json!({"kind": "shell", "command": "bash_sessions list"}),
                typed_result: json!({"action": "list", "sessions": infos}),
                chunks: Vec::new(),
            })
        }
        "inspect" => {
            let session = required_session(&shell, &arguments, "inspect").await?;
            let mut rendered = managed_output(&session, 0, limit)?;
            rendered.typed_result = json!({
                "action": "inspect",
                "session": session.info(),
                "output": rendered.typed_result["output"],
                "nextCursor": rendered.typed_result["nextCursor"],
                "truncated": rendered.typed_result["truncated"],
            });
            Ok(rendered)
        }
        "kill" => {
            // A session is owned by the Vibe session, not by the turn that
            // started it, which is what the reference does: any turn may
            // stop any session of the family.
            let session = required_session(&shell, &arguments, "kill").await?;
            kill_managed_session(&shell, &session, SessionStatus::Killed).await?;
            shell.managed.lock().await.remove(&session.id);
            Ok(ToolExecutionOutput {
                model_text: format!("Killed session {}", session.id),
                display: json!({"kind": "shell", "command": session.command}),
                typed_result: json!({"action": "kill", "session": session.info()}),
                chunks: Vec::new(),
            })
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
                for session in &sessions {
                    let _ = std::fs::remove_file(&session.log_path);
                }
            }
            Ok(ToolExecutionOutput {
                model_text: format!("Stopped {} bash sessions", killed.len()),
                display: json!({"kind": "shell", "command": "bash_sessions reset"}),
                typed_result: json!({"action": "reset", "sessions": killed}),
                chunks: Vec::new(),
            })
        }
        other => Err(ToolError::Execution(format!(
            "unknown bash_sessions action `{other}`; use `list`, `inspect`, `kill` or `reset`"
        ))),
    }
}

async fn required_session(
    shell: &SessionShell,
    arguments: &Value,
    action: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    let Some(session_id) = string_argument(arguments, "session_id") else {
        return Err(ToolError::SchemaViolation {
            path: "/session_id".to_owned(),
            message: format!("is required by the `{action}` action"),
        });
    };
    managed_session(shell, session_id).await
}

async fn run_bash_log_file(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = string_argument(&arguments, "action").unwrap_or_default();
    let path = resolve_log_path(&shell, &arguments)?;
    match action {
        "read" => {
            let offset = arguments["offset"].as_u64().unwrap_or(0);
            let limit = byte_limit(&arguments, &output);
            let (content, next_cursor, truncated) = read_file_window(&path, offset, limit)?;
            Ok(ToolExecutionOutput {
                model_text: content.clone(),
                display: json!({"kind": "shell", "command": "bash_log_file read"}),
                typed_result: json!({
                    "action": "read",
                    "path": path.to_string_lossy(),
                    "content": content,
                    "nextCursor": next_cursor,
                    "truncated": truncated,
                }),
                chunks: Vec::new(),
            })
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
            Ok(ToolExecutionOutput {
                model_text: format!("Wrote {} bytes to {}", content.len(), path.display()),
                display: json!({"kind": "shell", "command": format!("bash_log_file {action}")}),
                typed_result: json!({
                    "action": action,
                    "path": path.to_string_lossy(),
                    "bytesWritten": content.len(),
                }),
                chunks: Vec::new(),
            })
        }
        other => Err(ToolError::Execution(format!(
            "unknown bash_log_file action `{other}`; use `read`, `write` or `append`"
        ))),
    }
}

/// The file a `bash_log_file` call addresses.
///
/// A session id resolves to that session's own log. A relative path is joined
/// to the shell-tool directory and refused before any filesystem access when it
/// climbs out of it or names another family's session file.
fn resolve_log_path(shell: &SessionShell, arguments: &Value) -> Result<PathBuf, ToolError> {
    if let Some(session_id) = string_argument(arguments, "session_id") {
        // A session id names a file inside the session directory, so it is held
        // to the same rule as a relative path: one component, this family's.
        if !is_family_session_id(session_id) {
            return Err(ToolError::Execution(format!(
                "the log path must name a {SESSION_PREFIX} session file"
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
            .is_some_and(is_family_session_id)
    {
        return Err(ToolError::Execution(format!(
            "the log path must name a {SESSION_PREFIX} session file"
        )));
    }
    Ok(resolved)
}

/// Whether `candidate` is one plain name belonging to this shell family.
fn is_family_session_id(candidate: &str) -> bool {
    let mut components = Path::new(candidate).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && candidate.starts_with(&format!("{SESSION_PREFIX}_"))
}

async fn refuse_live_session_log(shell: &SessionShell, path: &Path) -> Result<(), ToolError> {
    let sessions = shell.managed.lock().await;
    if sessions
        .values()
        .any(|session| session.log_path == path && session.is_running())
    {
        return Err(ToolError::Execution(
            "a live session log cannot be written; use bash_stdin or wait for the session to exit"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
