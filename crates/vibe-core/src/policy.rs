use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock as StdRwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::matching::pattern_matches;
use crate::scratchpad::is_scratchpad_path;
use crate::tools::config::{SharedToolConfig, ToolConfigResolver, permission_label};
use crate::tools::{ToolError, ToolHandler, ToolInvocation, ToolOutputSink};

pub mod arity;

/// The rationale a rule stored by an approval carries, which is also what tells
/// the two apart when a session is rebuilt.
const SESSION_APPROVAL: &str = "session approval";
const PERMANENT_APPROVAL: &str = "permanent approval";

pub type ApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ApprovalDecision, PolicyError>> + Send + 'a>>;

/// Where a permanent approval writes the patterns it grants.
///
/// Reference `approve_always(save_permanently=True)` extends
/// `tools.<name>.allowlist` through the configuration orchestrator. The store
/// lives one layer below the configuration writer, so the session hands it this
/// instead of reaching up.
pub type AllowlistPersistence = Arc<dyn Fn(&str, &[String]) -> Result<(), String> + Send + Sync>;

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

/// The four scopes an approval is granted under.
///
/// Reference `vibe/permissions.py::PermissionScope`. The wire value is the
/// lowercase member name, which is what its `StrEnum` with `auto()` produces,
/// and no fifth value exists: a permission this port cannot express as one of
/// these four is a permission the Python client cannot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    CommandPattern,
    OutsideDirectory,
    FilePattern,
    UrlPattern,
}

impl PermissionScope {
    /// Every scope, in the order the reference declares them.
    ///
    /// The vocabulary's own inventory, which is what the app-server surface
    /// oracle compares against the reference enumeration, the way every other
    /// published vocabulary in this workspace declares an `ALL`.
    pub const ALL: [Self; 4] = [
        Self::CommandPattern,
        Self::OutsideDirectory,
        Self::FilePattern,
        Self::UrlPattern,
    ];
}

/// One thing an operator is asked to approve.
///
/// Reference `vibe/permissions.py::RequiredPermission`, which forbids surplus
/// fields and serializes under a camelCase alias. The two patterns are what
/// separate the call from the grant: `invocation_pattern` names this
/// invocation, `session_pattern` names everything a session-wide approval would
/// also cover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRequirement {
    pub scope: PermissionScope,
    pub invocation_pattern: String,
    pub session_pattern: String,
    pub label: String,
}

impl PermissionRequirement {
    /// A command segment, under the session pattern its arity derives.
    ///
    /// Reference `BashTool._build_required_permissions`, which labels the
    /// requirement with the session pattern rather than with the segment.
    #[must_use]
    pub fn command(segment: &str) -> Self {
        let tokens = segment.split_whitespace().collect::<Vec<_>>();
        let session_pattern = arity::build_session_pattern(&tokens);
        Self {
            scope: PermissionScope::CommandPattern,
            invocation_pattern: segment.to_owned(),
            session_pattern: session_pattern.clone(),
            label: session_pattern,
        }
    }

    /// A command segment whose grant may not widen past itself.
    ///
    /// Reference `_build_required_permissions` takes this branch for a
    /// sensitive segment and for a `find` carrying an execution predicate: both
    /// carry the segment as its own session pattern, so approving `sudo apt
    /// update` never approves `sudo rm`.
    #[must_use]
    pub fn exact_command(segment: &str) -> Self {
        Self {
            scope: PermissionScope::CommandPattern,
            invocation_pattern: segment.to_owned(),
            session_pattern: segment.to_owned(),
            label: segment.to_owned(),
        }
    }

    /// A directory the call reaches outside every root.
    ///
    /// Reference `_build_outside_directory_permission` and the shared file-tool
    /// chain, which both carry the glob as both patterns.
    #[must_use]
    pub fn outside_directory(glob: &str) -> Self {
        Self {
            scope: PermissionScope::OutsideDirectory,
            invocation_pattern: glob.to_owned(),
            session_pattern: glob.to_owned(),
            label: format!("outside workdir ({glob})"),
        }
    }

    /// A file whose name matched the tool's `sensitive_patterns`.
    ///
    /// Reference `resolve_file_tool_permission` names the file itself and grants
    /// the whole class for the session, which is why the session pattern is `*`.
    #[must_use]
    pub fn sensitive_file(file_name: &str, tool: &str) -> Self {
        Self {
            scope: PermissionScope::FilePattern,
            invocation_pattern: file_name.to_owned(),
            session_pattern: "*".to_owned(),
            label: format!("accessing sensitive files ({tool})"),
        }
    }

    /// A host a fetch reaches. Reference `WebFetchTool.resolve_permission`.
    #[must_use]
    pub fn url_domain(domain: &str) -> Self {
        Self {
            scope: PermissionScope::UrlPattern,
            invocation_pattern: domain.to_owned(),
            session_pattern: domain.to_owned(),
            label: format!("fetching from {domain}"),
        }
    }

    /// The rule an approval for the session stores this requirement under.
    #[must_use]
    pub fn approved_rule(&self, tool: &str, rationale: &str) -> PermissionRule {
        PermissionRule {
            tool: tool.to_owned(),
            scope: Some(self.scope),
            pattern: self.session_pattern.clone(),
            mode: PermissionMode::Always,
            rationale: rationale.to_owned(),
        }
    }
}

/// What a tool's own permission resolution answers.
///
/// Reference `PermissionContext`, plus the paths this port checks against its
/// trust roots. The reference reads its project roots inside the file-tool
/// chain; this port keeps them in the store so a revocation can invalidate a
/// lease that was granted a moment earlier, which is why the paths travel with
/// the context rather than being resolved once by the tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionContext {
    /// What the tool decided on its own, or [`None`] to fall back to the
    /// configured permission, which is the reference's `resolve_permission`
    /// returning `None`.
    pub permission: Option<PermissionMode>,
    pub requirements: Vec<PermissionRequirement>,
    /// Paths the call touches, positioned against the trust roots.
    pub paths: Vec<PathBuf>,
    pub reason: Option<String>,
}

impl PermissionContext {
    /// A context deciding nothing, which defers to the configured permission.
    #[must_use]
    pub fn deferred() -> Self {
        Self::default()
    }

    /// A context settling the call outright.
    #[must_use]
    pub fn settled(permission: PermissionMode) -> Self {
        Self {
            permission: Some(permission),
            ..Self::default()
        }
    }

    /// A context asking for `requirements`.
    #[must_use]
    pub fn asking(requirements: Vec<PermissionRequirement>) -> Self {
        Self {
            permission: Some(PermissionMode::Ask),
            requirements,
            ..Self::default()
        }
    }

    /// The same context carrying `reason`.
    #[must_use]
    pub fn because(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// The same context checking `paths` against the trust roots.
    #[must_use]
    pub fn over_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.paths = paths;
        self
    }
}

/// A stored permission decision, matched against a requirement's invocation
/// pattern.
///
/// Reference `ApprovedRule` carries a tool, a scope and a session pattern.
/// This port adds a mode and a rationale, which is what lets an agent profile
/// install a refusal through the same table rather than through a second one; a
/// rule with no scope answers for every scope, which is the profile's `*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub tool: String,
    #[serde(default)]
    pub scope: Option<PermissionScope>,
    pub pattern: String,
    pub mode: PermissionMode,
    pub rationale: String,
}

impl PermissionRule {
    /// Whether this rule answers for `requirement` on `tool`.
    ///
    /// Reference `PermissionStore.covers`: the tool and the scope have to be
    /// the rule's own, and the requirement's invocation pattern has to match the
    /// rule's session pattern under [`wildcard_match`].
    #[must_use]
    pub fn covers(&self, tool: &str, requirement: &PermissionRequirement) -> bool {
        (self.tool == tool || self.tool == "*")
            && self.scope.is_none_or(|scope| scope == requirement.scope)
            && wildcard_match(&requirement.invocation_pattern, &self.pattern)
    }

    /// How much of a requirement this rule pins down, used to prefer the more
    /// specific of two rules answering for the same requirement.
    fn specificity(&self) -> (usize, usize) {
        (
            usize::from(self.scope.is_some()),
            self.pattern.trim_end_matches(" *").len(),
        )
    }
}

/// Whether `text` matches `pattern`, with the trailing arguments optional.
///
/// Reference `wildcard_match`: a pattern ending in ` *` also matches the text
/// that is the pattern without that suffix, so an approval stored for
/// `git status *` covers a later bare `git status`.
#[must_use]
pub fn wildcard_match(text: &str, pattern: &str) -> bool {
    if pattern_matches(pattern, text) {
        return true;
    }
    pattern
        .strip_suffix(" *")
        .is_some_and(|prefix| pattern_matches(prefix, text))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyResolution {
    pub mode: PermissionMode,
    pub rationale: String,
    pub matched_rule: Option<PermissionRule>,
    /// The requirements no stored rule covers, which is exactly what a prompt
    /// lists. Reference `_resolve_tool_decision` filters the covered ones out
    /// before asking.
    pub required_permissions: Vec<PermissionRequirement>,
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
    /// The session's own scratchpad, which every file tool grants outright.
    /// [`None`] for a session whose scratchpad could not be opened, where the
    /// chain simply never takes that branch.
    pub scratchpad: Option<PathBuf>,
}

impl ToolGuard {
    #[must_use]
    pub fn new(policy: PermissionStore, approval: Arc<dyn ApprovalAgent>) -> Self {
        let config = policy.tool_config();
        Self {
            policy,
            approval,
            config,
            scratchpad: None,
        }
    }

    /// The same guard whose file tools may always reach `scratchpad`.
    #[must_use]
    pub fn with_scratchpad(mut self, scratchpad: Option<PathBuf>) -> Self {
        self.scratchpad = scratchpad;
        self
    }
}

pub type RequirementResolver =
    dyn Fn(&ToolInvocation) -> Result<PermissionContext, ToolError> + Send + Sync;

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
            let context = requirements(&invocation)?;
            let lease = store
                .authorize(
                    &name,
                    invocation.arguments.clone(),
                    context,
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

#[derive(Clone)]
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
    /// Where a permanent approval writes, or [`None`] when this session has
    /// nowhere to write and every approval dies with it.
    persist_allowlist: Option<AllowlistPersistence>,
    /// What went wrong while persisting an approval, read by the session that
    /// reports it. A failed write never fails the call the operator approved,
    /// so the failure has to be readable somewhere.
    diagnostics: Arc<StdRwLock<Vec<String>>>,
}

impl std::fmt::Debug for PermissionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PermissionStore")
            .field("state", &self.state)
            .field("tool_config", &self.tool_config)
            .field("persist_allowlist", &self.persist_allowlist.is_some())
            .finish_non_exhaustive()
    }
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
            persist_allowlist: None,
            diagnostics: Arc::default(),
        }
        .with_tool_config(ToolConfigResolver::new())
    }
}

impl PermissionStore {
    /// The same store writing a permanent approval through `persist`.
    #[must_use]
    pub fn with_allowlist_persistence(mut self, persist: AllowlistPersistence) -> Self {
        self.persist_allowlist = Some(persist);
        self
    }

    /// What went wrong while persisting an approval, oldest first.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .read()
            .map(|stored| stored.clone())
            .unwrap_or_default()
    }

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
        context: &PermissionContext,
    ) -> Result<PolicyResolution, PolicyError> {
        let settings = self.tool_settings(tool);
        let state = self.state.read().await;
        resolve_locked(&state, tool, context, &settings)
    }

    pub async fn authorize(
        &self,
        tool: &str,
        input: Value,
        context: PermissionContext,
        approval: &dyn ApprovalAgent,
    ) -> Result<PolicyLease, PolicyError> {
        // The settings are read once per call rather than held: an operator who
        // raises a budget or a permission between two turns is obeyed on the
        // next one without the surface being registered again.
        let settings = Arc::new(self.tool_settings(tool));
        let context = Arc::new(context);
        let state = self.state.read().await;
        let resolution = resolve_locked(&state, tool, &context, &settings)?;
        match resolution.mode {
            PermissionMode::Always => {
                let revision = state.revision;
                drop(state);
                Ok(self.lease(revision, tool, context, settings))
            }
            PermissionMode::Never => Err(PolicyError::Denied(resolution.rationale)),
            PermissionMode::Ask => {
                drop(state);
                let _approval_guard = self.approvals.lock().await;
                let state = self.state.read().await;
                let resolution = resolve_locked(&state, tool, &context, &settings)?;
                match resolution.mode {
                    PermissionMode::Always => {
                        return Ok(self.lease(state.revision, tool, context, settings));
                    }
                    PermissionMode::Never => {
                        return Err(PolicyError::Denied(resolution.rationale));
                    }
                    PermissionMode::Ask => {}
                }
                let revision = state.revision;
                drop(state);
                // Only the uncovered requirements are presented: a rule the
                // operator already granted is not asked again, which is what
                // reference `_resolve_tool_decision` filters before prompting.
                let uncovered = resolution.required_permissions;
                let decision = approval
                    .request(ApprovalRequest {
                        tool: tool.to_owned(),
                        input,
                        requirements: uncovered.clone(),
                        rationale: resolution.rationale,
                    })
                    .await?;
                match decision {
                    ApprovalDecision::ApproveOnce => {
                        let state = self.state.read().await;
                        if state.revision != revision {
                            let current = resolve_locked(&state, tool, &context, &settings)?;
                            if current.mode == PermissionMode::Never {
                                return Err(PolicyError::Denied(current.rationale));
                            }
                        }
                        Ok(self.lease(state.revision, tool, context, settings))
                    }
                    ApprovalDecision::ApproveForSession | ApprovalDecision::ApprovePermanently => {
                        let permanent = decision == ApprovalDecision::ApprovePermanently;
                        let persistence = if permanent {
                            PERMANENT_APPROVAL
                        } else {
                            SESSION_APPROVAL
                        };
                        // An approval carrying no granular requirement has no
                        // pattern to store a rule under, so what was approved is
                        // the tool: reference `approve_always` sets the session
                        // permission for exactly that case.
                        if uncovered.is_empty() {
                            self.set_tool_permission(tool, PermissionMode::Always);
                        } else if permanent {
                            // Reference `approve_always(save_permanently=True)`
                            // extends the tool's configured allowlist, which is
                            // the only half of an approval that outlives the
                            // session.
                            self.persist_allowlist(tool, &uncovered);
                        }
                        let mut state = self.state.write().await;
                        state.rules.extend(
                            uncovered
                                .iter()
                                .map(|requirement| requirement.approved_rule(tool, persistence)),
                        );
                        state.revision = state.revision.saturating_add(1);
                        Ok(self.lease(state.revision, tool, context, settings))
                    }
                    ApprovalDecision::Deny => {
                        Err(PolicyError::Denied("approval denied".to_owned()))
                    }
                    ApprovalDecision::CancelTurn => Err(PolicyError::TurnCancelled),
                }
            }
        }
    }

    /// Extends `tool`'s configured allowlist with the session patterns of
    /// `requirements`, when the session was handed somewhere to write them.
    ///
    /// A store with no persistence hook keeps the approval for the session
    /// only, which is what a bare `PermissionStore` in a test wants; the
    /// app-server hands the real one so a permanent approval survives a
    /// restart. A failed write is reported and never fails the call the
    /// operator just approved.
    fn persist_allowlist(&self, tool: &str, requirements: &[PermissionRequirement]) {
        let Some(persist) = &self.persist_allowlist else {
            return;
        };
        let mut patterns = requirements
            .iter()
            .map(|requirement| requirement.session_pattern.clone())
            .collect::<Vec<_>>();
        patterns.sort();
        patterns.dedup();
        if let Err(error) = persist(tool, &patterns)
            && let Ok(mut diagnostics) = self.diagnostics.write()
        {
            diagnostics.push(format!(
                "the permanent approval for `{tool}` was kept for this session only: {error}"
            ));
        }
    }

    fn lease(
        &self,
        revision: u64,
        tool: &str,
        context: Arc<PermissionContext>,
        settings: Arc<SharedToolConfig>,
    ) -> PolicyLease {
        PolicyLease {
            store: self.clone(),
            revision,
            tool: tool.to_owned(),
            context,
            settings,
        }
    }
}

#[derive(Clone)]
pub struct PolicyLease {
    store: PermissionStore,
    revision: u64,
    tool: String,
    /// What was authorized, carried so revalidation asks the same question
    /// rather than a newer one.
    context: Arc<PermissionContext>,
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
        let resolution = resolve_locked(state, &self.tool, &self.context, &self.settings)?;
        if resolution.mode == PermissionMode::Always {
            Ok(())
        } else {
            Err(PolicyError::StaleApproval)
        }
    }
}

/// What every refusal message starts with.
///
/// A tool executor reports its failures as strings, so this prefix is the only
/// thing that tells an operator's refusal from a tool that failed on its own
/// once the error has crossed that boundary. `the_denial_prefix_is_what_a_
/// denial_reports` holds it to [`PolicyError::Denied`]'s own rendering.
pub const DENIAL_PREFIX: &str = "permission denied: ";

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

/// Resolves what a call may do, from the tool's own context and the stored
/// rules.
///
/// Reference `AgentLoop._resolve_tool_decision`: a context answering `ALWAYS`
/// or `NEVER` settles the call, and a context asking is filtered against the
/// stored rules so an approval already granted is not asked again. The trust
/// roots are this port's own step, and they run before the tool's own answer so
/// a root the operator revoked closes a call the tool would have granted.
fn resolve_locked(
    state: &PolicyState,
    tool: &str,
    context: &PermissionContext,
    settings: &SharedToolConfig,
) -> Result<PolicyResolution, PolicyError> {
    let refusal = |rationale: String| PolicyResolution {
        mode: PermissionMode::Never,
        rationale,
        matched_rule: None,
        required_permissions: Vec::new(),
    };
    // A tool configured to `never` is refused outright, whatever a rule or a
    // trusted root would otherwise say: reference `get_tool_config` composes
    // that permission from the operator's table and the session override, and
    // no approval reopens it. A path leaving the working directory under that
    // permission is refused here rather than turned into a requirement, which
    // is the reference's early `NEVER` in the file-tool chain.
    if settings.permission == PermissionMode::Never {
        return Ok(refusal(format!("`{tool}` is configured to never run")));
    }
    // A path the operator refused closes the call before the tool's own answer
    // is read, so neither an allowlist match nor a stored approval reopens it.
    // A path that resolves nowhere is positioned outside every root rather than
    // inside one, which is where reference `is_path_within_workdir` puts what it
    // cannot resolve; the guard then fails toward asking rather than refusing.
    let positioned = context
        .paths
        .iter()
        .map(|path| (path, canonicalize_for_policy(path).ok()))
        .collect::<Vec<_>>();
    for (_, canonical) in &positioned {
        if let Some(root) = canonical
            .as_ref()
            .and_then(|canonical| state.closest_root(canonical))
            && root.decision == TrustDecision::Untrusted
        {
            return Ok(refusal(format!(
                "closest {:?} root `{}` is untrusted",
                root.kind,
                root.canonical_path.display()
            )));
        }
    }
    if context.permission == Some(PermissionMode::Never) {
        return Ok(refusal(
            context
                .reason
                .clone()
                .unwrap_or_else(|| format!("`{tool}` refused this call")),
        ));
    }
    // A tool that already granted the call answers for its own paths: the
    // reference returns `ALWAYS` on an allowlist match without ever reaching the
    // working-directory check, so an allowlisted path outside it stays granted.
    if context.permission == Some(PermissionMode::Always) && context.requirements.is_empty() {
        return Ok(PolicyResolution {
            mode: PermissionMode::Always,
            rationale: context
                .reason
                .clone()
                .unwrap_or_else(|| format!("`{tool}` granted this call")),
            matched_rule: None,
            required_permissions: Vec::new(),
        });
    }
    // Every path outside every root carries its own requirement, deduplicated
    // by the directory it names: reference `_build_outside_directory_permission`
    // over a set of directories rather than over one per file.
    let mut requirements = context.requirements.clone();
    for (path, canonical) in &positioned {
        if canonical
            .as_ref()
            .is_some_and(|canonical| state.closest_root(canonical).is_some())
        {
            continue;
        }
        let named = canonical.as_deref().unwrap_or(path.as_path());
        let requirement = PermissionRequirement::outside_directory(&outside_glob(named));
        if !requirements.contains(&requirement) {
            requirements.push(requirement);
        }
    }
    let had_requirements = !requirements.is_empty();
    let mut matched_rule = None;
    let mut uncovered = Vec::new();
    let mut refused = None;
    for requirement in requirements {
        let rule = best_rule(&state.rules, tool, &requirement);
        if let Some(rule) = rule {
            matched_rule = Some(rule.clone());
            match rule.mode {
                PermissionMode::Never => {
                    refused.get_or_insert_with(|| rule.rationale.clone());
                    continue;
                }
                PermissionMode::Always => continue,
                PermissionMode::Ask => {}
            }
        }
        uncovered.push(requirement);
    }
    if let Some(rationale) = refused {
        return Ok(refusal(rationale));
    }
    // Reference `_resolve_tool_decision`: a call that raised requirements and
    // has none left uncovered runs without asking, and one that raised none is
    // decided by the permission alone.
    let (mode, rationale) = match (had_requirements, uncovered.is_empty()) {
        (true, true) => (
            PermissionMode::Always,
            format!("every permission `{tool}` requires is already approved"),
        ),
        (true, false) => (
            PermissionMode::Ask,
            context.reason.clone().unwrap_or_else(|| {
                let labels = uncovered
                    .iter()
                    .map(|requirement| requirement.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("`{tool}` needs approval for {labels}")
            }),
        ),
        (false, _) => {
            let mode = context.permission.unwrap_or(settings.permission);
            (
                mode,
                context.reason.clone().unwrap_or_else(|| {
                    format!(
                        "`{tool}` declared no permission requirement and is configured to {}",
                        permission_label(mode)
                    )
                }),
            )
        }
    };
    Ok(PolicyResolution {
        mode,
        rationale,
        matched_rule,
        required_permissions: uncovered,
    })
}

/// Resolves what a file tool may do with `path`, before the trust roots.
///
/// Reference `resolve_file_tool_permission`, in its order: the scratchpad is
/// the runtime's own capability and is granted first; the denylist refuses and
/// the allowlist grants, both against the resolved absolute path and both
/// before anything else is read; a sensitive match then raises a `file_pattern`
/// requirement even where the configured permission is `always`. The
/// working-directory boundary is the one step this port answers elsewhere: the
/// roots live in the store, so the path travels on the context and
/// [`resolve_locked`] positions it.
#[must_use]
pub fn resolve_file_tool_permission(
    path: &Path,
    tool: &str,
    settings: &SharedToolConfig,
    scratchpad: Option<&Path>,
) -> PermissionContext {
    if is_scratchpad_path(path, scratchpad) {
        return PermissionContext::settled(PermissionMode::Always)
            .because(format!("`{tool}` may always reach its own scratchpad"));
    }
    // The lists are matched against the resolved absolute path, which is what
    // makes `**/.env` mean the same thing here and upstream. A path that cannot
    // be resolved is matched as written rather than skipped: refusing to look is
    // not a reason to grant.
    let resolved = canonicalize_for_policy(path).unwrap_or_else(|_| path.to_path_buf());
    let subject = resolved.display().to_string();
    if let Some(pattern) = matched_pattern(&settings.denylist, &subject) {
        return PermissionContext::settled(PermissionMode::Never).because(format!(
            "`{subject}` matches the `{tool}` denylist entry `{pattern}`"
        ));
    }
    if let Some(pattern) = matched_pattern(&settings.allowlist, &subject) {
        return PermissionContext::settled(PermissionMode::Always).because(format!(
            "`{subject}` matches the `{tool}` allowlist entry `{pattern}`"
        ));
    }
    let mut context = PermissionContext::deferred().over_paths(vec![path.to_path_buf()]);
    if let Some(pattern) = matched_path_pattern(&settings.sensitive_patterns, &subject) {
        let file_name = resolved.file_name().map_or_else(
            || subject.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        context.permission = Some(PermissionMode::Ask);
        context.reason = Some(format!(
            "`{subject}` matches the `{tool}` sensitive pattern `{pattern}`"
        ));
        context
            .requirements
            .push(PermissionRequirement::sensitive_file(&file_name, tool));
    }
    context
}

/// Resolves what the `task` tool may delegate to `agent`.
///
/// Reference `TaskTool.resolve_permission`, in its order: the denylist refuses
/// first, so a name matching both lists is refused, the allowlist then grants,
/// and a name in neither list settles nothing and leaves the configured
/// permission to decide. Both lists are matched by the same glob rules the file
/// lists use rather than by equality, so `review-*` names a family.
#[must_use]
pub fn resolve_task_tool_permission(agent: &str, settings: &SharedToolConfig) -> PermissionContext {
    if let Some(pattern) = matched_pattern(&settings.denylist, agent) {
        return PermissionContext::settled(PermissionMode::Never).because(format!(
            "subagent `{agent}` matches the `task` denylist entry `{pattern}`"
        ));
    }
    if let Some(pattern) = matched_pattern(&settings.allowlist, agent) {
        return PermissionContext::settled(PermissionMode::Always).because(format!(
            "subagent `{agent}` matches the `task` allowlist entry `{pattern}`"
        ));
    }
    PermissionContext::deferred()
}

/// The first entry of `patterns` matching `subject`, in the order the operator
/// wrote them.
fn matched_pattern<'a>(patterns: &'a [String], subject: &str) -> Option<&'a String> {
    patterns
        .iter()
        .find(|pattern| pattern_matches(pattern, subject))
}

/// The first entry of `patterns` naming `subject` as a path, in the order the
/// operator wrote them.
fn matched_path_pattern<'a>(patterns: &'a [String], subject: &str) -> Option<&'a String> {
    patterns
        .iter()
        .find(|pattern| path_pattern_matches(pattern, subject))
}

/// Whether `pattern` names `subject` the way a sensitive pattern names a file.
///
/// Reference `resolve_file_tool_permission` runs `sensitive_patterns` through
/// `PurePath.match` while the allowlist and the denylist beside it stay on
/// `fnmatch`, so the two matchers answer differently and both are kept. This
/// one compares component by component and anchors on the right: `.env` names a
/// file called `.env` at any depth, `/etc/*` names only a direct child of
/// `/etc`, and a `*` never crosses a separator the way `fnmatch` lets it.
///
/// A pattern naming no component matches nothing. Upstream raises `ValueError`
/// there; failing to read a pattern is a reason to raise no requirement from
/// it, never a reason to drop the ones the other patterns raise.
fn path_pattern_matches(pattern: &str, subject: &str) -> bool {
    let expected = path_components(pattern);
    let Some(width) = (!expected.is_empty()).then_some(expected.len()) else {
        return false;
    };
    let found = path_components(subject);
    if pattern.starts_with('/') {
        // An anchored pattern compares against the whole path, root included.
        return found.len() == width && components_match(&expected, &found);
    }
    let Some(tail) = found
        .len()
        .checked_sub(width)
        .and_then(|start| found.get(start..))
    else {
        return false;
    };
    components_match(&expected, tail)
}

/// The components `PurePath` compares by, keeping the root as an empty leading
/// component so an absolute path and an absolute pattern line up.
///
/// Only `/` separates: the reference dispatches on the host flavor, and the
/// oracle measures the POSIX one.
fn path_components(value: &str) -> Vec<&str> {
    let mut components = Vec::new();
    if value.starts_with('/') {
        components.push("");
    }
    components.extend(
        value
            .split('/')
            .filter(|component| !component.is_empty() && *component != "."),
    );
    components
}

/// Whether every pattern component names the component facing it.
fn components_match(expected: &[&str], found: &[&str]) -> bool {
    expected
        .iter()
        .zip(found)
        .all(|(pattern, component)| component_matches(pattern, component))
}

/// Whether one pattern component names one path component.
fn component_matches(pattern: &str, component: &str) -> bool {
    match pattern {
        // `**` stands for any single component, the root included, which is
        // what lets the shipped `**/.env` name a file sitting at the root.
        "**" => true,
        // A lone `*` needs something to name, so it never stands in for the
        // root the way `**` does.
        "*" => !component.is_empty(),
        _ => pattern_matches(pattern, component),
    }
}

/// The glob an outside path is named by: the parent for a file, the directory
/// itself for a directory, joined with `*`.
///
/// Reference `resolve_file_tool_permission` names `parent / "*"`, and
/// `_collect_outside_dirs` names the directory a command operand resolves to.
fn outside_glob(canonical: &Path) -> String {
    let directory = if canonical.is_dir() {
        canonical
    } else {
        canonical.parent().unwrap_or(canonical)
    };
    directory.join("*").display().to_string()
}

/// The most specific rule answering for `requirement`, or [`None`] when none
/// does.
///
/// A rule naming the tool outranks the `*` one, and a rule naming a scope
/// outranks one answering for every scope, so a profile's blanket refusal never
/// hides the narrower grant written next to it.
fn best_rule<'a>(
    rules: &'a [PermissionRule],
    tool: &str,
    requirement: &PermissionRequirement,
) -> Option<&'a PermissionRule> {
    rules
        .iter()
        .filter(|rule| rule.covers(tool, requirement))
        .max_by_key(|rule| (usize::from(rule.tool == tool), rule.specificity()))
}

pub(crate) fn canonicalize_for_policy(path: &Path) -> Result<PathBuf, PolicyError> {
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
mod permission_parity_tests;
#[cfg(test)]
mod policy_tests;
