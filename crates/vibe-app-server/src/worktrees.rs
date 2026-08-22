//! Which worktrees a checkout has, answered for the client that is deciding
//! where to open a session.
//!
//! The reference answers this from its host handler
//! (`vibe/app_server/_host.py:385-408`), one layer above the enumeration
//! `vibe_core::worktree` publishes.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use vibe_core::worktree::{LinkedWorktree, WorktreeError, list_linked_worktrees};

use crate::host::expand_home;

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
