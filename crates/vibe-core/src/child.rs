//! Owned child processes and their process groups.
//!
//! Spawning a child, tracking the group it leads, and shutting that group down
//! is identical whether the child is a shell terminal or an MCP stdio server.
//! Both needed the same escalation ladder (wait, SIGTERM, SIGKILL) applied to
//! the leader and then to any surviving descendants, so the ladder lives here
//! once and each caller maps [`TerminationError`] into its own error type.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use thiserror::Error;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

#[cfg(windows)]
use command_group::{AsyncCommandGroup, AsyncGroupChild};
#[cfg(not(windows))]
use tokio::process::Child;

#[cfg(windows)]
type PlatformChild = AsyncGroupChild;
#[cfg(not(windows))]
type PlatformChild = Child;

pub(crate) const GROUP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How a child ended, normalized across the two backends that can own one.
///
/// A PTY child is reaped by `portable-pty`, which reports its own status type
/// rather than a [`std::process::ExitStatus`], so the ladder speaks this instead
/// of a type only one backend can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChildExit {
    pub(crate) code: Option<i32>,
    pub(crate) success: bool,
}

impl From<ExitStatus> for ChildExit {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

/// The pipes of a freshly spawned child.
pub(crate) struct ChildPipes {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

/// A child process together with the process group it leads.
pub(crate) struct ChildGroup {
    child: PlatformChild,
    group: Option<i32>,
}

/// Where to enter the shutdown ladder.
///
/// A caller that already closed the child's input starts at [`Rung::Wait`] so a
/// well-behaved process exits on its own; a caller interrupting live work
/// starts at [`Rung::Terminate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rung {
    Wait,
    Terminate,
    Kill,
}

impl ChildGroup {
    /// Spawns `command` in its own process group with all three pipes captured.
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Self, ChildPipes)> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        #[cfg(not(windows))]
        let mut child = command.spawn()?;
        #[cfg(windows)]
        let mut child = {
            let mut group = command.group();
            group.kill_on_drop(true);
            group.spawn()?
        };
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        #[cfg(not(windows))]
        let pipes = ChildPipes {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        };
        #[cfg(windows)]
        let pipes = {
            let inner = child.inner();
            ChildPipes {
                stdin: inner.stdin.take(),
                stdout: inner.stdout.take(),
                stderr: inner.stderr.take(),
            }
        };
        Ok((Self { child, group }, pipes))
    }

    /// Waits for the group leader to exit.
    pub(crate) async fn wait(&mut self) -> std::io::Result<ChildExit> {
        #[cfg(not(windows))]
        {
            self.child.wait().await.map(ChildExit::from)
        }
        #[cfg(windows)]
        {
            self.child.inner().wait().await.map(ChildExit::from)
        }
    }

    /// Reports the leader's exit status without blocking.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        #[cfg(not(windows))]
        {
            self.child
                .try_wait()
                .map(|status| status.map(ChildExit::from))
        }
        #[cfg(windows)]
        {
            self.child
                .inner()
                .try_wait()
                .map(|status| status.map(ChildExit::from))
        }
    }

    /// Shuts the leader down, escalating from `from` until it exits.
    ///
    /// Each rung waits up to `grace` before the next one fires.
    pub(crate) async fn shut_down(
        &mut self,
        grace: Duration,
        from: Rung,
    ) -> Result<ChildExit, TerminationError> {
        for rung in [Rung::Wait, Rung::Terminate, Rung::Kill] {
            if rung < from {
                continue;
            }
            match rung {
                Rung::Wait => {}
                Rung::Terminate => self.signal(false)?,
                Rung::Kill => self.signal(true)?,
            }
            match tokio::time::timeout(grace, self.wait()).await {
                Ok(Ok(status)) => return Ok(status),
                Ok(Err(source)) => return Err(TerminationError::Wait(source.to_string())),
                Err(_) => {}
            }
        }
        Err(TerminationError::Deadline)
    }

    /// Reaps descendants that outlived the group leader.
    ///
    /// `already_signaled` skips the first SIGTERM when the caller has already
    /// sent one while shutting the leader down.
    pub(crate) async fn reap_group(
        &mut self,
        grace: Duration,
        already_signaled: bool,
    ) -> Result<(), TerminationError> {
        #[cfg(unix)]
        {
            if !self.group_alive()? {
                return Ok(());
            }
            if !already_signaled {
                self.signal(false)?;
            }
            if self.wait_for_group_exit(grace).await? {
                return Ok(());
            }
            self.signal(true)?;
            if self.wait_for_group_exit(grace).await? {
                return Ok(());
            }
            Err(TerminationError::Deadline)
        }
        #[cfg(windows)]
        {
            // The job object terminates descendants with the leader, so only the
            // leader itself has to be confirmed dead.
            if !already_signaled {
                self.signal(true)?;
            }
            self.shut_down(grace, Rung::Wait).await.map(drop)
        }
    }

    /// Signals the whole group, falling back to the leader when no group is owned.
    #[cfg(unix)]
    pub(crate) fn signal(&mut self, force: bool) -> Result<(), TerminationError> {
        let Some(group) = self.group else {
            return self
                .child
                .start_kill()
                .map_err(|error| TerminationError::Signal(error.to_string()));
        };
        signal_group(group, force)
    }

    #[cfg(windows)]
    pub(crate) fn signal(&mut self, _force: bool) -> Result<(), TerminationError> {
        self.child
            .start_kill()
            .map_err(|error| TerminationError::Signal(error.to_string()))
    }

    /// Reports whether any member of the group is still alive.
    #[cfg(unix)]
    pub(crate) fn group_alive(&self) -> Result<bool, TerminationError> {
        self.group.map_or(Ok(false), group_alive)
    }

    #[cfg(unix)]
    async fn wait_for_group_exit(&self, grace: Duration) -> Result<bool, TerminationError> {
        match self.group {
            Some(group) => wait_for_group_exit(group, grace).await,
            None => Ok(true),
        }
    }
}

/// Signals every member of `group`.
///
/// A group nobody is left in is a group already shut down, so `ESRCH` is a
/// success rather than a failure to report.
#[cfg(unix)]
pub(crate) fn signal_group(group: i32, force: bool) -> Result<(), TerminationError> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let signal = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    match killpg(Pid::from_raw(group), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(TerminationError::Signal(error.to_string())),
    }
}

/// Reports whether any member of `group` is still alive.
#[cfg(unix)]
pub(crate) fn group_alive(group: i32) -> Result<bool, TerminationError> {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    match kill(Pid::from_raw(-group), None) {
        Ok(()) | Err(Errno::EPERM) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(TerminationError::Signal(error.to_string())),
    }
}

/// Waits up to `grace` for every member of `group` to leave it.
#[cfg(unix)]
pub(crate) async fn wait_for_group_exit(
    group: i32,
    grace: Duration,
) -> Result<bool, TerminationError> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if !group_alive(group)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(GROUP_POLL_INTERVAL).await;
    }
}

/// A failure while shutting a child process group down.
#[derive(Debug, Error)]
pub(crate) enum TerminationError {
    #[error("cannot signal the process group: {0}")]
    Signal(String),
    #[error("cannot reap the process: {0}")]
    Wait(String),
    #[error("the process group did not exit before the cleanup deadline")]
    Deadline,
}
