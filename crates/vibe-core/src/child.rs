//! Owned child processes and their process groups.
//!
//! Tracking the group a child leads and shutting that group down is identical
//! whether the child is piped, as an MCP stdio server or a shell command is, or
//! runs under a terminal [`crate::pty`] opened. Every one needs the same
//! escalation ladder (wait, SIGTERM, SIGKILL) applied to the leader and then to
//! any surviving descendants, so the ladder lives here once over a [`Leader`],
//! and each caller maps [`TerminationError`] into its own error type.

use std::future::Future;
use std::pin::Pin;
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
    /// The signal that ended the child, when one did.
    pub(crate) signal: Option<i32>,
}

impl From<ExitStatus> for ChildExit {
    fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            success: status.success(),
            signal,
        }
    }
}

/// The pipes of a freshly spawned child.
pub(crate) struct ChildPipes {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

/// What a [`Leader`] hands back from an operation that waits.
pub(crate) type LeaderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The process a [`ChildGroup`] leads, whichever backend started it: a piped
/// child here, or a terminal's child in [`crate::pty`].
pub(crate) trait Leader: Send {
    /// Reports the leader's exit status without blocking.
    fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>>;

    /// Waits for the leader to exit.
    fn wait(&mut self) -> LeaderFuture<'_, std::io::Result<ChildExit>>;

    /// Ends the leader itself: the signal a group without an owned process
    /// group gets, and on Windows the only one.
    fn kill(&mut self) -> Result<(), TerminationError>;

    /// Closes what keeps the leader's output open once it exits, which only a
    /// pseudoconsole does.
    fn close_console(
        &mut self,
        _grace: Duration,
    ) -> LeaderFuture<'_, Result<(), TerminationError>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(not(windows))]
impl Leader for Child {
    fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        Child::try_wait(self).map(|status| status.map(ChildExit::from))
    }

    fn wait(&mut self) -> LeaderFuture<'_, std::io::Result<ChildExit>> {
        Box::pin(async move { Child::wait(self).await.map(ChildExit::from) })
    }

    fn kill(&mut self) -> Result<(), TerminationError> {
        self.start_kill()
            .map_err(|error| TerminationError::Signal(error.to_string()))
    }
}

#[cfg(windows)]
impl Leader for AsyncGroupChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        self.inner()
            .try_wait()
            .map(|status| status.map(ChildExit::from))
    }

    fn wait(&mut self) -> LeaderFuture<'_, std::io::Result<ChildExit>> {
        Box::pin(async move { self.inner().wait().await.map(ChildExit::from) })
    }

    /// Ends the job object, and with it every descendant.
    fn kill(&mut self) -> Result<(), TerminationError> {
        self.start_kill()
            .map_err(|error| TerminationError::Signal(error.to_string()))
    }
}

/// A child process together with the process group it leads.
///
/// The leader is the last field so that a group of any backend coerces to
/// `ChildGroup<dyn Leader>`, which is how a caller holding either keeps one
/// type.
pub(crate) struct ChildGroup<L: ?Sized = PlatformChild> {
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only the POSIX process-group signals read it")
    )]
    group: Option<i32>,
    child: L,
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
            // The group sets the creation flags itself, over any the command holds.
            group
                .kill_on_drop(true)
                .creation_flags(crate::platform::background_creation_flags());
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
        Ok((Self::led_by(child, group), pipes))
    }
}

impl<L: Leader> ChildGroup<L> {
    /// The group `child` leads, `group` being its id when it owns one.
    pub(crate) const fn led_by(child: L, group: Option<i32>) -> Self {
        Self { group, child }
    }
}

impl<L: Leader + ?Sized> ChildGroup<L> {
    /// Waits for the group leader to exit.
    pub(crate) async fn wait(&mut self) -> std::io::Result<ChildExit> {
        self.child.wait().await
    }

    /// Reports the leader's exit status without blocking.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        self.child.try_wait()
    }

    /// Closes the leader's pseudoconsole, when it has one.
    pub(crate) async fn close_console(&mut self, grace: Duration) -> Result<(), TerminationError> {
        self.child.close_console(grace).await
    }

    /// The leader, for a test that has to reach into its backend.
    #[cfg(all(test, windows))]
    pub(crate) fn leader(&mut self) -> &mut L {
        &mut self.child
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
    /// The leader leads its own process group, so a grandchild that ignored
    /// the leader's exit is still addressable here rather than left running.
    /// `already_signaled` skips the first SIGTERM when the caller has already
    /// sent one while shutting the leader down.
    pub(crate) async fn reap_group(
        &mut self,
        grace: Duration,
        already_signaled: bool,
    ) -> Result<(), TerminationError> {
        #[cfg(unix)]
        {
            let Some(group) = self.group else {
                return Ok(());
            };
            if !group_alive(group)? {
                return Ok(());
            }
            if !already_signaled {
                self.signal(false)?;
            }
            if wait_for_group_exit(group, grace).await? {
                return Ok(());
            }
            self.signal(true)?;
            if wait_for_group_exit(group, grace).await? {
                return Ok(());
            }
            Err(TerminationError::Deadline)
        }
        #[cfg(windows)]
        {
            // A piped child's job object terminates its descendants with the
            // leader. A terminal's child has none: the tree kill reaches the
            // descendants still parented under a live leader, and closing the
            // pseudoconsole ends the ones attached to it that the leader left
            // behind. The console is closed even when the leader would not
            // die, so that failure is reported only after the close.
            if !already_signaled {
                self.signal(true)?;
            }
            let exit = self.shut_down(grace, Rung::Wait).await.map(drop);
            let closed = self.close_console(grace).await;
            exit.and(closed)
        }
    }

    /// The process group the child leads, when one was created.
    #[cfg(unix)]
    pub(crate) fn group(&self) -> Option<i32> {
        self.group
    }

    /// Signals the whole group, falling back to the leader when no group is
    /// owned. On Windows both rungs end the leader the way its backend does.
    pub(crate) fn signal(&mut self, force: bool) -> Result<(), TerminationError> {
        #[cfg(unix)]
        if let Some(group) = self.group {
            return signal_group(group, force);
        }
        let _ = force;
        self.child.kill()
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
    #[error("the pseudoconsole did not close: {0}")]
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "only a pseudoconsole is closed before it drops")
    )]
    Console(String),
}

/// Waits for a synchronous `child` up to `limit`, polling every `poll`, and
/// kills and reaps it once the deadline passes, answering [`None`] then.
///
/// A blocking wait cannot be bounded, so the wait polls; reaping right after
/// the kill is what keeps a timed-out child from lingering as a zombie.
///
/// # Errors
///
/// Fails when the child's status cannot be read, leaving it as it is.
pub(crate) fn wait_with_deadline(
    child: &mut std::process::Child,
    limit: Duration,
    poll: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(poll);
    }
}
