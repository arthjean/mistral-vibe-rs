use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use url::Url;

use crate::tools::{
    OwnedToolHandlerFuture, ToolError, ToolHandler, ToolInvocation, ToolOutputSink,
};

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

    fn path(&self) -> Option<&Path> {
        match self {
            Self::Read { path } | Self::Write { path } => Some(path),
            _ => None,
        }
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
        let future: OwnedToolHandlerFuture = Box::pin(async move {
            let requirements = requirements(&invocation)?;
            let lease = store
                .authorize(&name, requirements, approval.as_ref())
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let state = store.state.read().await;
            lease
                .revalidate_locked(&state)
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            inner.invoke(&invocation, output).await
        });
        future
    }
}

#[derive(Debug, Clone)]
struct TrustRoot {
    canonical_path: PathBuf,
    decision: TrustDecision,
    kind: TrustRootKind,
}

#[derive(Debug, Default)]
struct PolicyState {
    revision: u64,
    rules: Vec<PermissionRule>,
    roots: BTreeMap<PathBuf, TrustRoot>,
}

#[derive(Clone, Default, Debug)]
pub struct PermissionStore {
    state: Arc<RwLock<PolicyState>>,
    approvals: Arc<Mutex<()>>,
}

impl PermissionStore {
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
        let canonical_path =
            std::fs::canonicalize(path.as_ref()).map_err(|source| PolicyError::PathResolution {
                path: path.as_ref().to_path_buf(),
                source,
            })?;
        if canonical_path.parent().is_none() {
            return Err(PolicyError::UnsafeTrustRoot(canonical_path));
        }
        let mut state = self.state.write().await;
        state.roots.insert(
            canonical_path.clone(),
            TrustRoot {
                canonical_path,
                decision,
                kind,
            },
        );
        state.revision = state.revision.saturating_add(1);
        Ok(())
    }

    pub fn try_set_trust(
        &self,
        path: impl AsRef<Path>,
        decision: TrustDecision,
        kind: TrustRootKind,
    ) -> Result<(), PolicyError> {
        let canonical_path =
            std::fs::canonicalize(path.as_ref()).map_err(|source| PolicyError::PathResolution {
                path: path.as_ref().to_path_buf(),
                source,
            })?;
        if canonical_path.parent().is_none() {
            return Err(PolicyError::UnsafeTrustRoot(canonical_path));
        }
        let mut state = self.state.try_write().map_err(|_| PolicyError::Busy)?;
        state.roots.insert(
            canonical_path.clone(),
            TrustRoot {
                canonical_path,
                decision,
                kind,
            },
        );
        state.revision = state.revision.saturating_add(1);
        Ok(())
    }

    pub fn try_trust_decision(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Option<TrustDecision>, PolicyError> {
        let canonical_path =
            std::fs::canonicalize(path.as_ref()).map_err(|source| PolicyError::PathResolution {
                path: path.as_ref().to_path_buf(),
                source,
            })?;
        let state = self.state.try_read().map_err(|_| PolicyError::Busy)?;
        Ok(state
            .roots
            .values()
            .filter(|root| canonical_path.starts_with(&root.canonical_path))
            .max_by_key(|root| root.canonical_path.components().count())
            .map(|root| root.decision))
    }

    pub async fn revoke_trust(&self, path: impl AsRef<Path>) -> Result<(), PolicyError> {
        let canonical_path =
            std::fs::canonicalize(path.as_ref()).map_err(|source| PolicyError::PathResolution {
                path: path.as_ref().to_path_buf(),
                source,
            })?;
        let mut state = self.state.write().await;
        state.roots.insert(
            canonical_path.clone(),
            TrustRoot {
                canonical_path,
                decision: TrustDecision::Untrusted,
                kind: TrustRootKind::Workspace,
            },
        );
        state.revision = state.revision.saturating_add(1);
        Ok(())
    }

    pub async fn resolve(
        &self,
        tool: &str,
        requirements: &[PermissionRequirement],
    ) -> Result<PolicyResolution, PolicyError> {
        let state = self.state.read().await;
        resolve_locked(&state, tool, requirements)
    }

    pub async fn authorize(
        &self,
        tool: &str,
        requirements: Vec<PermissionRequirement>,
        approval: &dyn ApprovalAgent,
    ) -> Result<PolicyLease, PolicyError> {
        let state = self.state.read().await;
        let resolution = resolve_locked(&state, tool, &requirements)?;
        match resolution.mode {
            PermissionMode::Always => {
                let revision = state.revision;
                drop(state);
                Ok(self.lease(revision, tool, requirements))
            }
            PermissionMode::Never => Err(PolicyError::Denied(resolution.rationale)),
            PermissionMode::Ask => {
                drop(state);
                let _approval_guard = self.approvals.lock().await;
                let state = self.state.read().await;
                let resolution = resolve_locked(&state, tool, &requirements)?;
                match resolution.mode {
                    PermissionMode::Always => {
                        return Ok(self.lease(state.revision, tool, requirements));
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
                        requirements: requirements.clone(),
                        rationale: resolution.rationale,
                    })
                    .await?;
                match decision {
                    ApprovalDecision::ApproveOnce => {
                        let state = self.state.read().await;
                        if state.revision != revision {
                            let current = resolve_locked(&state, tool, &requirements)?;
                            if current.mode == PermissionMode::Never {
                                return Err(PolicyError::Denied(current.rationale));
                            }
                        }
                        Ok(self.lease(state.revision, tool, requirements))
                    }
                    ApprovalDecision::ApproveForSession | ApprovalDecision::ApprovePermanently => {
                        let persistence = if decision == ApprovalDecision::ApprovePermanently {
                            "permanent approval"
                        } else {
                            "session approval"
                        };
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
                        Ok(self.lease(state.revision, tool, requirements))
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
    ) -> PolicyLease {
        PolicyLease {
            store: self.clone(),
            revision,
            tool: tool.to_owned(),
            requirements,
        }
    }
}

#[derive(Clone)]
pub struct PolicyLease {
    store: PermissionStore,
    revision: u64,
    tool: String,
    requirements: Vec<PermissionRequirement>,
}

impl PolicyLease {
    pub async fn revalidate(&self) -> Result<(), PolicyError> {
        let state = self.store.state.read().await;
        self.revalidate_locked(&state).await
    }

    async fn revalidate_locked(&self, state: &PolicyState) -> Result<(), PolicyError> {
        if state.revision == self.revision {
            return Ok(());
        }
        let resolution = resolve_locked(state, &self.tool, &self.requirements)?;
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
) -> Result<PolicyResolution, PolicyError> {
    let mut effective = PermissionMode::Always;
    let mut rationales = Vec::new();
    let mut matched_rule = None;
    let mut required_permissions = Vec::new();
    for requirement in requirements {
        let scope = requirement.scope();
        required_permissions.push(scope.clone());
        let rule = best_rule(&state.rules, tool, &scope);
        let (mode, rationale) = if let Some(rule) = rule {
            matched_rule = Some(rule.clone());
            (rule.mode, rule.rationale.clone())
        } else if let Some(path) = requirement.path() {
            resolve_path(state, path)?
        } else {
            (
                PermissionMode::Ask,
                format!("no explicit policy covers `{scope}`"),
            )
        };
        effective = effective.min(mode);
        rationales.push(rationale);
    }
    if requirements.is_empty() {
        effective = PermissionMode::Ask;
        rationales.push("tool declared no permission requirements".to_owned());
    }
    Ok(PolicyResolution {
        mode: effective,
        rationale: rationales.join("; "),
        matched_rule,
        required_permissions,
    })
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
    let closest = state
        .roots
        .values()
        .filter(|root| canonical.starts_with(&root.canonical_path))
        .max_by_key(|root| root.canonical_path.components().count());
    Ok(match closest {
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
    let parent = path
        .parent()
        .ok_or_else(|| PolicyError::Denied("path has no parent".to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| PolicyError::Denied("path has no file name".to_owned()))?;
    let canonical_parent =
        std::fs::canonicalize(parent).map_err(|source| PolicyError::PathResolution {
            path: parent.to_path_buf(),
            source,
        })?;
    Ok(canonical_parent.join(file_name))
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
                    vec![PermissionRequirement::Network {
                        url: Url::parse("https://one.example").expect("url"),
                    }],
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
                    vec![PermissionRequirement::Network {
                        url: Url::parse("https://two.example").expect("url"),
                    }],
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
                vec![PermissionRequirement::Write {
                    path: directory.path().join("file.txt"),
                }],
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
