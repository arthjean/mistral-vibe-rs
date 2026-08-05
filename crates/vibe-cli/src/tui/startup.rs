mod dialog;
mod invocation;
mod mounted;
mod session;
mod trust;
mod update;
mod worktree;

use std::path::{Path, PathBuf};

use thiserror::Error;
use vibe_app_server::release3::Release3Paths;
use vibe_app_server::startup::{StartupHost, StartupHostError};

use crate::Arguments;

pub use invocation::{
    InteractiveInvocation, InvocationRoute, PostMountAction, PreparedInvocation,
    ProgrammaticInvocation,
};
pub(super) use mounted::{MountedStartup, complete_mounted_startup};
pub use session::{ResumeResolution, resolve_bare_resume};
pub use trust::{
    TrustResolution, dangerous_directory_warning, resolve_location_safety, resolve_workspace_trust,
};
pub use update::resolve_startup_update_prompt;
pub use update::{
    production_update_gateway, refresh_update_cache, run_check_upgrade, unix_seconds,
    update_cache_store, update_checks_enabled,
};
pub use worktree::{CleanupOutcome, LaunchWorkspace, PreparedWorktree, WorktreeCleanupState};

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
    #[error("worktree `{name}` failed: {message}")]
    Worktree { name: String, message: String },
    #[error("terminal startup interaction failed: {0}")]
    Terminal(String),
    #[error("stdin prompt could not be read: {0}")]
    Stdin(std::io::Error),
}

#[must_use]
pub(super) fn startup_host(arguments: &Arguments, working_directory: &Path) -> StartupHost {
    StartupHost::new(release3_paths(arguments, working_directory))
}

#[must_use]
pub(super) fn release3_paths(arguments: &Arguments, working_directory: &Path) -> Release3Paths {
    let vibe_home = vibe_home_directory(arguments, working_directory);
    Release3Paths {
        session_root: arguments
            .session_root
            .clone()
            .unwrap_or_else(|| vibe_home.join("sessions")),
        vibe_home,
        working_directory: working_directory.to_path_buf(),
    }
}

#[must_use]
pub(crate) fn vibe_home_directory(arguments: &Arguments, working_directory: &Path) -> PathBuf {
    arguments
        .session_root
        .as_deref()
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
