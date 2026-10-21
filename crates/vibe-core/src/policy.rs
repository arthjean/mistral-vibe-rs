//! The permission model a session's tools are gated by.
//!
//! This module holds the vocabulary that crosses the wire and the approval
//! callback: the modes, the four scopes, the requirements a call raises and the
//! rules an approval stores, and what deciding a call resolves to.
//! [`PermissionStore`] (`store`) decides a call from them,
//! [`PolicyGuardedTool`] (`guard`) puts that decision in front of a
//! handler, and `tool_rules` holds the list rules the file and `task` tools
//! resolve on their own.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::matching::fnmatch;
use crate::tools::ToolApprovalSource;

pub mod arity;
mod guard;
mod path_grants;
mod store;
mod tool_rules;

pub use guard::{PolicyGuardedTool, RequirementResolver, ToolGuard};
pub use path_grants::{
    PathGrantScope, PathScopeRefusal, approval_path_scope, available_path_scopes,
    path_grant_pattern, path_grant_pattern_matches, path_pattern_matches, path_scope_root,
    path_scope_target, scope_required_permissions,
};
pub(crate) use store::canonicalize_for_policy;
pub use store::{AllowlistPersistence, PermissionStore, PolicyLease};
pub use tool_rules::{
    matches_sensitive_pattern, resolve_file_tool_permission, resolve_task_tool_permission,
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
    /// Whether a stored approval covers this requirement only by being its
    /// exact text rather than by matching it as a glob.
    ///
    /// Reference `RequiredPermission.literal` (`vibe/permissions.py`), read by
    /// `PermissionStore.covers` (`vibe/core/tools/permissions.py:39-42`): a
    /// scope recorded because nothing wider was honest must stay out of reach
    /// of the wider grants, and its own text may carry glob characters. The
    /// reference excludes it from serialization but still validates it, so it
    /// never reaches the wire and a payload that carries it still reads.
    #[serde(default, skip_serializing)]
    pub literal: bool,
    /// The directory a recursive grant of this requirement would reach.
    ///
    /// Reference `RequiredPermission.path_scope_root` (`vibe/permissions.py`,
    /// since v2.25.8): set by [`path_scope_root`] on an outside-directory
    /// requirement whose target is a directory the session can enter, and
    /// null everywhere else, which is how it crosses the wire.
    #[serde(default)]
    pub path_scope_root: Option<String>,
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
            literal: false,
            path_scope_root: None,
        }
    }

    /// A command segment whose grant may not widen past itself.
    ///
    /// Reference `_build_required_permissions` takes this branch for a
    /// sensitive segment, `_resolve_guardrail_permission` for a guardrailed one
    /// such as a `find` carrying an execution predicate, and `resolve_permission`
    /// for a command only its whole text can scope. All three carry the text as
    /// its own session pattern and mark it literal, so approving `sudo apt
    /// update` never approves `sudo rm` and a stored `git log *` is never read
    /// as a glob.
    #[must_use]
    pub fn exact_command(segment: &str) -> Self {
        Self {
            scope: PermissionScope::CommandPattern,
            invocation_pattern: segment.to_owned(),
            session_pattern: segment.to_owned(),
            label: segment.to_owned(),
            literal: true,
            path_scope_root: None,
        }
    }

    /// A path a file tool or a shell operand reaches outside every root.
    ///
    /// Reference `resolve_file_tool_permission` and the shells'
    /// `_build_outside_directory_permission` since v2.25.8: the call is asked
    /// about the resolved path itself, the session pattern is its exact encoded
    /// grant, so approving one file leaves its siblings asking, and the
    /// recursive grant root is the path when it is a directory the session can
    /// enter.
    #[must_use]
    pub fn outside_path(resolved: &Path) -> Self {
        let path = resolved.display().to_string();
        Self {
            scope: PermissionScope::OutsideDirectory,
            session_pattern: path_grant_pattern(&path, PathGrantScope::Exact),
            label: format!("outside workdir ({path})"),
            literal: false,
            path_scope_root: path_scope_root(resolved),
            invocation_pattern: path,
        }
    }

    /// A file whose resolved path matched the tool's `sensitive_patterns`.
    ///
    /// Reference `resolve_file_tool_permission` (since v2.24.1) scopes the
    /// grant to that one file: the invocation pattern is the resolved path and
    /// the session pattern its `glob.escape`, so approving one sensitive file
    /// leaves every other one asking.
    #[must_use]
    pub fn sensitive_file(resolved: &str, tool: &str) -> Self {
        Self {
            scope: PermissionScope::FilePattern,
            invocation_pattern: resolved.to_owned(),
            session_pattern: glob_escape(resolved),
            label: format!("accessing sensitive files ({tool})"),
            literal: false,
            path_scope_root: None,
        }
    }

    /// The URL scope a fetch reaches: the origin a `web_fetch` call names
    /// (reference `WebFetch.resolve_permission`), or the host a connector
    /// calls.
    #[must_use]
    pub fn url_pattern(scope: &str) -> Self {
        Self {
            scope: PermissionScope::UrlPattern,
            invocation_pattern: scope.to_owned(),
            session_pattern: scope.to_owned(),
            label: format!("fetching from {scope}"),
            literal: false,
            path_scope_root: None,
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
    /// The tool call the decision is for, which an approval names so a
    /// client can attach the question to the call it gates.
    pub call_id: Option<String>,
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
    /// rule's session pattern under [`wildcard_match`], or equal it when the
    /// requirement is literal. An outside-directory requirement is matched by
    /// path instead, under [`path_pattern_matches`], so an encoded grant covers
    /// what its scope reaches and a path glob never lets `*` cross a separator.
    ///
    /// Only an approval is held to the literal reading. The reference stores
    /// approvals alone, each under a scope; a rule with no scope is this port's
    /// agent-profile rule, whose `*` refusal has to keep answering for every
    /// call, literal or not.
    #[must_use]
    pub fn covers(&self, tool: &str, requirement: &PermissionRequirement) -> bool {
        (self.tool == tool || self.tool == "*")
            && self.scope.is_none_or(|scope| scope == requirement.scope)
            && if self.scope == Some(PermissionScope::OutsideDirectory) {
                path_pattern_matches(&requirement.invocation_pattern, &self.pattern)
            } else if requirement.literal && self.scope.is_some() {
                self.pattern == requirement.invocation_pattern
            } else {
                wildcard_match(&requirement.invocation_pattern, &self.pattern)
            }
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

/// Python `glob.escape`: every `*`, `?` and `[` wrapped in brackets so the
/// text matches only itself, a Windows drive prefix left as written.
#[must_use]
pub fn glob_escape(text: &str) -> String {
    let drive = if cfg!(windows) {
        match Path::new(text).components().next() {
            Some(std::path::Component::Prefix(prefix)) => {
                prefix.as_os_str().to_string_lossy().len()
            }
            _ => 0,
        }
    } else {
        0
    };
    let (drive, rest) = text.split_at(drive.min(text.len()));
    let mut escaped = drive.to_owned();
    for character in rest.chars() {
        if matches!(character, '*' | '?' | '[') {
            escaped.push('[');
            escaped.push(character);
            escaped.push(']');
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// Whether `text` matches `pattern`, with the trailing arguments optional.
///
/// Reference `wildcard_match`: a pattern ending in ` *` also matches the text
/// that is the pattern without that suffix, so an approval stored for
/// `git status *` covers a later bare `git status`.
#[must_use]
pub fn wildcard_match(text: &str, pattern: &str) -> bool {
    if fnmatch(pattern, text) {
        return true;
    }
    pattern
        .strip_suffix(" *")
        .is_some_and(|prefix| fnmatch(prefix, text))
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

/// What the rules make of one call before anyone is asked.
///
/// Reference `UnifiedPermissionResolver.resolve`
/// (`vibe/app_server/_unified_permissions.py`), which reads a tool's
/// `PermissionContext` and the session's `PermissionStore.covers` into an
/// outcome without prompting. [`PermissionStore::authorize`] asks about exactly
/// what this leaves open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Refused, with the reason the model is told.
    Refused(String),
    /// Runs without asking: `covered` when stored approvals answered for every
    /// requirement the call raised, rather than the tool or its configuration.
    Cleared { covered: bool },
    /// Asks, though nothing scopes the call, so an approval that outlasts it
    /// grants the tool itself.
    Unscoped { rationale: String },
    /// Asks about these requirements, none of which a stored rule covers.
    Needs {
        requirements: Vec<PermissionRequirement>,
        rationale: String,
    },
}

impl From<PolicyResolution> for PermissionOutcome {
    fn from(resolution: PolicyResolution) -> Self {
        let PolicyResolution {
            mode,
            rationale,
            matched_rule,
            required_permissions,
        } = resolution;
        match mode {
            PermissionMode::Never => Self::Refused(rationale),
            PermissionMode::Always => Self::Cleared {
                covered: matched_rule.is_some(),
            },
            PermissionMode::Ask if required_permissions.is_empty() => Self::Unscoped { rationale },
            PermissionMode::Ask => Self::Needs {
                requirements: required_permissions,
                rationale,
            },
        }
    }
}

/// How long an approval [`PermissionStore::grant`] records lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantLifetime<'a> {
    /// The session only.
    Session,
    /// The session, and the tool's configuration for good. Reference
    /// `approve_always(save_permanently=True)` writes every session pattern
    /// into the allowlist.
    Permanent,
    /// As [`Self::Permanent`], except that only the patterns the tool's
    /// allowlist reads back are written: reference
    /// `UnifiedPermissionResolver._persist` drops a blank pattern, which a
    /// prefix-matched allowlist reads as every command, and a scope outside
    /// the tool's `allowlist_scopes`. What it drops lasts for the session.
    PermanentWithin(&'a [PermissionScope]),
}

impl GrantLifetime<'_> {
    /// Whether the approval of `requirement` lasts beyond the session.
    fn outlives_session(self, requirement: &PermissionRequirement) -> bool {
        match self {
            Self::Session => false,
            Self::Permanent => true,
            Self::PermanentWithin(scopes) => {
                !requirement.session_pattern.trim().is_empty()
                    && scopes.contains(&requirement.scope)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub input: Value,
    pub requirements: Vec<PermissionRequirement>,
    pub rationale: String,
    /// The tool call being gated, when the caller knows it.
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    ApproveOnce,
    /// Approved for the session, outside paths granted under the scope the
    /// operator chose, or the exact one when they chose none (reference
    /// `ApprovalDecision.path_scope`).
    ApproveForSession(Option<PathGrantScope>),
    /// Approved permanently, under the same choice of path scope.
    ApprovePermanently(Option<PathGrantScope>),
    Deny,
    /// Refused with the reason the model is told instead of the result.
    Reject(String),
    CancelTurn,
    /// The question itself was refused, which fails the turn with this
    /// message (reference `CallbackRejectedError`).
    Fail(String),
}

pub trait ApprovalAgent: Send + Sync {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a>;

    /// Who answers for `tool` when this agent does. An agent that asks
    /// somebody answers as the operator; one that approves without asking
    /// stands for a configured permission or for a bypass of the gate, which
    /// is what reference `_should_execute_tool` records as the source.
    fn attribution(&self, _tool: &str) -> ToolApprovalSource {
        ToolApprovalSource::User
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
    #[error("{0}")]
    TurnFailed(String),
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
    /// Reference `_handle_tool_skip`: the user declined the call, which the
    /// model reads as `reason`; a decline without a reason cancels the turn.
    #[error("{reason}")]
    Skipped { reason: String, cancelled: bool },
    #[error("permission state is busy with an active side effect")]
    Busy,
}

#[cfg(test)]
mod permission_parity_tests;
#[cfg(test)]
mod policy_tests;
