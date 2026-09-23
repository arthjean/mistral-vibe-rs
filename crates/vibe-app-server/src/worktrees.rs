//! The worktree half of the app-server contract: which worktrees a checkout
//! has, which one a session was asked to open in, and what taking one back
//! answers.
//!
//! The lifecycle itself is `vibe_core::worktree::lifecycle`, which speaks
//! paths. This translates: a `worktree` off the wire becomes a request the core
//! understands, the directory it resolves to becomes the session's working
//! directory, and the listing and removal become their wire shapes
//! (`vibe/app_server/_worktree_session.py`, `vibe/app_server/_host.py:774-912`).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_core::worktree::lifecycle::{
    LifecycleError, ResolvedWorktree, SessionWorktrees, WorktreeRequest,
};
use vibe_core::worktree::{
    LinkedWorktree, ManagedRoot, ManagedWorktree, PendingSessionHold, PreparedWorktree,
    WorktreeError, WorktreeRepository,
};

use crate::host::expand_home;

/// The worktree a `session/start` asks to run in.
///
/// Tagged on `kind`, as the reference discriminates its three protocol models
/// (`vibe/app_server/protocol.py:317-335`). `branch` is required on the
/// creating variant there, so it is not optional here either.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum WorktreeInput {
    /// A worktree the checkout already has, named by the directory to run in.
    #[serde(rename = "existing")]
    Existing { cwd: String },
    /// A worktree to create under the managed root before the session opens.
    #[serde(rename = "create")]
    Create { branch: String, name: String },
    /// A worktree to create under a name the naming model picks from `prompt`.
    #[serde(rename = "auto")]
    Auto {
        #[serde(default)]
        prompt: Option<String>,
    },
}

impl WorktreeInput {
    /// The request in the lifecycle's vocabulary, refusing the empty strings
    /// the reference's `min_length=1` refuses.
    pub(crate) fn request(&self) -> Result<WorktreeRequest, String> {
        let required = |field: &str, value: &str| {
            if value.is_empty() {
                Err(format!("worktree.{field} must not be empty"))
            } else {
                Ok(())
            }
        };
        match self {
            Self::Existing { cwd } => {
                required("cwd", cwd)?;
                Ok(WorktreeRequest::UseExisting {
                    cwd: PathBuf::from(cwd),
                })
            }
            Self::Create { branch, name } => {
                required("branch", branch)?;
                required("name", name)?;
                Ok(WorktreeRequest::CreateNamed {
                    name: name.clone(),
                    branch: Some(branch.clone()),
                })
            }
            Self::Auto { prompt } => Ok(WorktreeRequest::CreateForPrompt {
                prompt: prompt.clone(),
            }),
        }
    }
}

/// Where a session start resolved to, and what a failed start has to undo.
#[derive(Debug, Default)]
pub(crate) struct WorktreeResolution {
    /// The directory the session moves to, or [`None`] when it stays where it
    /// asked to be.
    pub(crate) cwd: Option<String>,
    /// The worktree this resolution raised, when it raised one.
    pub(crate) prepared: Option<PreparedWorktree>,
    /// The attachment hold that keeps retention off the worktree until the
    /// session registers as its holder.
    pub(crate) pending_hold: Option<PendingSessionHold>,
}

impl WorktreeResolution {
    /// The worktree this start created, which is the one a session that never
    /// ran a turn takes back on close.
    pub(crate) fn created(&self) -> Option<&PreparedWorktree> {
        self.prepared.as_ref().filter(|worktree| worktree.created)
    }
}

/// Resolves a start's `worktree` into the directory the session runs in.
///
/// A start that asks for none still takes an attachment hold when it opens
/// inside a managed worktree, so retention sees it occupied before the session
/// registers (`vibe/app_server/_worktree_session.py:76-89`). `requested` is the
/// client's working directory, the process directory when it named none.
/// `suggest` answers the naming model's name for an `auto` request.
pub(crate) fn resolve(
    input: Option<&WorktreeInput>,
    requested: Option<&str>,
    lifecycle: &SessionWorktrees,
    suggest: impl FnOnce(Option<&str>) -> Option<String>,
) -> Result<WorktreeResolution, SelectionError> {
    let base = resolve_request_path(Path::new(requested.unwrap_or(".")));
    let Some(input) = input else {
        return Ok(WorktreeResolution {
            pending_hold: lifecycle.hold_for_attachment(&base),
            ..WorktreeResolution::default()
        });
    };
    let request = input.request().map_err(SelectionError::Invalid)?;
    let suggested = match &request {
        WorktreeRequest::CreateForPrompt { prompt } => suggest(prompt.as_deref()),
        _ => None,
    };
    let ResolvedWorktree {
        cwd,
        prepared,
        pending_hold,
    } = lifecycle.resolve(&request, &base, suggested.as_deref())?;
    Ok(WorktreeResolution {
        cwd: Some(path_string(&cwd)),
        prepared,
        pending_hold,
    })
}

/// Why a `worktree` could not name a directory to run in. Every variant
/// reaches the client as `invalid_params`, as the reference translates its git
/// failures at the session boundary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SelectionError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
}

/// The workspace roots a session moved into a worktree keeps: the worktree
/// first, then every root the client authorized besides the checkout it moved
/// out of (`vibe/app_server/_worktree_session.py:236-255`).
pub(crate) fn moved_roots(cwd: &str, previous: Option<&str>, roots: &[String]) -> Vec<String> {
    let previous = resolve_request_path(Path::new(previous.unwrap_or(".")));
    let moved = resolve_request_path(Path::new(cwd));
    let mut kept = vec![cwd.to_owned()];
    for root in roots {
        let resolved = resolve_request_path(Path::new(root));
        let spelled = path_string(&expand_home(Path::new(root)));
        if resolved != previous && resolved != moved && !kept.contains(&spelled) {
            kept.push(spelled);
        }
    }
    kept
}

/// Why a `worktree` is refused on every call that reopens a recorded session.
///
/// A saved session already has a directory, and honoring one here would move
/// it out from under a transcript that names the old paths
/// (`vibe/app_server/_worktree_session.py:176-189`).
pub(crate) const REOPEN_REFUSAL: &str =
    "a worktree can be requested only when a session is started, not when one is reopened";

/// The diagnostics file a worktree the app-server could not take back is
/// reported against.
pub(crate) const WORKTREE_LABEL: &str = "worktree";

/// What `workspace/git/worktrees/list` answers for `cwd`.
///
/// A retained worktree's session is listed against the repository it came
/// from, so a session whose worktree was reclaimed still finds its project. A
/// path in no repository, or a host without git, lists nothing rather than
/// refusing, because an app server is expected to run without either
/// (`vibe/app_server/_host.py:774-836`).
pub(crate) fn list_response(
    cwd: &Path,
    include_details: bool,
    managed: &ManagedRoot,
) -> Result<Value, WorktreeError> {
    let cwd = resolve_request_path(cwd);
    let mapping =
        ManagedWorktree::at(managed, &cwd).and_then(|held| held.retained_repository_mapping(&cwd));
    let listing_cwd = mapping
        .as_ref()
        .map_or_else(|| cwd.clone(), |mapping| mapping.cwd.clone());
    let opened = match &mapping {
        Some(mapping) => WorktreeRepository::open_at(&mapping.cwd, Some(&mapping.root), managed),
        None => WorktreeRepository::open(&cwd, managed),
    };
    let listed = swallow_missing_checkout(opened.and_then(|repository| {
        let worktrees = repository.linked()?;
        let counterpart = repository.repository_counterpart();
        let mapped = repository.repository_mapped_cwd()?;
        let root = repository.root()?;
        Ok(Some((worktrees, counterpart, mapped, root)))
    }))?;
    let (worktrees, counterpart, mapped, root) = match listed {
        Some(listed) => (listed.0, listed.1, Some(listed.2), Some(listed.3)),
        None => (Vec::new(), None, None, None),
    };

    let details = (include_details && !worktrees.is_empty())
        .then(|| details(&listing_cwd, &worktrees, managed));
    let entries = worktrees
        .iter()
        .map(|worktree| {
            let changes = details
                .as_ref()
                .and_then(|details| {
                    details
                        .changes
                        .iter()
                        .find(|(branch, _)| *branch == worktree.branch)
                })
                .and_then(|(_, changes)| changes.clone());
            json!({
                "name": worktree.name,
                "branch": worktree.branch,
                "cwd": path_string(&worktree.path),
                "root": path_string(&worktree.root),
                "repoRoot": path_string(&worktree.repo_root),
                "branchChanges": changes,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "worktrees": entries,
        "repositoryBranch": details.and_then(|details| details.repository_branch),
        "repositoryCwd": counterpart.as_deref().map(path_string),
        "repositoryMappedCwd": mapped.as_deref().map(path_string),
        "repositoryRoot": root.as_deref().map(path_string),
    }))
}

struct Details {
    repository_branch: Option<String>,
    changes: Vec<(String, Option<Value>)>,
}

/// What the listing reports only when a caller renders it: the lines each
/// branch changed against its merge base, counted in one checkout, and the
/// branch the main checkout is on (`vibe/app_server/_host.py:845-882`).
fn details(cwd: &Path, worktrees: &[LinkedWorktree], managed: &ManagedRoot) -> Details {
    let Ok(checkout) = WorktreeRepository::open(cwd, managed) else {
        return Details {
            repository_branch: None,
            changes: Vec::new(),
        };
    };
    let changes = worktrees
        .iter()
        .map(|worktree| {
            let counted = checkout.changes_on(&worktree.branch).map(
                |changes| json!({"additions": changes.additions, "deletions": changes.deletions}),
            );
            (worktree.branch.clone(), counted)
        })
        .collect();
    let repository_branch = worktrees
        .first()
        .and_then(|first| WorktreeRepository::open(&first.repo_root, managed).ok())
        .and_then(|repository| repository.branch());
    Details {
        repository_branch,
        changes,
    }
}

/// What `workspace/git/worktrees/remove` answers for `cwd`.
///
/// A kept worktree is a normal outcome the client renders, never a fault, so a
/// removal that failed answers `kept_error` with the reason
/// (`vibe/app_server/_host.py:894-912`).
pub(crate) fn remove_response(cwd: &Path, managed: &ManagedRoot) -> Value {
    let cwd = resolve_request_path(cwd);
    let Some(held) = ManagedWorktree::at(managed, &cwd) else {
        return removal("kept_unmanaged", None, None, false, Vec::new());
    };
    match held.release(None) {
        Ok(release) => removal(
            release.outcome.as_str(),
            release.root.as_deref().map(path_string),
            release.branch,
            release.branch_deleted,
            release.reasons,
        ),
        Err(error) => {
            vibe_core::observability::log(
                vibe_core::observability::LogLevel::Warning,
                &format!("Failed to remove worktree cwd={}: {error}", cwd.display()),
            );
            removal("kept_error", None, None, false, vec![error.to_string()])
        }
    }
}

fn removal(
    outcome: &str,
    root: Option<String>,
    branch: Option<String>,
    branch_deleted: bool,
    reasons: Vec<String>,
) -> Value {
    json!({
        "outcome": outcome,
        "root": root,
        "branch": branch,
        "branchDeleted": branch_deleted,
        "reasons": reasons,
    })
}

/// Turns a checkout this host cannot enumerate into an empty listing.
///
/// Neither reason is a client error: a directory outside a repository has no
/// worktrees, and an app server is expected to run without git at all.
pub(crate) fn swallow_missing_checkout<T>(
    listing: Result<Option<T>, WorktreeError>,
) -> Result<Option<T>, WorktreeError> {
    match listing {
        Err(WorktreeError::RepositoryRequired | WorktreeError::GitUnavailable(_)) => Ok(None),
        other => other,
    }
}

/// Expands a leading `~` and resolves the result, as the reference resolves
/// every path a client hands it (`vibe/app_server/_host.py:349-351`).
///
/// A path that does not exist yet cannot be canonicalized, and is answered as
/// given rather than dropped: the caller reports the path it was asked about.
fn resolve_request_path(path: &Path) -> PathBuf {
    let expanded = expand_home(path);
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
