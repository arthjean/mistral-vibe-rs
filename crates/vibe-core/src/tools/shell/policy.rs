//! What a shell call is allowed to do, and how the call's arguments are read.
//!
//! Two invariants live here. Every command is analyzed before it runs, and a
//! command the analysis does not permit outright reaches the operator as an
//! approval request rather than a process. And an override the analysis of the
//! command text cannot see (`cwd`, `shell`, `env`) lowers that mode itself,
//! because it decides where the command runs, what interprets it and what it
//! inherits.
//!
//! [`ShellCallPolicy`] is the single answer both readings come from: the guard
//! routes on it and the permission resolver derives its requirements from it,
//! so the two can never disagree about what a call is asking for.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::platform::{Platform, parse_policy_path};
use crate::policy::{
    ApprovalAgent, PermissionContext, PermissionMode, PermissionRequirement, PermissionStore,
    PolicyGuardedTool,
};
use crate::process::ClientToolIo;
use crate::shell::{
    ShellAnalysis, ShellCommandLists, ShellConfig, ShellFlavor, ShellPolicyContext, analyze_shell,
    override_requirements, pager_input_permission,
};
use crate::tools::config::{ShellCommandConfig, ToolConfigResolver};
use crate::tools::{ToolError, ToolHandler, ToolHandlerFuture, ToolInvocation, ToolOutputSink};

use super::command_handler;
use super::host::ShellFamily;
use super::session::SessionShell;

// --------------------------------------------------------------------------
// Policy
// --------------------------------------------------------------------------

/// Everything one command variant reads to decide what a call may do.
///
/// The routing guard and the permission resolver both need the same answer:
/// the analysis of the command text, the overrides the text cannot see, and the
/// requirements those compose into. Holding it once means the two readings are
/// the same function rather than two copies to keep in step.
struct ShellCallPolicy {
    flavor: ShellFlavor,
    platform: Platform,
    /// The directory a call runs in unless it overrides one.
    root: PathBuf,
    /// The session's own scratchpad, whose paths raise no requirement.
    scratchpad: Option<PathBuf>,
    /// The `tools.<family>` settings, re-read per call so a raised limit or an
    /// edited list applies to the next command.
    config: ToolConfigResolver,
    tool: String,
    /// Whether this variant publishes `cwd`, `shell` and `env`. Reference
    /// `GitBashArgs` and `WindowsShellArgs` carry them on the legacy variant
    /// too, so the Windows families answer for them whichever variant is
    /// selected.
    overrides: bool,
    /// The store whose authorized roots the command's operands and its `cwd`
    /// override are positioned against, read per call.
    policy: PermissionStore,
}

impl ShellCallPolicy {
    /// What the command runs under, with the overrides the command text cannot
    /// see already folded in.
    ///
    /// An override decides where the command runs, what interprets it and what
    /// it inherits, none of which an analysis of the text can see. The
    /// reference resolver for these variants reads all three: the `cwd` is
    /// where operands resolve and is itself an outside directory when it leaves
    /// the workspace, and a custom shell or environment carries a requirement
    /// of its own that keeps an allowlisted command from running unasked.
    fn analysis(&self, arguments: &Value) -> Result<ShellAnalysis, ToolError> {
        let command = command_argument(arguments)?;
        let settings: ShellCommandConfig = self.config.view(&self.tool);
        let overrides = self.overrides.then(|| CallOverrides {
            cwd: string_argument(arguments, "cwd").map(ToOwned::to_owned),
            requirements: context_requirements(arguments),
            environment: arguments
                .get("env")
                .and_then(Value::as_object)
                .map(|overrides| {
                    overrides
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
        Ok(analyze(
            self.flavor,
            self.platform,
            &self.root,
            &self.policy.workspace_roots(),
            self.scratchpad.clone(),
            &command,
            &ShellCommandLists::from_config(&settings),
            overrides,
        ))
    }

    /// What the operator is asked to approve, derived from the same analysis
    /// the routing read.
    fn context(&self, arguments: &Value) -> Result<PermissionContext, ToolError> {
        let analysis = self.analysis(arguments)?;
        // An analysis that composed nothing is the reference resolving `None`:
        // the configured permission decides and nothing is asked about.
        if analysis.requirements.is_empty() {
            return Ok(PermissionContext::deferred());
        }
        Ok(PermissionContext::asking(analysis.requirements))
    }
}

/// Runs the call's [`ShellCallPolicy`] and routes by what it decides.
///
/// The reference resolves a command to `ALWAYS`, `ASK` or `NEVER` before it
/// runs; this reproduces that split on top of the workspace's own analyzer. An
/// `Always` command executes directly, an `Ask` command goes through the
/// permission store, and a `Never` command is refused before a process exists.
struct ShellPolicyGuard {
    policy: Arc<ShellCallPolicy>,
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
            let analysis = self.policy.analysis(&invocation.arguments)?;
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

/// What one command variant needs to reach a process under policy.
pub(super) struct CommandWiring {
    pub(super) family: ShellFamily,
    pub(super) shell: Arc<SessionShell>,
    /// The interpreter this family drives.
    pub(super) shell_config: ShellConfig,
    /// The `tools.<family>` settings the call reads its limits and lists from.
    pub(super) tool_config: ToolConfigResolver,
    pub(super) working_directory: PathBuf,
    pub(super) platform: Platform,
    pub(super) policy: PermissionStore,
    pub(super) approval: Arc<dyn ApprovalAgent>,
    pub(super) managed: bool,
    pub(super) client_io: Option<ClientToolIo>,
    /// The session's own scratchpad, whose paths raise no requirement.
    pub(super) scratchpad: Option<PathBuf>,
}

pub(super) fn guarded_command(wiring: CommandWiring) -> Arc<dyn ToolHandler> {
    let CommandWiring {
        family,
        shell,
        shell_config,
        tool_config,
        working_directory,
        platform,
        policy,
        approval,
        managed,
        client_io,
        scratchpad,
    } = wiring;
    let inner = command_handler(
        shell,
        shell_config.clone(),
        tool_config.clone(),
        family.name().to_owned(),
        working_directory.clone(),
        managed,
        client_io,
    );
    let call_policy = Arc::new(ShellCallPolicy {
        flavor: shell_config.flavor,
        platform,
        root: working_directory,
        scratchpad,
        config: tool_config,
        tool: family.name().to_owned(),
        overrides: managed || family != ShellFamily::Bash,
        policy: policy.clone(),
    });
    let resolver = call_policy.clone();
    let guarded = Arc::new(PolicyGuardedTool::new(
        family.name(),
        policy,
        approval,
        Arc::new(move |invocation: &ToolInvocation| resolver.context(&invocation.arguments)),
        inner.clone(),
    ));
    Arc::new(ShellPolicyGuard {
        policy: call_policy,
        guarded,
        inner,
    })
}

/// The overrides a call carries beside its command.
pub(super) struct CallOverrides {
    /// The `cwd` argument as the call wrote it.
    pub(super) cwd: Option<String>,
    /// What the custom shell and environment require.
    pub(super) requirements: Vec<PermissionRequirement>,
    /// The environment overrides themselves, which a PowerShell path expands.
    pub(super) environment: Vec<(String, String)>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn analyze(
    flavor: ShellFlavor,
    platform: Platform,
    working_directory: &Path,
    listed_roots: &[PathBuf],
    scratchpad: Option<PathBuf>,
    command: &str,
    lists: &ShellCommandLists,
    overrides: Option<CallOverrides>,
) -> ShellAnalysis {
    let Ok(root) = parse_policy_path(platform, &working_directory.to_string_lossy()) else {
        // A working directory the policy cannot parse is not a reason to run
        // unanalyzed: the call falls back to asking.
        return ShellAnalysis {
            mode: PermissionMode::Ask,
            rationale: vec!["the working directory is not a policy path".to_owned()],
            commands: Vec::new(),
            path_operands: Vec::new(),
            requirements: vec![PermissionRequirement::exact_command(command)],
        };
    };
    // A listed root the policy cannot parse is left out, which positions its
    // operands outside and asks about them rather than granting them.
    let listed = listed_roots
        .iter()
        .filter_map(|root| parse_policy_path(platform, &root.to_string_lossy()).ok());
    let mut context = ShellPolicyContext::new(platform, root)
        .with_scratchpad(scratchpad)
        .with_roots(listed);
    if let Some(overrides) = overrides {
        context = context
            .managed(flavor, overrides.cwd.as_deref(), overrides.requirements)
            .with_environment(overrides.environment);
    }
    analyze_shell(flavor, command, &context, lists)
}

/// What a custom shell and a custom environment require.
fn context_requirements(arguments: &Value) -> Vec<PermissionRequirement> {
    let names = arguments
        .get("env")
        .and_then(Value::as_object)
        .map(|overrides| overrides.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    override_requirements(string_argument(arguments, "shell"), &names)
}

/// What a `*_log_file` call needs.
///
/// Reference `BashLogFile.resolve_permission`: a read is always granted, and a
/// write or an append falls to the configured permission. Neither raises a
/// requirement, so where the log sits is not asked about.
pub(super) fn log_file_requirements(arguments: &Value) -> PermissionContext {
    if arguments["action"].as_str() == Some("read") {
        return PermissionContext::settled(PermissionMode::Always);
    }
    PermissionContext::deferred()
}

/// What a `*_stdin` call needs.
///
/// Reference `BashStdin.resolve_permission`: input to a session whose command
/// runs `git`, `less` or `more` can reach a pager's own command prompt, so it is
/// asked about under the session's own pattern, as is input to a session this
/// family does not know. Anything else falls to the configured permission.
pub(super) fn stdin_requirements(
    shell: &SessionShell,
    flavor: ShellFlavor,
    arguments: &Value,
) -> PermissionContext {
    let session_id = arguments["session_id"].as_str().unwrap_or_default();
    let live = shell.managed.try_lock().ok().and_then(|sessions| {
        sessions
            .get(session_id)
            .map(|session| session.command.clone())
    });
    let command = live.or_else(|| {
        shell.orphan(session_id).and_then(|manifest| {
            manifest
                .get("command")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
    });
    pager_input_permission(flavor, session_id, command.as_deref())
}

// --------------------------------------------------------------------------
// Arguments
// --------------------------------------------------------------------------

/// The command a call runs, exactly as the call spelled it: reference
/// `TerminalSessionManager.start` refuses one that is blank once stripped, but
/// runs and reports the text it was handed.
pub(super) fn command_argument(arguments: &Value) -> Result<String, ToolError> {
    let command = arguments["command"].as_str().unwrap_or_default();
    if command.trim().is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/command".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    Ok(command.to_owned())
}

pub(super) fn string_argument<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

/// The foreground wait of a managed command, bounded by the configured
/// maximum.
///
/// Reference `ExperimentalBash.run` reads the legacy `timeout` first, even at
/// zero, then `timeout_seconds`, then the configured default, and caps the
/// result at `max_timeout_seconds`. A wait of zero checks the session once and
/// hands it to the background.
pub(super) fn timeout_argument(arguments: &Value, settings: &ShellCommandConfig) -> Duration {
    let requested = arguments["timeout"]
        .as_i64()
        .map(|seconds| seconds as f64)
        .or_else(|| arguments["timeout_seconds"].as_f64())
        .unwrap_or(settings.default_timeout as f64);
    let bounded = requested.min(settings.max_timeout_seconds).max(0.0);
    Duration::try_from_secs_f64(bounded).unwrap_or_default()
}

/// Whether a managed wait that expires kills the session: reference
/// `hard_timeout or timeout is not None`.
pub(super) fn is_hard_timeout(arguments: &Value) -> bool {
    arguments["hard_timeout"].as_bool().unwrap_or(false) || arguments["timeout"].as_i64().is_some()
}

/// Renders a wait the way Python's `{timeout:g}` renders a float.
pub(super) fn render_seconds(timeout: Duration) -> String {
    let seconds = timeout.as_secs_f64();
    if seconds.fract() == 0.0 {
        format!("{seconds:.0}")
    } else {
        format!("{seconds}")
    }
}

/// The read window one inline answer may carry: what the call asked for,
/// bounded by the configured window and by what the turn's budget has left.
pub(super) fn byte_limit(
    arguments: &Value,
    sink: &ToolOutputSink,
    max_inline_bytes: usize,
) -> usize {
    // Reference `args.max_bytes or config.max_inline_bytes`, read through the
    // `max_chars` alias when the canonical name is absent. A request larger
    // than the configured window is honored, as it is upstream; only the
    // turn's remaining budget bounds it further.
    let requested = arguments["max_bytes"]
        .as_u64()
        .or_else(|| arguments["max_chars"].as_u64())
        .filter(|value| *value > 0)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(max_inline_bytes);
    requested.min(sink.remaining_bytes().max(1))
}
