//! What a session's lifecycle means for worktrees: the directory it opens in,
//! the holders occupying it, and the cleanup after a start that fails.
//!
//! Reference `vibe/core/session/worktrees.py`. Everything here speaks paths
//! rather than a wire format, so the app server translates its request into
//! [`WorktreeRequest`] and nothing here learns a protocol type.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::observability::{self, LogLevel};
use crate::platform::{expand_home, resolve_lenient};

use super::{
    ManagedRoot, ManagedWorktree, PendingSessionHold, PreparedWorktree, RetainedRepositoryMapping,
    WorktreeError, WorktreeRepository,
};

/// What a session asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRequest {
    /// Run in a worktree already linked to the project.
    UseExisting { cwd: PathBuf },
    /// Create a worktree with a caller-supplied name.
    CreateNamed {
        name: String,
        branch: Option<String>,
    },
    /// Create a worktree whose name comes from the naming model.
    CreateForPrompt { prompt: Option<String> },
}

/// Where a session is going to work, plus anything that had to be created for it.
///
/// `prepared` stays [`None`] when the worktree existed beforehand; that same
/// field therefore tells a failed start exactly what it must roll back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorktree {
    pub cwd: PathBuf,
    pub prepared: Option<PreparedWorktree>,
    pub pending_hold: Option<PendingSessionHold>,
}

/// Why a request names no directory to run in.
#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("local project path is not a directory: {0}")]
    BaseNotADirectory(PathBuf),
    #[error("the local project has no linked worktree at {0}")]
    NotLinked(PathBuf),
    #[error("worktree is no longer available: {0}")]
    NoLongerAvailable(PathBuf),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
}

/// The worktree decisions a session depends on, all against one managed root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWorktrees {
    managed: ManagedRoot,
}

impl SessionWorktrees {
    #[must_use]
    pub fn new(managed: ManagedRoot) -> Self {
        Self { managed }
    }

    #[must_use]
    pub fn managed(&self) -> &ManagedRoot {
        &self.managed
    }

    /// Resolves a request to the session's working directory.
    ///
    /// An existing worktree has to be one the project lists, and a managed one
    /// is held for attachment at once; one whose record is gone and whose
    /// directory went with it is refused (`vibe/core/session/worktrees.py:83-118`).
    pub fn resolve(
        &self,
        request: &WorktreeRequest,
        base_cwd: &Path,
        suggested_name: Option<&str>,
    ) -> Result<ResolvedWorktree, LifecycleError> {
        let base_cwd = resolve_lenient(&expand_home(base_cwd));
        if !base_cwd.is_dir() {
            return Err(LifecycleError::BaseNotADirectory(base_cwd));
        }
        let created = match request {
            WorktreeRequest::UseExisting { cwd } => {
                let requested = resolve_lenient(&expand_home(cwd));
                let linked = WorktreeRepository::open(&base_cwd, &self.managed)?.linked()?;
                if !linked.iter().any(|worktree| worktree.path == requested) {
                    return Err(LifecycleError::NotLinked(requested));
                }
                let managed = ManagedWorktree::at(&self.managed, &requested);
                let pending_hold = match &managed {
                    Some(managed) => managed.hold_for_attachment()?,
                    None => None,
                };
                if managed.is_some() && pending_hold.is_none() && !requested.is_dir() {
                    return Err(LifecycleError::NoLongerAvailable(requested));
                }
                return Ok(ResolvedWorktree {
                    cwd: requested,
                    prepared: None,
                    pending_hold,
                });
            }
            WorktreeRequest::CreateNamed { name, branch } => {
                WorktreeRepository::open(&base_cwd, &self.managed)?
                    .prepare(name, branch.as_deref())?
            }
            WorktreeRequest::CreateForPrompt { prompt } => {
                WorktreeRepository::open(&base_cwd, &self.managed)?
                    .prepare_auto(prompt.as_deref(), suggested_name)?
            }
        };
        Ok(ResolvedWorktree {
            cwd: created.path.clone(),
            pending_hold: created.pending_hold.clone(),
            prepared: Some(created),
        })
    }

    /// The attachment hold a session starting in `cwd` with no request takes,
    /// so retention sees the worktree occupied before the session registers.
    pub fn hold_for_attachment(&self, cwd: &Path) -> Option<PendingSessionHold> {
        ManagedWorktree::at(&self.managed, cwd)?
            .hold_for_attachment()
            .ok()
            .flatten()
    }

    /// Undoes what a failed start did, and only that: the pending hold, and a
    /// worktree this start created with the branch it created.
    ///
    /// Best effort throughout, since the session already failed and the error
    /// the caller is about to report matters more than a directory left
    /// behind (`vibe/core/session/worktrees.py:153-181`). Answers the failure
    /// it logged, so a caller with a diagnostics channel can publish it.
    pub fn cleanup(
        &self,
        worktree: Option<&PreparedWorktree>,
        pending_hold: Option<&PendingSessionHold>,
    ) -> Option<String> {
        if let Some(hold) = pending_hold {
            hold.release();
        }
        let worktree = worktree.filter(|worktree| worktree.created)?;
        if let Err(error) = worktree.remove(worktree.branch_created) {
            let note = format!(
                "failed to clean up worktree `{}` after the session failed to start: {error}",
                worktree.name
            );
            observability::log(LogLevel::Warning, &note);
            return Some(note);
        }
        if let Some(managed) = ManagedWorktree::at(&self.managed, &worktree.root) {
            managed.forget();
        }
        None
    }

    /// Puts back the retained worktree `cwd` sits in, answering whether it
    /// recreated one. A directory outside every managed worktree answers
    /// `false`.
    pub fn restore(&self, cwd: &Path) -> Result<bool, WorktreeError> {
        match ManagedWorktree::at(&self.managed, cwd) {
            Some(managed) => managed.restore(cwd),
            None => Ok(false),
        }
    }

    /// Where `cwd` sits in the repository its retained worktree came from.
    #[must_use]
    pub fn retained_repository_mapping(&self, cwd: &Path) -> Option<RetainedRepositoryMapping> {
        ManagedWorktree::at(&self.managed, cwd)?.retained_repository_mapping(cwd)
    }

    #[must_use]
    pub fn is_managed(&self, cwd: &Path) -> bool {
        ManagedWorktree::at(&self.managed, cwd).is_some()
    }

    /// Marks the worktree a session stands in as occupied, turning its pending
    /// hold into the session's own. Outside a managed worktree this only
    /// releases the pending hold (`vibe/core/session/worktrees.py:190-203`).
    pub fn hold(
        &self,
        cwd: &Path,
        session_id: &str,
        pending_hold: Option<&PendingSessionHold>,
    ) -> Result<(), WorktreeError> {
        match ManagedWorktree::at(&self.managed, cwd) {
            Some(managed) => managed.hold(session_id, pending_hold),
            None => {
                if let Some(hold) = pending_hold {
                    hold.release();
                }
                Ok(())
            }
        }
    }

    /// The managed worktree containing a directory, when there is one.
    #[must_use]
    pub fn root(&self, cwd: &Path) -> Option<PathBuf> {
        ManagedWorktree::at(&self.managed, cwd).map(|managed| managed.root())
    }

    /// Removes this session's hold so a later sweep may reclaim the worktree;
    /// a hold left behind by a finished session would pin it indefinitely.
    pub fn release(&self, cwd: &Path, session_id: &str) {
        if let Some(managed) = ManagedWorktree::at(&self.managed, cwd) {
            let _ = managed.release_holder(session_id);
        }
    }
}
