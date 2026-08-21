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
pub use trust::{
    TrustResolution, dangerous_directory_warning, resolve_location_safety, resolve_workspace_trust,
};
pub use update::resolve_startup_update_prompt;
pub use update::{
    production_update_gateway, refresh_update_cache, release_repository, run_check_upgrade,
    scheduled_update_gateway, update_cache_store, update_checks_enabled,
};
pub use vibe_core::worktree::{PreparedWorktree, WorktreeCleanupState};
pub use worktree::{CleanupOutcome, LaunchWorkspace, cleanup_worktree, cleanup_worktree_terminal};

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
    #[error("--worktree NAME must be a single path segment")]
    InvalidWorktreeName,
    #[error("--worktree requires a git repository")]
    WorktreeRepositoryRequired,
    #[error("git worktree operations require git on PATH: {0}")]
    WorktreeGitUnavailable(String),
    #[error("worktree `{name}` failed: {message}")]
    Worktree { name: String, message: String },
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
/// workspace, which is the order the reference reads them in.
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
        .or_else(|| std::env::var_os("VIBE_HOME").map(PathBuf::from))
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
