//! Running the commands an accepted update prompt installs with.
//!
//! Reference `do_update` (`vibe/cli/update_notifier/update.py`). The rest of
//! the update notifier lives in `vibe-cli`; this part stays here because each
//! command runs in a process group of its own, which only this crate's child
//! layer spawns and tears down.

use std::future::Future;
use std::time::Duration;

use crate::child::{ChildGroup, Rung};

/// Reference `_terminate`: a cancelled command has two seconds to answer the
/// termination signal before it is killed.
const UPGRADE_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Reference `do_update` once every command has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// At least one command exited zero.
    Succeeded,
    /// Every command failed, or the list named none.
    Failed,
    /// The operator cancelled, so the remaining commands never ran.
    Cancelled,
}

/// The shell the reference's `create_subprocess_shell` uses on each platform.
#[cfg(windows)]
const UPGRADE_SHELL: (&str, &str) = ("cmd", "/C");
#[cfg(not(windows))]
const UPGRADE_SHELL: (&str, &str) = ("sh", "-c");

/// Reference `do_update`: every command runs through the platform shell, and
/// one command exiting zero is enough for the upgrade to have succeeded.
///
/// `cancelled` stands for the reference's `CancelledError`: the running command
/// is terminated, killed [`UPGRADE_TERMINATE_GRACE`] later if it is still
/// alive, and the commands after it never start. Output is captured and never
/// rendered, as the reference captures it.
pub async fn run_upgrade_commands<C>(commands: &[String], cancelled: C) -> UpgradeOutcome
where
    C: Future<Output = ()>,
{
    let mut any_succeeded = false;
    let mut cancelled = std::pin::pin!(cancelled);
    for command in commands {
        let mut builder = tokio::process::Command::new(UPGRADE_SHELL.0);
        builder.arg(UPGRADE_SHELL.1).arg(command);
        let Ok((mut child, _pipes)) = ChildGroup::spawn(&mut builder) else {
            // A command the shell cannot start is a command that did not exit
            // zero, which is the reference's `returncode != 0`.
            continue;
        };
        let exit = tokio::select! {
            exit = child.wait() => exit,
            () = &mut cancelled => {
                let _ = child.shut_down(UPGRADE_TERMINATE_GRACE, Rung::Terminate).await;
                return UpgradeOutcome::Cancelled;
            }
        };
        any_succeeded |= exit.is_ok_and(|exit| exit.success);
    }
    if any_succeeded {
        UpgradeOutcome::Succeeded
    } else {
        UpgradeOutcome::Failed
    }
}

#[cfg(test)]
mod upgrade_tests;
