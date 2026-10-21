//! The session's permission store: the stored rules and trust roots, what they
//! make of a call, and the approvals that extend them.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock as StdRwLock};

use serde_json::Value;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};

use super::{
    ApprovalAgent, ApprovalDecision, ApprovalRequest, GrantLifetime, PathGrantScope,
    PermissionContext, PermissionMode, PermissionOutcome, PermissionRequirement, PermissionRule,
    PolicyError, PolicyResolution, TrustDecision, TrustRootKind, approval_path_scope,
    scope_required_permissions,
};
use crate::observability::{self, LogLevel};
use crate::tools::config::{SharedToolConfig, ToolConfigResolver, permission_label};
use crate::tools::{ToolApproval, ToolApprovalSource, ToolApprovalType, ToolVerdict};

/// The rationale a rule stored by an approval carries, which is also what tells
/// the two apart when a session is rebuilt.
pub(super) const SESSION_APPROVAL: &str = "session approval";
const PERMANENT_APPROVAL: &str = "permanent approval";

/// Where a permanent approval writes the patterns it grants.
///
/// Reference `approve_always(save_permanently=True)` extends
/// `tools.<name>.allowlist` through the configuration orchestrator, or, for an
/// approval that carried no pattern, sets `tools.<name>.permission` to
/// `always`: this hook is called with no pattern then. The store lives one
/// layer below the configuration writer, so the session hands it this instead
/// of reaching up.
pub type AllowlistPersistence = Arc<dyn Fn(&str, &[String]) -> Result<(), String> + Send + Sync>;

#[derive(Debug, Clone)]
struct TrustRoot {
    canonical_path: PathBuf,
    decision: TrustDecision,
    #[expect(
        dead_code,
        reason = "names the root in the store's `Debug` output; since trust stopped gating \
                  where a tool reaches, no decision reads it"
    )]
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
pub(super) struct PolicyState {
    revision: u64,
    rules: Vec<PermissionRule>,
    roots: BTreeMap<PathBuf, TrustRoot>,
    /// The session's write boundary: its working directory and every listed
    /// root, canonical. Reference `Workspace.authorized_roots`
    /// (`vibe/core/workspace.py`), which authorizes the working directory
    /// whether or not it is trusted.
    authorized: Vec<PathBuf>,
}

impl PolicyState {
    fn insert_root(&mut self, root: TrustRoot) {
        self.roots.insert(root.canonical_path.clone(), root);
        self.revision = self.revision.saturating_add(1);
    }

    /// Whether a canonical path sits inside the session's write boundary.
    ///
    /// Reference `is_path_within_workdir` asks `Workspace.allows`, which is
    /// the authorized roots alone: trust decides which configuration a session
    /// reads, never where its tools reach. A root a caller trusted without
    /// authorizing it still counts, which is how a store no session opened
    /// positions paths; one declined or revoked stops counting and its paths
    /// are asked about like any other outside the boundary, never refused.
    fn reaches(&self, canonical: &Path) -> bool {
        self.authorized
            .iter()
            .any(|root| canonical.starts_with(root))
            || self
                .closest_root(canonical)
                .is_some_and(|root| root.decision != TrustDecision::Untrusted)
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
    /// The authorized roots again, for a reader that cannot wait on the policy
    /// lock: the shell analysis positions a command's operands synchronously.
    /// Written only alongside [`PolicyState::authorized`].
    workspace_roots: Arc<StdRwLock<Vec<PathBuf>>>,
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
            workspace_roots: Arc::default(),
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

    /// Authorizes the session's working directory and its listed roots.
    ///
    /// Reference `Workspace.for_session(cwd, project_roots)`: the working
    /// directory is resolved and always authorized, whatever the operator
    /// decided about trusting it, and each `--add-dir` root joins it. A
    /// relative listed root is read against the working directory.
    ///
    /// # Errors
    ///
    /// [`PolicyError::Busy`] while another writer holds the store, and a
    /// resolution error for a root that cannot be positioned.
    pub fn try_authorize_workspace(
        &self,
        working_directory: &Path,
        listed_roots: &[PathBuf],
    ) -> Result<(), PolicyError> {
        let mut authorized = vec![canonicalize_for_policy(working_directory)?];
        for root in listed_roots {
            let root = canonicalize_for_policy(&working_directory.join(root))?;
            if !authorized.contains(&root) {
                authorized.push(root);
            }
        }
        let mut state = self.state.try_write().map_err(|_| PolicyError::Busy)?;
        state.authorized.clone_from(&authorized);
        state.revision = state.revision.saturating_add(1);
        if let Ok(mut mirror) = self.workspace_roots.write() {
            *mirror = authorized;
        }
        Ok(())
    }

    /// The roots [`Self::try_authorize_workspace`] authorized, canonical, the
    /// working directory first; empty for a store no session opened.
    #[must_use]
    pub fn workspace_roots(&self) -> Vec<PathBuf> {
        self.workspace_roots
            .read()
            .map(|roots| roots.clone())
            .unwrap_or_default()
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

    /// What the rules make of `context` for `tool`, without asking anyone.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::resolve`] reports.
    pub async fn outcome(
        &self,
        tool: &str,
        context: &PermissionContext,
    ) -> Result<PermissionOutcome, PolicyError> {
        self.resolve(tool, context)
            .await
            .map(PermissionOutcome::from)
    }

    /// Records an approval of `requirements` that was answered elsewhere, as
    /// an approval asked by [`Self::authorize`] records one.
    ///
    /// Reference `UnifiedPermissionResolver.grant`. What `tool` already covers
    /// is kept as it is, and only the rest is recorded, its outside paths
    /// granted under `scope`; a tool that refuses the requirements records
    /// nothing. An approval carrying no requirement grants the tool itself.
    pub async fn grant(
        &self,
        tool: &str,
        requirements: &[PermissionRequirement],
        scope: PathGrantScope,
        lifetime: GrantLifetime<'_>,
    ) {
        let context = PermissionContext::asking(requirements.to_vec());
        let uncovered = match self.outcome(tool, &context).await {
            Ok(PermissionOutcome::Unscoped { .. }) => Vec::new(),
            Ok(PermissionOutcome::Needs { requirements, .. }) => requirements,
            // A tool that refuses the requirements, or already covers them,
            // records nothing.
            Ok(PermissionOutcome::Refused(_) | PermissionOutcome::Cleared { .. }) | Err(_) => {
                return;
            }
        };
        self.record(tool, &uncovered, scope, lifetime).await;
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
        let attribution = approval.attribution(tool);
        let state = self.state.read().await;
        let outcome = resolve_locked(&state, tool, &context, &settings)?.into();
        let revision = state.revision;
        drop(state);
        if let Settled::Lease(lease) =
            self.settle(outcome, revision, tool, &context, &settings, attribution)?
        {
            return Ok(lease);
        }
        let _approval_guard = self.approvals.lock().await;
        let state = self.state.read().await;
        let outcome = resolve_locked(&state, tool, &context, &settings)?.into();
        let revision = state.revision;
        drop(state);
        // Only the uncovered requirements are presented: a rule the operator
        // already granted is not asked again, which is what reference
        // `_resolve_tool_decision` filters before prompting.
        let (uncovered, rationale) =
            match self.settle(outcome, revision, tool, &context, &settings, attribution)? {
                Settled::Lease(lease) => return Ok(lease),
                Settled::Asking {
                    uncovered,
                    rationale,
                } => (uncovered, rationale),
            };
        crate::tools::mark_awaiting_approval(true);
        let decision = approval
            .request(ApprovalRequest {
                tool: tool.to_owned(),
                input,
                requirements: uncovered.clone(),
                rationale,
                call_id: context.call_id.clone(),
            })
            .await;
        crate::tools::mark_awaiting_approval(false);
        let decision = decision?;
        // An agent that approves without asking answers as what it stands
        // for; one that asked answers as the operator.
        let answered = ToolApproval {
            decision: ToolVerdict::Execute,
            approval_type: match attribution {
                ToolApprovalSource::User => ToolApprovalType::Ask,
                _ => ToolApprovalType::Always,
            },
            approval_source: attribution,
        };
        match decision {
            ApprovalDecision::ApproveOnce => {
                let state = self.state.read().await;
                if state.revision != revision {
                    let current = resolve_locked(&state, tool, &context, &settings)?;
                    if current.mode == PermissionMode::Never {
                        return Err(PolicyError::Denied(current.rationale));
                    }
                }
                Ok(self.lease(state.revision, tool, context, settings, answered))
            }
            ApprovalDecision::ApproveForSession(selected)
            | ApprovalDecision::ApprovePermanently(selected) => {
                let lifetime = if matches!(decision, ApprovalDecision::ApprovePermanently(_)) {
                    GrantLifetime::Permanent
                } else {
                    GrantLifetime::Session
                };
                // Reference `approval_grant_permissions`: the server derives
                // the grant from the scope the operator chose, and a scope it
                // did not offer fails the turn once the answer is in.
                let scope = approval_path_scope(&uncovered, selected)
                    .map_err(|refusal| PolicyError::TurnFailed(refusal.to_string()))?;
                let revision = self.record(tool, &uncovered, scope, lifetime).await;
                Ok(self.lease(revision, tool, context, settings, answered))
            }
            // Reference `get_user_cancellation_message(TOOL_SKIPPED)`.
            ApprovalDecision::Deny => Err(PolicyError::Skipped {
                reason: format!("<user_cancellation>{tool}</user_cancellation>"),
                cancelled: true,
            }),
            // Reference `agent_loop_hooks`: a reason carrying the cancellation
            // tag counts as the user cancelling.
            ApprovalDecision::Reject(feedback) => Err(PolicyError::Skipped {
                cancelled: feedback.contains("<user_cancellation>"),
                reason: feedback,
            }),
            ApprovalDecision::CancelTurn => Err(PolicyError::TurnCancelled),
            ApprovalDecision::Fail(message) => Err(PolicyError::TurnFailed(message)),
        }
    }

    /// The lease of a call `outcome` settles without asking, or what it
    /// leaves to ask about.
    fn settle(
        &self,
        outcome: PermissionOutcome,
        revision: u64,
        tool: &str,
        context: &Arc<PermissionContext>,
        settings: &Arc<SharedToolConfig>,
        attribution: ToolApprovalSource,
    ) -> Result<Settled, PolicyError> {
        Ok(match outcome {
            PermissionOutcome::Refused(rationale) => return Err(PolicyError::Denied(rationale)),
            PermissionOutcome::Cleared { covered } => Settled::Lease(self.lease(
                revision,
                tool,
                Arc::clone(context),
                Arc::clone(settings),
                granted_without_asking(covered, attribution),
            )),
            PermissionOutcome::Unscoped { rationale } => Settled::Asking {
                uncovered: Vec::new(),
                rationale,
            },
            PermissionOutcome::Needs {
                requirements,
                rationale,
            } => Settled::Asking {
                uncovered: requirements,
                rationale,
            },
        })
    }

    /// Stores the approval of `uncovered` under `scope`, and returns the
    /// revision the store is at once it is stored.
    ///
    /// An approval carrying no granular requirement has no pattern to store a
    /// rule under, so what was approved is the tool: reference
    /// `approve_always` sets the session permission for exactly that case, and
    /// `set_tool_permission(save_permanently)` writes it for good.
    async fn record(
        &self,
        tool: &str,
        uncovered: &[PermissionRequirement],
        scope: PathGrantScope,
        lifetime: GrantLifetime<'_>,
    ) -> u64 {
        let granted = scope_required_permissions(uncovered, scope);
        if granted.is_empty() {
            self.set_tool_permission(tool, PermissionMode::Always);
        }
        let rationale = match lifetime {
            GrantLifetime::Session => SESSION_APPROVAL,
            GrantLifetime::Permanent | GrantLifetime::PermanentWithin(_) => {
                self.persist_allowlist(tool, &granted, lifetime);
                PERMANENT_APPROVAL
            }
        };
        let mut state = self.state.write().await;
        state.rules.extend(
            granted
                .iter()
                .map(|requirement| requirement.approved_rule(tool, rationale)),
        );
        state.revision = state.revision.saturating_add(1);
        state.revision
    }

    /// Extends `tool`'s configured allowlist with the session patterns of
    /// `requirements` that `lifetime` keeps, or sets its permission to
    /// `always` when there are no requirements at all, when the session was
    /// handed somewhere to write them.
    ///
    /// A store with no persistence hook keeps the approval for the session
    /// only, which is what a bare `PermissionStore` in a test wants; the
    /// app-server hands the real one so a permanent approval survives a
    /// restart. A failed write is reported and never fails the call the
    /// operator just approved.
    fn persist_allowlist(
        &self,
        tool: &str,
        requirements: &[PermissionRequirement],
        lifetime: GrantLifetime<'_>,
    ) {
        let Some(persist) = &self.persist_allowlist else {
            return;
        };
        let (kept, dropped): (Vec<_>, Vec<_>) = match lifetime {
            GrantLifetime::PermanentWithin(scopes) => {
                requirements.iter().partition(|requirement| {
                    !requirement.session_pattern.trim().is_empty()
                        && scopes.contains(&requirement.scope)
                })
            }
            _ => (requirements.iter().collect(), Vec::new()),
        };
        if !dropped.is_empty() {
            observability::log(
                LogLevel::Warning,
                &format!(
                    "the permanent approval for `{tool}` keeps {} of its patterns for this \
                     session only: its allowlist does not read them back",
                    dropped.len()
                ),
            );
            if kept.is_empty() {
                return;
            }
        }
        let mut patterns = kept
            .into_iter()
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
        approval: ToolApproval,
    ) -> PolicyLease {
        PolicyLease {
            store: self.clone(),
            revision,
            tool: tool.to_owned(),
            context,
            settings,
            approval,
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
    /// How the gate settled the call this lease authorizes.
    approval: ToolApproval,
}

impl PolicyLease {
    /// How the gate settled the call, which the settled effect publishes.
    #[must_use]
    pub fn approval(&self) -> ToolApproval {
        self.approval
    }

    pub async fn revalidate(&self) -> Result<(), PolicyError> {
        self.revalidated().await.map(drop)
    }

    /// Revalidates, and holds the policy frozen for as long as the returned
    /// guard lives.
    ///
    /// The guard is held across the side effect on purpose: it is what makes
    /// revocation atomic. `revoke_trust` needs the write lock, so it cannot
    /// land between this revalidation and the effect it authorizes, and any
    /// waiting revocation applies to the next call.
    pub(super) async fn revalidated(
        &self,
    ) -> Result<RwLockReadGuard<'_, PolicyState>, PolicyError> {
        let state = self.store.state.read().await;
        self.revalidate_locked(&state)?;
        Ok(state)
    }

    /// Revalidates against a state the caller already holds a guard on.
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

/// The provenance of a call the gate let through without asking.
///
/// Reference `_should_execute_tool`: an agent that bypasses the gate answers
/// first, then a call whose raised requirements stored rules already cover is
/// the smart approval, and anything else the configured permission.
pub(super) fn granted_without_asking(
    covered: bool,
    attribution: ToolApprovalSource,
) -> ToolApproval {
    let approval_source = match attribution {
        ToolApprovalSource::Bypass => ToolApprovalSource::Bypass,
        _ if covered => ToolApprovalSource::Smart,
        _ => ToolApprovalSource::Config,
    };
    ToolApproval {
        decision: ToolVerdict::Execute,
        approval_type: ToolApprovalType::Always,
        approval_source,
    }
}

/// What [`PermissionStore::authorize`] makes of an outcome before asking.
enum Settled {
    Lease(PolicyLease),
    Asking {
        uncovered: Vec<PermissionRequirement>,
        rationale: String,
    },
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
    // A path that resolves nowhere is positioned outside every root rather than
    // inside one, which is where reference `is_path_within_workdir` puts what it
    // cannot resolve; the guard then fails toward asking rather than refusing.
    let positioned = context
        .paths
        .iter()
        .map(|path| (path, canonicalize_for_policy(path).ok()))
        .collect::<Vec<_>>();
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
    // Every path outside every root carries its own requirement, named by the
    // resolved path: reference `resolve_file_tool_permission` since v2.25.8.
    let mut requirements = context.requirements.clone();
    for (path, canonical) in &positioned {
        if canonical
            .as_ref()
            .is_some_and(|canonical| state.reaches(canonical))
        {
            continue;
        }
        let named = canonical.as_deref().unwrap_or(path.as_path());
        let requirement = PermissionRequirement::outside_path(named);
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
    if exists_physically(path) {
        return crate::platform::canonicalize(path).map_err(|source| PolicyError::PathResolution {
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
        if exists_physically(parent) {
            let canonical_parent = crate::platform::canonicalize(parent).map_err(|source| {
                PolicyError::PathResolution {
                    path: parent.to_path_buf(),
                    source,
                }
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

/// Whether `path` exists when every `..` is taken against the directory it
/// follows, as a POSIX kernel takes it.
///
/// Win32 folds `..` lexically before it looks at the disk, so on Windows
/// `missing\..\..\escape.txt` exists whenever the grandparent does, and a
/// traversal hidden behind a directory that was never created would resolve
/// instead of being refused. Each `..` therefore requires the directory before
/// it to exist.
fn exists_physically(path: &Path) -> bool {
    if !cfg!(windows) {
        return path.exists();
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir && !prefix.is_dir() {
            return false;
        }
        prefix.push(component);
    }
    path.exists()
}
