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

use serde_json::Value;

use crate::platform::{Platform, parse_policy_path};
use crate::policy::{
    ApprovalAgent, PermissionContext, PermissionMode, PermissionRequirement, PermissionStore,
    PolicyGuardedTool,
};
use crate::process::ClientToolIo;
use crate::shell::{
    ShellAnalysis, ShellCommandLists, ShellConfig, ShellFlavor, ShellPolicyContext, analyze_shell,
};
use crate::tools::config::{ShellCommandConfig, ToolConfigResolver};
use crate::tools::{ToolError, ToolHandler, ToolHandlerFuture, ToolInvocation, ToolOutputSink};

use super::host::ShellFamily;
use super::session::SessionShell;
use super::{command_handler, resolve_log_path};

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
}

impl ShellCallPolicy {
    /// What the command runs under, with the overrides the command text cannot
    /// see already folded in.
    ///
    /// An override decides where the command runs, what interprets it and what
    /// it inherits, none of which an analysis of the text can see. So a call
    /// carrying one stops being allowed outright and reaches the operator
    /// instead.
    fn analysis(&self, arguments: &Value) -> Result<ShellAnalysis, ToolError> {
        let command = command_argument(arguments)?;
        let settings: ShellCommandConfig = self.config.view(&self.tool);
        let mut analysis = analyze(
            self.flavor,
            self.platform,
            &self.root,
            self.scratchpad.clone(),
            &command,
            &ShellCommandLists::from_config(&settings),
        );
        if self.overrides && !override_requirements(arguments, &self.root).is_empty() {
            analysis.mode = analysis.mode.min(PermissionMode::Ask);
            analysis.rationale.push(
                "the call overrides the working directory, the shell or the environment".to_owned(),
            );
        }
        Ok(analysis)
    }

    /// What the operator is asked to approve, derived from the same analysis
    /// the routing read.
    fn context(&self, arguments: &Value) -> Result<PermissionContext, ToolError> {
        let analysis = self.analysis(arguments)?;
        // The analysis already composed what the operator answers: one
        // requirement per session pattern, one per directory the call leaves
        // the workspace for, and one per `find` that runs a program. Rebuilding
        // them here would be a second vocabulary to keep in step with the first.
        let mut requirements = analysis.requirements;
        if requirements.is_empty() {
            // An analysis that composed none still needs something to approve,
            // which is the whole command under its own pattern.
            requirements.push(PermissionRequirement::command(&command_argument(
                arguments,
            )?));
        }
        if self.overrides {
            requirements.extend(override_requirements(arguments, &self.root));
        }
        let mut context = PermissionContext::asking(requirements);
        // A `cwd` override is a directory the call reaches, so it travels on
        // the context and is positioned against the trust roots the way a file
        // tool's path is. A root the operator revoked refuses the call rather
        // than becoming one more thing an approval reopens.
        if self.overrides
            && let Some(directory) = string_argument(arguments, "cwd")
        {
            context.paths.push(PathBuf::from(directory));
        }
        Ok(context)
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

pub(super) fn analyze(
    flavor: ShellFlavor,
    platform: Platform,
    working_directory: &Path,
    scratchpad: Option<PathBuf>,
    command: &str,
    lists: &ShellCommandLists,
) -> ShellAnalysis {
    let Ok(root) = parse_policy_path(platform, &working_directory.to_string_lossy()) else {
        // A working directory the policy cannot parse is not a reason to run
        // unanalyzed: the call falls back to asking.
        return ShellAnalysis {
            mode: PermissionMode::Ask,
            rationale: vec!["the working directory is not a policy path".to_owned()],
            commands: Vec::new(),
            path_operands: Vec::new(),
            requirements: Vec::new(),
        };
    };
    analyze_shell(
        flavor,
        command,
        &ShellPolicyContext::new(platform, root).with_scratchpad(scratchpad),
        lists,
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
        // Reference `_collect_outside_dirs` names the directory itself, joined
        // with `*`, rather than the file-shaped parent a file tool names.
        requirements.push(PermissionRequirement::outside_directory(
            &Path::new(directory).join("*").display().to_string(),
        ));
    }
    if let Some(shell) = string_argument(arguments, "shell") {
        // Reference `_build_context_permissions` carries the override verbatim
        // as both patterns, so approving one interpreter never approves another.
        requirements.push(PermissionRequirement::exact_command(&format!(
            "shell override: {shell}"
        )));
    }
    if let Some(names) = environment_names(arguments) {
        // The environment override is the one context permission the reference
        // widens for the session: the names change per call, so the session
        // pattern covers any of them.
        requirements.push(PermissionRequirement {
            scope: crate::policy::PermissionScope::CommandPattern,
            invocation_pattern: format!("env override: {names}"),
            session_pattern: "env override *".to_owned(),
            label: format!("env override: {names}"),
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

/// The log a `*_log_file` call touches, positioned against the trust roots.
///
/// The reference declares no requirement of its own here, so what decides is
/// the configured permission plus whether the log sits inside the workspace: a
/// session whose log directory was redirected outside it is asked about.
pub(super) fn log_file_requirements(
    shell: &SessionShell,
    arguments: &Value,
) -> Result<PermissionContext, ToolError> {
    let path = resolve_log_path(shell, arguments)?;
    Ok(PermissionContext::deferred().over_paths(vec![path]))
}

// --------------------------------------------------------------------------
// Arguments
// --------------------------------------------------------------------------

pub(super) fn command_argument(arguments: &Value) -> Result<String, ToolError> {
    let command = arguments["command"].as_str().unwrap_or_default().trim();
    if command.is_empty() {
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

/// The foreground wait, in seconds, bounded by the configured maximum.
pub(super) fn timeout_argument(arguments: &Value, settings: &ShellCommandConfig) -> u64 {
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
        .unwrap_or(settings.default_timeout);
    let ceiling = settings.max_timeout_seconds.max(1.0) as u64;
    requested.clamp(1, ceiling)
}

/// The read window one inline answer may carry: what the call asked for,
/// bounded by the configured window and by what the turn's budget has left.
pub(super) fn byte_limit(
    arguments: &Value,
    sink: &ToolOutputSink,
    max_inline_bytes: usize,
) -> usize {
    let requested = arguments["max_bytes"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(max_inline_bytes);
    requested
        .min(max_inline_bytes)
        .min(sink.remaining_bytes().max(1))
}
