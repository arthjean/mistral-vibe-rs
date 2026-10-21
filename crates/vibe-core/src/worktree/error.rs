use std::path::{Path, PathBuf};

use thiserror::Error;

/// Every way a worktree operation can refuse.
///
/// One enum where the reference publishes a hierarchy: `GitError` with its
/// `GitUnavailableError` and `GitRepositoryNotFoundError` subclasses, and
/// `WorktreeError` below `GitError` for what the worktree layer itself refuses
/// (`vibe/core/git/errors.py`, `vibe/core/git/worktree/repository.py:45`).
/// [`WorktreeError::reference_class`] names which class each variant stands
/// for, which is what a caller that branches on the class reads.
#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error(
        "--worktree NAME must be one portable path segment: no separator, no drive letter, \
             no character a Windows path forbids, and no reserved device name"
    )]
    InvalidName,
    /// A branch git itself refuses to name a ref with, a git-level refusal
    /// asked of `git check-ref-format --branch` (`vibe/core/git/repo.py:388-392`).
    #[error("`{branch}` is not a valid git branch name")]
    InvalidBranch { branch: String },
    /// A managed root that resolved out of the vibe home it was built under.
    #[error("managed worktree root `{target}` resolves outside `{managed_root}`")]
    ManagedRootEscape {
        target: PathBuf,
        managed_root: PathBuf,
    },
    /// The path is inside no git repository.
    #[error("--worktree requires a git repository")]
    RepositoryRequired,
    #[error("git worktree operations require git on PATH: {0}")]
    GitUnavailable(String),
    /// A refusal of the git layer rather than of the worktree layer: a command
    /// git failed, a layout it cannot answer for, a base outside the checkout.
    #[error("{message}")]
    Git { message: String },
    /// A refusal of the worktree layer, naming the worktree.
    #[error("worktree `{name}` failed: {message}")]
    Failed { name: String, message: String },
    /// Git refused to enumerate the worktrees of a checkout that exists.
    #[error("failed to list git worktrees: {message}")]
    ListFailed { message: String },
    /// A failure that carries a second failure it did not replace.
    ///
    /// The reference attaches the rollback's own failure to the original
    /// exception with `add_note` rather than raising it
    /// (`vibe/core/git/worktree/repository.py:506-510`), so the class of the
    /// original is what the note is attached to.
    #[error("{source}; {note}")]
    Noted {
        name: String,
        #[source]
        source: Box<WorktreeError>,
        note: String,
    },
    /// The claim store refused: an unusable holder id, a holder already
    /// active, or a record that could not be written
    /// (`vibe/core/git/worktree/record.py:40`).
    #[error("{message}")]
    Record { message: String },
    #[error("worktree I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl WorktreeError {
    pub(super) fn failed(name: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            name: name.to_owned(),
            message: message.into(),
        }
    }

    pub(super) fn git(message: impl Into<String>) -> Self {
        Self::Git {
            message: message.into(),
        }
    }

    pub(super) fn record(message: impl Into<String>) -> Self {
        Self::Record {
            message: message.into(),
        }
    }

    pub(super) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// The exception class the reference raises for this refusal.
    #[must_use]
    pub fn reference_class(&self) -> &'static str {
        match self {
            Self::InvalidName | Self::ManagedRootEscape { .. } | Self::Failed { .. } => {
                "WorktreeError"
            }
            Self::InvalidBranch { .. } | Self::Git { .. } | Self::ListFailed { .. } => "GitError",
            Self::RepositoryRequired => "GitRepositoryNotFoundError",
            Self::GitUnavailable(_) => "GitUnavailableError",
            Self::Noted { source, .. } => source.reference_class(),
            Self::Record { .. } => "WorktreeRecordError",
            Self::Io { .. } => "OSError",
        }
    }

    /// Whether the reference would catch this as a `GitError`, which is what
    /// its callers catch.
    #[must_use]
    pub fn is_git_error(&self) -> bool {
        !matches!(self.reference_class(), "WorktreeRecordError" | "OSError")
    }
}
