use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock as StdRwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use url::Url;

use crate::matching::pattern_matches;
use crate::tools::config::{SharedToolConfig, ToolConfigResolver, permission_label};
use crate::tools::{ToolError, ToolHandler, ToolInvocation, ToolOutputSink};

pub type ApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ApprovalDecision, PolicyError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PermissionMode {
    Never,
    Ask,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustDecision {
    Trusted,
    SessionTrusted,
    Untrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustRootKind {
    Workspace,
    AddDirectory,
    Ancestor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionRequirement {
    Read { path: PathBuf },
    Write { path: PathBuf },
    Shell { command: String },
    Network { url: Url },
    Mcp { server: String, tool: String },
    Destructive { action: String },
}

impl PermissionRequirement {
    #[must_use]
    pub fn scope(&self) -> String {
        match self {
            Self::Read { path } => format!("read {}", path.display()),
            Self::Write { path } => format!("write {}", path.display()),
            Self::Shell { command } => format!("shell {command}"),
            Self::Network { url } => format!("network {url}"),
            Self::Mcp { server, tool } => format!("mcp {server}/{tool}"),
            Self::Destructive { action } => format!("destructive {action}"),
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Read { path } | Self::Write { path } => {
                format!("outside workdir ({})", path.display())
            }
            Self::Shell { command } => command.clone(),
            Self::Network { url } => format!(
                "fetching from {}",
                url.host_str().unwrap_or_else(|| url.as_str())
            ),
            Self::Mcp { server, tool } => format!("MCP tool ({server}/{tool})"),
            Self::Destructive { action } => action.clone(),
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Self::Read { path } | Self::Write { path } => Some(path),
            _ => None,
        }
    }

    /// The text a tool's configured `allowlist`, `denylist` and
    /// `sensitive_patterns` are matched against.
    ///
    /// Reference `resolve_path_permission` matches a file tool's lists against
    /// the resolved absolute path, and `BashTool._is_allowlisted` matches the
    /// shell's against one extracted command segment.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::Read { path } | Self::Write { path } => path.display().to_string(),
            Self::Shell { command } => command.clone(),
            Self::Network { url } => url.to_string(),
            Self::Mcp { server, tool } => format!("{server}/{tool}"),
            Self::Destructive { action } => action.clone(),
        }
    }

    /// Whether the configured lists decide this requirement here, or the tool
    /// that produced it already composed them.
    ///
    /// A shell command is the one case they do not: reference
    /// `_is_unconditionally_allowed` grants an allowlisted command only when no
    /// operand leaves the working directory, a condition that lives in the
    /// shell analysis and not in the requirement. Matching the same lists again
    /// here would drop that condition and auto-allow `cat /etc/passwd`.
    fn lists_decide(&self) -> bool {
        !matches!(self, Self::Shell { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub tool: String,
    pub scope: String,
    pub mode: PermissionMode,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyResolution {
    pub mode: PermissionMode,
    pub rationale: String,
    pub matched_rule: Option<PermissionRule>,
    pub required_permissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub input: Value,
    pub requirements: Vec<PermissionRequirement>,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    ApproveOnce,
    ApproveForSession,
    ApprovePermanently,
    Deny,
    CancelTurn,
}

pub trait ApprovalAgent: Send + Sync {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a>;
}

/// What a tool family publishes its tools behind: the session's permission
/// store, the agent that answers an approval, and the configuration each tool
/// reads at every call.
///
/// The three travel together through every registration, and the resolver is
/// the store's own, so a family and the policy in front of it always read one
/// composition rather than two.
#[derive(Clone)]
pub struct ToolGuard {
    pub policy: PermissionStore,
    pub approval: Arc<dyn ApprovalAgent>,
    pub config: ToolConfigResolver,
}

impl ToolGuard {
    #[must_use]
    pub fn new(policy: PermissionStore, approval: Arc<dyn ApprovalAgent>) -> Self {
        let config = policy.tool_config();
        Self {
            policy,
            approval,
            config,
        }
    }
}

pub type RequirementResolver =
    dyn Fn(&ToolInvocation) -> Result<Vec<PermissionRequirement>, ToolError> + Send + Sync;

pub struct PolicyGuardedTool {
    name: String,
    store: PermissionStore,
    approval: Arc<dyn ApprovalAgent>,
    requirements: Arc<RequirementResolver>,
    inner: Arc<dyn ToolHandler>,
}

impl PolicyGuardedTool {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        store: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
        requirements: Arc<RequirementResolver>,
        inner: Arc<dyn ToolHandler>,
    ) -> Self {
        Self {
            name: name.into(),
            store,
            approval,
            requirements,
            inner,
        }
    }
}

impl ToolHandler for PolicyGuardedTool {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> crate::tools::ToolHandlerFuture<'a> {
        let invocation = invocation.clone();
        let name = self.name.clone();
        let store = self.store.clone();
        let approval = self.approval.clone();
        let requirements = self.requirements.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let requirements = requirements(&invocation)?;
            let subjects = requirements
                .iter()
                .filter(|requirement| requirement.lists_decide())
                .map(PermissionRequirement::subject)
                .collect();
            let lease = store
                .authorize(
                    &name,
                    invocation.arguments.clone(),
                    requirements,
                    subjects,
                    approval.as_ref(),
                )
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            // The read guard is held across the side effect on purpose: it is
            // what makes revocation atomic. `revoke_trust` needs the write lock,
            // so it cannot land between this revalidation and the effect it
            // authorizes, and any waiting revocation applies to the next call.
            let state = store.state.read().await;
            lease
                .revalidate_locked(&state)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            inner.invoke(&invocation, output).await
        })
    }
}

#[derive(Debug, Clone)]
struct TrustRoot {
    canonical_path: PathBuf,
    decision: TrustDecision,
    kind: TrustRootKind,
}

impl TrustRoot {
    /// Resolves `path` to the canonical root a trust decision applies to.
    fn resolve(
        path: &Path,
        decision: TrustDecision,
        kind: TrustRootKind,
    ) -> Result<Self, PolicyError> {
        let canonical_path = canonicalize_for_policy(path)?;
        if canonical_path.parent().is_none() {
            return Err(PolicyError::UnsafeTrustRoot(canonical_path));
        }
        Ok(Self {
            canonical_path,
            decision,
            kind,
        })
    }
}

#[derive(Debug, Default)]
struct PolicyState {
    revision: u64,
    rules: Vec<PermissionRule>,
    roots: BTreeMap<PathBuf, TrustRoot>,
}

impl PolicyState {
    fn insert_root(&mut self, root: TrustRoot) {
        self.roots.insert(root.canonical_path.clone(), root);
        self.revision = self.revision.saturating_add(1);
    }

    /// The most specific trust root covering `canonical`, if any.
    fn closest_root(&self, canonical: &Path) -> Option<&TrustRoot> {
        self.roots
            .values()
            .filter(|root| canonical.starts_with(&root.canonical_path))
            .max_by_key(|root| root.canonical_path.components().count())
    }
}

#[derive(Clone, Debug)]
pub struct PermissionStore {
    state: Arc<RwLock<PolicyState>>,
    approvals: Arc<Mutex<()>>,
    /// Reference `PermissionStore._tool_permissions`: the per-tool permission a
    /// session grants, which outranks the configured one and dies with the
    /// session.
    tool_permissions: Arc<StdRwLock<BTreeMap<String, PermissionMode>>>,
    /// The configuration each tool's permission, allowlist, denylist and
    /// sensitive patterns are read from at every resolution.
    tool_config: ToolConfigResolver,
}

impl Default for PermissionStore {
    /// A store holding no operator settings, whose session overrides still
    /// count: [`Self::set_tool_permission`] is only observed by a resolver this
    /// store narrowed, so the default one is narrowed here rather than left for
    /// a caller to remember.
    fn default() -> Self {
        Self {
            state: Arc::default(),
            approvals: Arc::default(),
            tool_permissions: Arc::default(),
            tool_config: ToolConfigResolver::new(),
        }
        .with_tool_config(ToolConfigResolver::new())
    }
}

impl PermissionStore {
    /// The same store reading its per-tool settings from `resolver`.
    ///
    /// The resolver returned by [`Self::tool_config`] is this one narrowed to
    /// the session, so the tool families and the policy read one composition
    /// rather than two.
    #[must_use]
    pub fn with_tool_config(mut self, resolver: ToolConfigResolver) -> Self {
        let permissions = self.tool_permissions.clone();
        self.tool_config = resolver.with_session_permissions(Arc::new(move |tool| {
            permissions
                .read()
                .ok()
                .and_then(|stored| stored.get(tool).copied())
        }));
        self
    }

    /// The session-scoped resolver every tool this store guards reads through.
    #[must_use]
    pub fn tool_config(&self) -> ToolConfigResolver {
        self.tool_config.clone()
    }

    /// Grants `tool` a session permission, outranking the configured one.
    ///
    /// Reference `AgentLoop.set_tool_permission`, which is where an approval
    /// carrying no granular requirement lands: there is no pattern to store a
    /// rule under, so the tool itself is what was approved.
    pub fn set_tool_permission(&self, tool: &str, mode: PermissionMode) {
        if let Ok(mut stored) = self.tool_permissions.write() {
            stored.insert(tool.to_owned(), mode);
        }
    }

    /// The session permission granted to `tool`, if any.
    #[must_use]
    pub fn tool_permission(&self, tool: &str) -> Option<PermissionMode> {
        self.tool_permissions
            .read()
            .ok()
            .and_then(|stored| stored.get(tool).copied())
    }

    /// The permission settings `tool` resolves right now.
    #[must_use]
    pub fn tool_settings(&self, tool: &str) -> SharedToolConfig {
        self.tool_config.view(tool)
    }
    pub async fn add_rule(&self, rule: PermissionRule) {
        let mut state = self.state.write().await;
        state.rules.push(rule);
        state.revision = state.revision.saturating_add(1);
    }

    pub fn try_replace_rules_with_rationale_prefix(
        &self,
        rationale_prefix: &str,
        rules: Vec<PermissionRule>,
    ) -> Result<(), PolicyError> {
        let mut state = self.state.try_write().map_err(|_| PolicyError::Busy)?;
        state
            .rules
            .retain(|rule| !rule.rationale.starts_with(rationale_prefix));
        state.rules.extend(rules);
        state.revision = state.revision.saturating_add(1);
        Ok(())
    }

    pub async fn set_trust(
        &self,
        path: impl AsRef<Path>,
        decision: TrustDecision,
        kind: TrustRootKind,
    ) -> Result<(), PolicyError> {
        let root = TrustRoot::resolve(path.as_ref(), decision, kind)?;
        self.state.write().await.insert_root(root);
        Ok(())
    }

    /// Non-blocking [`PermissionStore::set_trust`], for callers holding a lock.
    pub fn try_set_trust(
        &self,
        path: impl AsRef<Path>,
        decision: TrustDecision,
        kind: TrustRootKind,
    ) -> Result<(), PolicyError> {
        let root = TrustRoot::resolve(path.as_ref(), decision, kind)?;
        self.state
            .try_write()
            .map_err(|_| PolicyError::Busy)?
            .insert_root(root);
        Ok(())
    }

    pub fn try_trust_decision(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Option<TrustDecision>, PolicyError> {
        let canonical_path = canonicalize_for_policy(path.as_ref())?;
        Ok(self
            .state
            .try_read()
            .map_err(|_| PolicyError::Busy)?
            .closest_root(&canonical_path)
            .map(|root| root.decision))
    }

    pub async fn revoke_trust(&self, path: impl AsRef<Path>) -> Result<(), PolicyError> {
        let root = TrustRoot::resolve(
            path.as_ref(),
            TrustDecision::Untrusted,
            TrustRootKind::Workspace,
        )?;
        self.state.write().await.insert_root(root);
        Ok(())
    }

    pub async fn resolve(
        &self,
        tool: &str,
        requirements: &[PermissionRequirement],
    ) -> Result<PolicyResolution, PolicyError> {
        let settings = self.tool_settings(tool);
        let subjects = requirements
            .iter()
            .filter(|requirement| requirement.lists_decide())
            .map(PermissionRequirement::subject)
            .collect::<Vec<_>>();
        let state = self.state.read().await;
        resolve_locked(&state, tool, requirements, &subjects, &settings)
    }

    pub async fn authorize(
        &self,
        tool: &str,
        input: Value,
        requirements: Vec<PermissionRequirement>,
        subjects: Vec<String>,
        approval: &dyn ApprovalAgent,
    ) -> Result<PolicyLease, PolicyError> {
        // The settings are read once per call rather than held: an operator who
        // raises a budget or a permission between two turns is obeyed on the
        // next one without the surface being registered again.
        let settings = Arc::new(self.tool_settings(tool));
        let subjects = Arc::new(subjects);
        let state = self.state.read().await;
        let resolution = resolve_locked(&state, tool, &requirements, &subjects, &settings)?;
        match resolution.mode {
            PermissionMode::Always => {
                let revision = state.revision;
                drop(state);
                Ok(self.lease(revision, tool, requirements, subjects, settings))
            }
            PermissionMode::Never => Err(PolicyError::Denied(resolution.rationale)),
            PermissionMode::Ask => {
                drop(state);
                let _approval_guard = self.approvals.lock().await;
                let state = self.state.read().await;
                let resolution = resolve_locked(&state, tool, &requirements, &subjects, &settings)?;
                match resolution.mode {
                    PermissionMode::Always => {
                        return Ok(self.lease(
                            state.revision,
                            tool,
                            requirements,
                            subjects,
                            settings,
                        ));
                    }
                    PermissionMode::Never => {
                        return Err(PolicyError::Denied(resolution.rationale));
                    }
                    PermissionMode::Ask => {}
                }
                let revision = state.revision;
                drop(state);
                let decision = approval
                    .request(ApprovalRequest {
                        tool: tool.to_owned(),
                        input,
                        requirements: requirements.clone(),
                        rationale: resolution.rationale,
                    })
                    .await?;
                match decision {
                    ApprovalDecision::ApproveOnce => {
                        let state = self.state.read().await;
                        if state.revision != revision {
                            let current =
                                resolve_locked(&state, tool, &requirements, &subjects, &settings)?;
                            if current.mode == PermissionMode::Never {
                                return Err(PolicyError::Denied(current.rationale));
                            }
                        }
                        Ok(self.lease(state.revision, tool, requirements, subjects, settings))
                    }
                    ApprovalDecision::ApproveForSession | ApprovalDecision::ApprovePermanently => {
                        let persistence = if decision == ApprovalDecision::ApprovePermanently {
                            "permanent approval"
                        } else {
                            "session approval"
                        };
                        // An approval carrying no granular requirement has no
                        // pattern to store a rule under, so what was approved is
                        // the tool: reference `approve_always` sets the session
                        // permission for exactly that case.
                        if requirements.is_empty() {
                            self.set_tool_permission(tool, PermissionMode::Always);
                        }
                        let mut state = self.state.write().await;
                        state
                            .rules
                            .extend(requirements.iter().map(|requirement| PermissionRule {
                                tool: tool.to_owned(),
                                scope: requirement.scope(),
                                mode: PermissionMode::Always,
                                rationale: persistence.to_owned(),
                            }));
                        state.revision = state.revision.saturating_add(1);
                        Ok(self.lease(state.revision, tool, requirements, subjects, settings))
                    }
                    ApprovalDecision::Deny => {
                        Err(PolicyError::Denied("approval denied".to_owned()))
                    }
                    ApprovalDecision::CancelTurn => Err(PolicyError::TurnCancelled),
                }
            }
        }
    }

    fn lease(
        &self,
        revision: u64,
        tool: &str,
        requirements: Vec<PermissionRequirement>,
        subjects: Arc<Vec<String>>,
        settings: Arc<SharedToolConfig>,
    ) -> PolicyLease {
        PolicyLease {
            store: self.clone(),
            revision,
            tool: tool.to_owned(),
            requirements,
            subjects,
            settings,
        }
    }
}

#[derive(Clone)]
pub struct PolicyLease {
    store: PermissionStore,
    revision: u64,
    tool: String,
    requirements: Vec<PermissionRequirement>,
    /// What the lists were matched against, carried so revalidation asks the
    /// same question rather than a newer one.
    subjects: Arc<Vec<String>>,
    settings: Arc<SharedToolConfig>,
}

impl PolicyLease {
    pub async fn revalidate(&self) -> Result<(), PolicyError> {
        let state = self.store.state.read().await;
        self.revalidate_locked(&state)
    }

    /// Revalidates against a state the caller already holds a guard on.
    ///
    /// Callers that must keep policy frozen across a side effect hold the read
    /// guard themselves and use this rather than [`Self::revalidate`].
    fn revalidate_locked(&self, state: &PolicyState) -> Result<(), PolicyError> {
        if state.revision == self.revision {
            return Ok(());
        }
        let resolution = resolve_locked(
            state,
            &self.tool,
            &self.requirements,
            &self.subjects,
            &self.settings,
        )?;
        if resolution.mode == PermissionMode::Always {
            Ok(())
        } else {
            Err(PolicyError::StaleApproval)
        }
    }
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("approval became stale before the side effect")]
    StaleApproval,
    #[error("turn cancelled during approval")]
    TurnCancelled,
    #[error("cannot trust filesystem root `{0}`")]
    UnsafeTrustRoot(PathBuf),
    #[error("cannot resolve policy path `{path}`: {source}")]
    PathResolution {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("approval failed: {0}")]
    Approval(String),
    #[error("permission state is busy with an active side effect")]
    Busy,
}

fn resolve_locked(
    state: &PolicyState,
    tool: &str,
    requirements: &[PermissionRequirement],
    subjects: &[String],
    settings: &SharedToolConfig,
) -> Result<PolicyResolution, PolicyError> {
    let required_permissions = requirements
        .iter()
        .map(PermissionRequirement::scope)
        .collect::<Vec<_>>();
    // A tool configured to `never` is refused outright, whatever a rule or a
    // trusted root would otherwise say: reference `get_tool_config` composes
    // that permission from the operator's table and the session override, and
    // no approval reopens it.
    if settings.permission == PermissionMode::Never {
        return Ok(PolicyResolution {
            mode: PermissionMode::Never,
            rationale: format!("`{tool}` is configured to never run"),
            matched_rule: None,
            required_permissions,
        });
    }
    let listed = list_decision(tool, subjects, settings);
    // The denylist closes a subject before anything else looks at it, so
    // neither a stored approval nor a trusted root reopens it. Reference
    // `resolve_path_permission` answers NEVER before it answers ALWAYS.
    if let Some((PermissionMode::Never, rationale)) = &listed {
        return Ok(PolicyResolution {
            mode: PermissionMode::Never,
            rationale: rationale.clone(),
            matched_rule: None,
            required_permissions,
        });
    }
    let mut effective = PermissionMode::Always;
    let mut rationales = Vec::new();
    let mut matched_rule = None;
    for requirement in requirements {
        let scope = requirement.scope();
        let rule = best_rule(&state.rules, tool, &scope);
        let (mode, rationale) = if let Some(rule) = rule {
            matched_rule = Some(rule.clone());
            (rule.mode, rule.rationale.clone())
        } else if let Some(path) = requirement.path() {
            // A list match only ever tightens what the path itself resolves
            // to. The sensitive patterns exist to turn a granted read into a
            // prompt, not to turn a root the operator refused into one.
            let resolved = resolve_path(state, path)?;
            match listed.clone() {
                Some(decision) if decision.0 < resolved.0 => decision,
                _ => resolved,
            }
        } else if let Some(decision) = listed.clone() {
            decision
        } else {
            (
                settings.permission,
                format!(
                    "no explicit policy covers `{scope}`; `{tool}` is configured to {}",
                    permission_label(settings.permission)
                ),
            )
        };
        effective = effective.min(mode);
        rationales.push(rationale);
    }
    if requirements.is_empty() {
        // Reference `get_tool_config().permission` is the whole decision for a
        // tool that produces no granular requirement, unless its own lists
        // already answered for what it was called on.
        let (mode, rationale) = listed.unwrap_or_else(|| {
            (
                settings.permission,
                format!(
                    "`{tool}` declared no permission requirement and is configured to {}",
                    permission_label(settings.permission)
                ),
            )
        });
        effective = mode;
        rationales.push(rationale);
    }
    Ok(PolicyResolution {
        mode: effective,
        rationale: rationales.join("; "),
        matched_rule,
        required_permissions,
    })
}

/// What the tool's three configured lists say about what it was called on, or
/// [`None`] when none of them matches.
///
/// Reference `resolve_path_permission` and `BashTool._is_unconditionally_allowed`
/// compose the same order: a denylist match refuses, a sensitive match asks even
/// at permission `always`, and a grant needs every subject allowlisted rather
/// than any one of them.
fn list_decision(
    tool: &str,
    subjects: &[String],
    settings: &SharedToolConfig,
) -> Option<(PermissionMode, String)> {
    if subjects.is_empty() {
        return None;
    }
    for subject in subjects {
        if let Some(pattern) = matched_pattern(&settings.denylist, subject) {
            return Some((
                PermissionMode::Never,
                format!("`{subject}` matches the `{tool}` denylist entry `{pattern}`"),
            ));
        }
    }
    for subject in subjects {
        if let Some(pattern) = matched_pattern(&settings.sensitive_patterns, subject) {
            return Some((
                PermissionMode::Ask,
                format!("`{subject}` matches the `{tool}` sensitive pattern `{pattern}`"),
            ));
        }
    }
    let granted = subjects
        .iter()
        .map(|subject| matched_pattern(&settings.allowlist, subject))
        .collect::<Option<Vec<_>>>()?;
    let pattern = granted.first()?;
    Some((
        PermissionMode::Always,
        format!("every subject matches the `{tool}` allowlist, first `{pattern}`"),
    ))
}

/// The first entry of `patterns` matching `subject`, in the order the operator
/// wrote them.
fn matched_pattern<'a>(patterns: &'a [String], subject: &str) -> Option<&'a String> {
    patterns
        .iter()
        .find(|pattern| pattern_matches(pattern, subject))
}

fn best_rule<'a>(
    rules: &'a [PermissionRule],
    tool: &str,
    scope: &str,
) -> Option<&'a PermissionRule> {
    rules
        .iter()
        .filter(|rule| rule.tool == tool || rule.tool == "*")
        .filter(|rule| scope_matches(&rule.scope, scope))
        .max_by_key(|rule| {
            (
                usize::from(rule.tool == tool),
                scope_specificity(&rule.scope),
            )
        })
}

fn scope_matches(pattern: &str, scope: &str) -> bool {
    pattern == "*"
        || pattern == scope
        || pattern
            .strip_suffix(" *")
            .is_some_and(|prefix| scope == prefix || scope.starts_with(&format!("{prefix} ")))
}

fn scope_specificity(pattern: &str) -> usize {
    pattern.trim_end_matches(" *").len()
}

fn resolve_path(
    state: &PolicyState,
    requested: &Path,
) -> Result<(PermissionMode, String), PolicyError> {
    let canonical = canonicalize_for_policy(requested)?;
    Ok(match state.closest_root(&canonical) {
        Some(root) if root.decision == TrustDecision::Untrusted => (
            PermissionMode::Never,
            format!(
                "closest {:?} root `{}` is untrusted",
                root.kind,
                root.canonical_path.display()
            ),
        ),
        Some(root) => (
            PermissionMode::Always,
            format!(
                "path is inside {:?} root `{}`",
                root.kind,
                root.canonical_path.display()
            ),
        ),
        None => (
            PermissionMode::Ask,
            format!("path `{}` is outside trusted roots", canonical.display()),
        ),
    })
}

fn canonicalize_for_policy(path: &Path) -> Result<PathBuf, PolicyError> {
    if path.exists() {
        return std::fs::canonicalize(path).map_err(|source| PolicyError::PathResolution {
            path: path.to_path_buf(),
            source,
        });
    }
    // A write may target a file whose parent directories do not exist yet, so
    // the decision is anchored at the deepest ancestor that does and the
    // remaining components are rejoined to it.
    let mut remainder = Vec::new();
    let mut cursor = path;
    loop {
        let parent = cursor
            .parent()
            .ok_or_else(|| PolicyError::Denied("path has no parent".to_owned()))?;
        let file_name = cursor
            .file_name()
            .ok_or_else(|| PolicyError::Denied("path has no file name".to_owned()))?;
        remainder.push(file_name.to_os_string());
        if parent.exists() {
            let canonical_parent =
                std::fs::canonicalize(parent).map_err(|source| PolicyError::PathResolution {
                    path: parent.to_path_buf(),
                    source,
                })?;
            return Ok(remainder
                .iter()
                .rev()
                .fold(canonical_parent, |resolved, component| {
                    resolved.join(component)
                }));
        }
        cursor = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        OwnedToolHandlerFuture, ToolAvailability, ToolExecutionOutput, ToolPresentationKind,
        ToolRegistry, ToolSource, ToolSpec,
    };
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;
    use tokio::sync::Notify;

    struct FixedApproval(ApprovalDecision);

    impl ApprovalAgent for FixedApproval {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async move { Ok(self.0) })
        }
    }

    /// A write into a directory that does not exist yet resolves against the
    /// deepest ancestor that does, and a traversal hidden behind that missing
    /// directory is refused rather than resolved into the trusted root.
    #[test]
    fn a_path_below_a_missing_directory_resolves_without_admitting_a_traversal() {
        let root = tempdir().expect("tempdir");
        let canonical = std::fs::canonicalize(root.path()).expect("canonical root");

        assert_eq!(
            canonicalize_for_policy(&canonical.join("missing/deeper/file.txt"))
                .expect("a missing parent chain resolves"),
            canonical.join("missing/deeper/file.txt")
        );
        assert!(matches!(
            canonicalize_for_policy(&canonical.join("missing/../../escape.txt")),
            Err(PolicyError::Denied(_))
        ));
    }

    /// US-103: a path matching the tool's `sensitive_patterns` asks even where
    /// the configured permission is `always` and the path sits in a trusted
    /// root.
    #[tokio::test]
    async fn a_sensitive_path_asks_even_at_permission_always() {
        let directory = tempdir().expect("tempdir");
        let store = PermissionStore::default();
        store
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        assert_eq!(
            store.tool_settings("read_file").permission,
            PermissionMode::Always,
            "the reference declares read_file as always"
        );

        let ordinary = store
            .resolve(
                "read_file",
                &[PermissionRequirement::Read {
                    path: directory.path().join("notes.txt"),
                }],
            )
            .await
            .expect("resolution");
        assert_eq!(ordinary.mode, PermissionMode::Always);

        let sensitive = store
            .resolve(
                "read_file",
                &[PermissionRequirement::Read {
                    path: directory.path().join(".env"),
                }],
            )
            .await
            .expect("resolution");
        assert_eq!(sensitive.mode, PermissionMode::Ask);
        assert!(
            sensitive.rationale.contains("sensitive pattern"),
            "{}",
            sensitive.rationale
        );
    }

    /// A list match may only tighten what the path itself resolves to. A root
    /// the operator refused stays refused, so a sensitive pattern cannot turn a
    /// denial into a prompt.
    #[tokio::test]
    async fn a_listed_subject_never_reopens_an_untrusted_root() {
        let directory = tempdir().expect("tempdir");
        let store = PermissionStore::default();
        store
            .set_trust(
                directory.path(),
                TrustDecision::Untrusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");

        for name in ["notes.txt", ".env"] {
            let resolution = store
                .resolve(
                    "read_file",
                    &[PermissionRequirement::Read {
                        path: directory.path().join(name),
                    }],
                )
                .await
                .expect("resolution");
            assert_eq!(
                resolution.mode,
                PermissionMode::Never,
                "`{name}` sits in an untrusted root: {}",
                resolution.rationale
            );
        }
    }

    /// US-102: the configured permission is the whole decision for a tool that
    /// produces no granular requirement, and `never` refuses outright.
    #[tokio::test]
    async fn the_configured_permission_decides_a_tool_with_no_requirement() {
        let store = PermissionStore::default();
        let granted = store.resolve("todo", &[]).await.expect("resolution");
        assert_eq!(
            granted.mode,
            PermissionMode::Always,
            "the reference declares todo as always"
        );

        let resolver = ToolConfigResolver::new();
        resolver.update(
            "[todo]\npermission = \"never\"\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let refusing = PermissionStore::default().with_tool_config(resolver);
        let refused = refusing.resolve("todo", &[]).await.expect("resolution");
        assert_eq!(refused.mode, PermissionMode::Never);

        // The session override outranks the configured value, both ways.
        refusing.set_tool_permission("todo", PermissionMode::Always);
        assert_eq!(
            refusing
                .resolve("todo", &[])
                .await
                .expect("resolution")
                .mode,
            PermissionMode::Always
        );
    }

    /// An approval carrying no granular requirement has no pattern to store a
    /// rule under, so what it grants is the tool for the session.
    #[tokio::test]
    async fn an_approval_without_a_requirement_grants_the_tool_for_the_session() {
        // A store nobody handed a resolver still observes its own session
        // overrides: the reference declares `web_fetch` as `ask`, so the grant
        // below is what moves it.
        let store = PermissionStore::default();
        assert_eq!(
            store.tool_settings("web_fetch").permission,
            PermissionMode::Ask
        );
        assert!(store.tool_permission("web_fetch").is_none());

        store
            .authorize(
                "web_fetch",
                Value::Null,
                Vec::new(),
                Vec::new(),
                &FixedApproval(ApprovalDecision::ApproveForSession),
            )
            .await
            .expect("the operator approves");
        assert_eq!(
            store.tool_permission("web_fetch"),
            Some(PermissionMode::Always)
        );
        // The next call resolves without asking again.
        assert_eq!(
            store
                .resolve("web_fetch", &[])
                .await
                .expect("resolution")
                .mode,
            PermissionMode::Always
        );
    }

    #[derive(Default)]
    struct GatedApproval {
        entered: Notify,
        release: Notify,
    }

    impl ApprovalAgent for GatedApproval {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(ApprovalDecision::ApproveOnce)
            })
        }
    }

    #[tokio::test]
    async fn closest_untrusted_root_overrides_trusted_ancestor() {
        let directory = tempdir().expect("tempdir");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        let store = PermissionStore::default();
        store
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Ancestor,
            )
            .await
            .expect("trust ancestor");
        store
            .set_trust(&nested, TrustDecision::Untrusted, TrustRootKind::Workspace)
            .await
            .expect("deny nested");
        let resolution = store
            .resolve(
                "read",
                &[PermissionRequirement::Read {
                    path: nested.join("missing.txt"),
                }],
            )
            .await
            .expect("resolve");
        assert_eq!(resolution.mode, PermissionMode::Never);
    }

    #[tokio::test]
    async fn wildcard_scope_covers_bare_command_and_more_specific_rule_wins() {
        let store = PermissionStore::default();
        store
            .add_rule(PermissionRule {
                tool: "shell".to_owned(),
                scope: "shell git *".to_owned(),
                mode: PermissionMode::Always,
                rationale: "read-only git".to_owned(),
            })
            .await;
        store
            .add_rule(PermissionRule {
                tool: "shell".to_owned(),
                scope: "shell git push".to_owned(),
                mode: PermissionMode::Never,
                rationale: "network mutation".to_owned(),
            })
            .await;
        let git = store
            .resolve(
                "shell",
                &[PermissionRequirement::Shell {
                    command: "git".to_owned(),
                }],
            )
            .await
            .expect("git");
        assert_eq!(git.mode, PermissionMode::Always);
        let push = store
            .resolve(
                "shell",
                &[PermissionRequirement::Shell {
                    command: "git push".to_owned(),
                }],
            )
            .await
            .expect("push");
        assert_eq!(push.mode, PermissionMode::Never);
    }

    #[tokio::test]
    async fn approval_lock_serializes_user_decisions_without_holding_policy_state() {
        let store = PermissionStore::default();
        let directory = tempdir().expect("tempdir");
        store
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let approval = Arc::new(GatedApproval::default());
        let first_store = store.clone();
        let first_approval = approval.clone();
        let first = tokio::spawn(async move {
            first_store
                .authorize(
                    "network",
                    Value::Null,
                    vec![PermissionRequirement::Network {
                        url: Url::parse("https://one.example").expect("url"),
                    }],
                    Vec::new(),
                    first_approval.as_ref(),
                )
                .await
        });
        approval.entered.notified().await;
        let safe = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            store.resolve(
                "read",
                &[PermissionRequirement::Read {
                    path: directory.path().join("safe.txt"),
                }],
            ),
        )
        .await
        .expect("safe policy resolution is not blocked")
        .expect("safe policy resolution");
        assert_eq!(safe.mode, PermissionMode::Always);
        let second_store = store.clone();
        let second_approval = approval.clone();
        let second = tokio::spawn(async move {
            second_store
                .authorize(
                    "network",
                    Value::Null,
                    vec![PermissionRequirement::Network {
                        url: Url::parse("https://two.example").expect("url"),
                    }],
                    Vec::new(),
                    second_approval.as_ref(),
                )
                .await
        });
        tokio::task::yield_now().await;
        approval.release.notify_one();
        first.await.expect("join first").expect("first approval");
        approval.entered.notified().await;
        approval.release.notify_one();
        second.await.expect("join second").expect("second approval");
    }

    #[tokio::test]
    async fn trust_revocation_invalidates_lease_before_side_effect() {
        let directory = tempdir().expect("tempdir");
        let store = PermissionStore::default();
        store
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let lease = store
            .authorize(
                "write",
                Value::Null,
                vec![PermissionRequirement::Write {
                    path: directory.path().join("file.txt"),
                }],
                Vec::new(),
                &FixedApproval(ApprovalDecision::Deny),
            )
            .await
            .expect("trusted root");
        store.revoke_trust(directory.path()).await.expect("revoke");
        assert!(matches!(
            lease.revalidate().await,
            Err(PolicyError::StaleApproval)
        ));
    }

    #[tokio::test]
    async fn revocation_is_atomic_with_guarded_side_effects() {
        let directory = tempdir().expect("tempdir");
        let store = PermissionStore::default();
        store
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let handler: Arc<dyn ToolHandler> = Arc::new({
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            move |_invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let entered = entered.clone();
                let release = release.clone();
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    release.notified().await;
                    Ok(ToolExecutionOutput::text("done"))
                })
            }
        });
        let required_path = directory.path().join("file.txt");
        let guarded = Arc::new(PolicyGuardedTool::new(
            "write",
            store.clone(),
            Arc::new(FixedApproval(ApprovalDecision::Deny)),
            Arc::new(move |_invocation| {
                Ok(vec![PermissionRequirement::Write {
                    path: required_path.clone(),
                }])
            }),
            handler,
        ));
        let registry = ToolRegistry::default();
        registry
            .register(
                ToolSpec {
                    name: "write".to_owned(),
                    description: "write".to_owned(),
                    input_schema: json!({"type": "object", "additionalProperties": false}),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Generic,
                    source: ToolSource::BuiltIn,
                    selection_priority: 0,
                },
                guarded,
            )
            .expect("register");

        let first_registry = registry.clone();
        let first = tokio::spawn(async move {
            first_registry
                .invoke(
                    "write",
                    ToolInvocation {
                        call_id: "first".to_owned(),
                        arguments: json!({}),
                    },
                )
                .await
        });
        entered.notified().await;

        let revoking_store = store.clone();
        let revoking_path = directory.path().to_path_buf();
        let revoke = tokio::spawn(async move { revoking_store.revoke_trust(revoking_path).await });
        tokio::task::yield_now().await;
        assert!(
            !revoke.is_finished(),
            "revocation waits for the active effect"
        );
        let second_registry = registry.clone();
        let second = tokio::spawn(async move {
            second_registry
                .invoke(
                    "write",
                    ToolInvocation {
                        call_id: "second".to_owned(),
                        arguments: json!({}),
                    },
                )
                .await
        });

        release.notify_one();
        first.await.expect("first join").expect("first completes");
        revoke.await.expect("revoke join").expect("revoke");
        assert!(second.await.expect("second join").is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_requires_external_permission() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), workspace.path().join("escape")).expect("symlink");
        let store = PermissionStore::default();
        store
            .set_trust(
                workspace.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let resolution = store
            .resolve(
                "read",
                &[PermissionRequirement::Read {
                    path: workspace.path().join("escape/secret.txt"),
                }],
            )
            .await
            .expect("resolve");
        assert_eq!(resolution.mode, PermissionMode::Ask);
    }
}
