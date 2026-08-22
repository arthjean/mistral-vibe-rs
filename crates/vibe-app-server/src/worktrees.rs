//! The local-workspace half of the session contract: which worktrees a checkout
//! has, and which one a session was asked to open in.
//!
//! The reference splits these between its host handler and its server
//! (`vibe/app_server/_host.py:385-408` and `vibe/app_server/server.py:889-936`).
//! They sit together here because they answer the same question from both ends:
//! which local workspace does this client mean, and what has to be taken back
//! if opening it fails.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::worktree::{
    LinkedWorktree, PreparedWorktree, WorktreeError, list_linked_worktrees, prepare_worktree,
    remove_worktree,
};

use crate::host::expand_home;

/// The local workspace a `session/start` asks to run in.
///
/// Tagged on `kind`, as the reference discriminates its two protocol models
/// (`vibe/app_server/protocol.py:201-216`). `branch` is required on the
/// creating variant there, so it is not optional here either: a client that
/// wants the worktree's own name has to say the name twice.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum LocalWorkspaceSelection {
    /// A worktree the checkout already has, named by the directory to run in.
    #[serde(rename = "existing")]
    Existing { cwd: String },
    /// A worktree to create under the managed root before the session opens.
    #[serde(rename = "create")]
    Create { branch: String, name: String },
}

/// Where a session start resolved to, and the worktree it minted to get there.
#[derive(Debug)]
pub(crate) struct ResolvedWorkspace {
    pub(crate) cwd: String,
    /// [`Some`] only when this resolution created the worktree, which is the
    /// one case a failed start has to take back.
    pub(crate) created: Option<PreparedWorktree>,
}

/// Why a selection could not name a local workspace.
///
/// Every variant reaches the client as `invalid_params`, as the reference
/// translates its worktree exception at the session boundary
/// (`vibe/app_server/server.py:885-887`).
#[derive(Debug, Error)]
pub(crate) enum SelectionError {
    #[error("local project path is not a directory: {0}")]
    BaseNotADirectory(PathBuf),
    #[error("worktree is not linked to the local project: {0}")]
    NotLinked(PathBuf),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
}

/// The worktrees `workspace/worktrees/list` answers with, already projected.
///
/// A path git cannot answer for is not a failure: the reference logs and
/// answers an empty listing both when the directory is no repository and when
/// there is no git to ask, because an app-server is expected to run without one
/// (`vibe/app_server/_host.py:385-396`).
pub(crate) fn list_response(cwd: &Path) -> Result<Vec<Value>, WorktreeError> {
    let base = resolve_request_path(cwd);
    let projected = swallow_missing_checkout(list_linked_worktrees(&base))?
        .into_iter()
        .map(|worktree| {
            json!({
                "name": worktree.name,
                "branch": worktree.branch,
                "cwd": path_string(&worktree.path),
                "root": path_string(&worktree.root),
                "repoRoot": path_string(&worktree.repo_root),
            })
        })
        .collect();
    Ok(projected)
}

/// Resolves a selection into the directory the session runs in.
///
/// `requested` is what the client asked for as its local project root, which is
/// the process directory when it named none: a non-desktop client may omit it
/// (`vibe/app_server/server.py:1258-1261`).
pub(crate) fn resolve(
    selection: &LocalWorkspaceSelection,
    requested: Option<&str>,
    vibe_home: &Path,
) -> Result<ResolvedWorkspace, SelectionError> {
    let base = match requested {
        Some(path) => resolve_request_path(Path::new(path)),
        None => resolve_request_path(Path::new(".")),
    };
    if !base.is_dir() {
        return Err(SelectionError::BaseNotADirectory(base));
    }
    match selection {
        LocalWorkspaceSelection::Existing { cwd } => {
            let requested = resolve_request_path(Path::new(cwd));
            let linked = list_linked_worktrees(&base)?;
            if !linked.iter().any(|worktree| worktree.path == requested) {
                return Err(SelectionError::NotLinked(requested));
            }
            Ok(ResolvedWorkspace {
                cwd: path_string(&requested),
                created: None,
            })
        }
        LocalWorkspaceSelection::Create { branch, name } => {
            let prepared = prepare_worktree(name, &base, vibe_home, Some(branch))?;
            Ok(ResolvedWorkspace {
                cwd: path_string(&prepared.path),
                created: Some(prepared),
            })
        }
    }
}

/// Why a selection is refused on every call that reopens a recorded session.
///
/// A saved session was recorded against a directory of its own, and resolving a
/// selection on the way to it would mint a worktree and then open the session
/// somewhere else, leaving the worktree behind. The reference refuses its two
/// reopening methods for the same reason (`vibe/app_server/server.py:1238-1247`);
/// this port also reaches both intents through `session/start`, so the refusal
/// covers the flags that carry them.
pub(crate) const REOPEN_REFUSAL: &str =
    "localWorkspaceSelection is accepted only when a session is started, not when one is reopened";

/// The diagnostics file a worktree the app-server could not take back is
/// reported against.
pub(crate) const WORKTREE_LABEL: &str = "worktree";

/// Takes back a worktree this start created, after the start failed, and
/// answers what went wrong when the removal itself failed.
///
/// Only a worktree this resolution minted is removed: one the checkout already
/// had belongs to whoever made it, and deleting it would discard work no
/// session ever touched. A failed removal is reported rather than raised, since
/// the client is owed the error that failed the start, not this one
/// (`vibe/app_server/server.py:922-936`).
#[must_use]
pub(crate) fn discard(prepared: &PreparedWorktree) -> Option<String> {
    if !prepared.created {
        return None;
    }
    remove_worktree(prepared, prepared.branch_created)
        .err()
        .map(|error| {
            format!(
                "failed to remove worktree `{}` after the session failed to start: {error}",
                prepared.name
            )
        })
}

/// Turns a checkout this host cannot enumerate into an empty listing.
///
/// Neither reason is a client error: a directory outside a repository has no
/// worktrees, and an app-server is expected to run without git at all, so the
/// reference logs both and answers the empty list
/// (`vibe/app_server/_host.py:385-396`).
pub(crate) fn swallow_missing_checkout(
    listing: Result<Vec<LinkedWorktree>, WorktreeError>,
) -> Result<Vec<LinkedWorktree>, WorktreeError> {
    match listing {
        Err(WorktreeError::RepositoryRequired | WorktreeError::GitUnavailable(_)) => Ok(Vec::new()),
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
