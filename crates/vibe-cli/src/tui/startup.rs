mod dialog;
mod invocation;
mod mounted;
mod preflight;
mod session;
mod trust;
mod update;
mod worktree;

use std::path::{Path, PathBuf};

use thiserror::Error;
use vibe_app_server::startup::{StartupHost, StartupHostError};
use vibe_app_server::workspace::WorkspacePaths;

use crate::Arguments;

pub use invocation::{
    InteractiveInvocation, InvocationRoute, PostMountAction, PreparedInvocation,
    ProgrammaticInvocation,
};
pub(super) use mounted::{MountedStartup, complete_mounted_startup};
pub(super) use preflight::{ReadyStartup, preflight};
pub use session::{ResumeResolution, resolve_bare_resume};
pub use trust::{resolve_location_safety, resolve_workspace_trust};
pub use update::resolve_startup_update_prompt;
pub use update::{
    production_update_gateway, refresh_update_cache, release_repository, run_check_upgrade,
    scheduled_update_gateway, update_cache_store, update_checks_enabled,
};
pub use vibe_core::worktree::PreparedWorktree;
pub(crate) use worktree::utility_model;
pub use worktree::{
    CleanupOutcome, LaunchWorkspace, cleanup_is_offered, cleanup_worktree,
    cleanup_worktree_terminal,
};

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Host(#[from] StartupHostError),
    #[error("startup I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `--workdir` naming something that is not a directory, reported with the
    /// path once it was expanded and made absolute, which is the form the
    /// reference prints (`vibe/cli/entrypoint.py:279-289`).
    #[error("--workdir does not exist or is not a directory: {0}")]
    WorkdirNotADirectory(PathBuf),
    /// `--add-dir` naming something that is not a directory, reported with the
    /// argument exactly as it was typed rather than as it resolved, which is
    /// the half of the pair the reference spells the other way
    /// (`vibe/cli/entrypoint.py:317-325`).
    #[error("--add-dir path does not exist or is not a directory: {0}")]
    AddDirNotADirectory(String),
    /// The directory the shell still points at was deleted underneath it. The
    /// second line is the way out, and names the flag that provides it
    /// (`vibe/cli/entrypoint.py:306-315`).
    #[error(
        "Current working directory no longer exists.\nThe directory this session was started \
         from has been deleted. Move to a directory that still exists and run vibe again, or \
         name one with --workdir."
    )]
    WorkingDirectoryGone,
    #[error(
        "--worktree NAME must be one portable path segment: no separator, no drive letter, \
             no character a Windows path forbids, and no reserved device name"
    )]
    InvalidWorktreeName,
    #[error("--worktree branch `{branch}` is not a valid Git branch name")]
    InvalidWorktreeBranch { branch: String },
    #[error("managed worktree root `{target}` resolves outside `{managed_root}`")]
    WorktreeManagedRoot {
        target: PathBuf,
        managed_root: PathBuf,
    },
    #[error("--worktree requires a git repository")]
    WorktreeRepositoryRequired,
    #[error("git worktree operations require git on PATH: {0}")]
    WorktreeGitUnavailable(String),
    #[error("worktree `{name}` failed: {message}")]
    Worktree { name: String, message: String },
    #[error("failed to list git worktrees: {0}")]
    WorktreeListFailed(String),
    /// A refusal of the git layer or of the claim store, printed as the core
    /// words it.
    #[error("{0}")]
    WorktreeRefused(String),
    #[error("terminal startup interaction failed: {0}")]
    Terminal(String),
    #[error("stdin prompt could not be read: {0}")]
    Stdin(std::io::Error),
}

#[must_use]
pub(super) fn startup_host(arguments: &Arguments, working_directory: &Path) -> StartupHost {
    StartupHost::new(workspace_paths(arguments, working_directory))
}

#[must_use]
pub(crate) fn workspace_paths(arguments: &Arguments, working_directory: &Path) -> WorkspacePaths {
    workspace_paths_for(arguments.session_root.as_deref(), working_directory)
}

/// The runtime paths a launch resolves, from the session root it named.
///
/// Every entry point resolves them here: the interactive launch, the one-shot
/// commands, and the log directory `/log` prints. A launch that names no
/// session root falls back to `VIBE_HOME`, then to the user's home, then to the
/// workspace, which is the order the reference reads them in. `VIBE_HOME` is
/// tilde-expanded on the way through, because the reference expands it before
/// it resolves it (`vibe/utils/paths.py`) and a home spelled `~/.vibe` in the
/// environment otherwise names a literal directory called `~`.
#[must_use]
pub(crate) fn workspace_paths_for(
    session_root: Option<&Path>,
    working_directory: &Path,
) -> WorkspacePaths {
    let vibe_home = vibe_home_for(session_root, working_directory);
    WorkspacePaths {
        session_root: session_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| vibe_home.join("sessions")),
        vibe_home,
        working_directory: working_directory.to_path_buf(),
    }
}

#[must_use]
pub(crate) fn vibe_home_directory(arguments: &Arguments, working_directory: &Path) -> PathBuf {
    vibe_home_for(arguments.session_root.as_deref(), working_directory)
}

fn vibe_home_for(session_root: Option<&Path>, working_directory: &Path) -> PathBuf {
    session_root
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os("VIBE_HOME")
                .map(PathBuf::from)
                .map(|path| worktree::expand_user_path(&path))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".vibe"))
        })
        .unwrap_or_else(|| working_directory.join(".vibe"))
}

pub(super) fn startup_io(path: &Path, source: std::io::Error) -> StartupError {
    StartupError::Io {
        path: path.to_path_buf(),
        source,
    }
}
