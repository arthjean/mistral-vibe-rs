//! The terminal a managed shell session runs under.
//!
//! The reference opens a real PTY for every managed session and hands the child
//! a controlling terminal (`vibe/core/tools/builtins/managed_shell/_posix.py`),
//! which is what makes a program that probes for a terminal behave the way it
//! behaves under one, and what lets a control key reach the foreground process
//! group instead of landing in a pipe nobody reads as a signal. Opening one
//! directly needs the `setsid` and `TIOCSCTTY` calls that `unsafe_code` forbids
//! here, so `portable-pty` owns those two and this module owns the rest: the
//! blocking-to-async bridge, the escalation ladder over the process group, and
//! the single release of the writer.
//!
//! Everything here is deliberately backend-shaped rather than session-shaped:
//! [`crate::process::TerminalManager`] decides which backend a terminal gets and
//! keeps one vocabulary in front of both.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::child::{ChildExit, GROUP_POLL_INTERVAL, Rung, TerminationError};

/// The name a session reports for the terminal behind it, reference
/// `ManagedTerminal.pty_backend`.
pub(crate) const PTY_BACKEND: &str = if cfg!(windows) { "conpty" } else { "posix" };

/// The visible size the reference pins through `COLUMNS` and `LINES` when it
/// builds a managed session's environment.
const DEFAULT_COLUMNS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// What one terminal needs to start a child under it.
pub(crate) struct PtySpec<'a> {
    pub(crate) program: &'a Path,
    pub(crate) arguments: &'a [String],
    pub(crate) working_directory: &'a Path,
    pub(crate) environment: &'a BTreeMap<String, String>,
}

/// The two halves a caller drives the terminal through.
///
/// Both are blocking, which is why the writer arrives already wrapped: a write
/// runs on a blocking task while the reader runs on its own thread.
pub(crate) struct PtyStreams {
    pub(crate) reader: Box<dyn std::io::Read + Send>,
    pub(crate) writer: PtyWriter,
}

/// A child running under its own terminal, session and process group.
pub(crate) struct PtyTerminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// `portable-pty` calls `setsid` before `exec`, so the child leads both its
    /// session and its process group and its pid is that group's id.
    group: Option<i32>,
    /// Held so the terminal outlives the child: dropping the master closes the
    /// descriptor the reader thread is still draining.
    _master: Box<dyn MasterPty + Send>,
    exit: Option<ChildExit>,
}

/// The writing half of a terminal, released exactly once.
///
/// Dropping the boxed writer is what sends EOF to the slave, so the option is
/// the release: [`PtyWriter::close`] takes it, and a second close finds nothing
/// left to take rather than erroring.
#[derive(Clone)]
pub(crate) struct PtyWriter {
    inner: Arc<StdMutex<Option<Box<dyn Write + Send>>>>,
}

impl PtyWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Arc::new(StdMutex::new(Some(writer))),
        }
    }

    /// Writes `bytes` to the terminal.
    ///
    /// The handle is blocking, so the write runs on a blocking task rather than
    /// on the runtime thread a slow reader could stall.
    pub(crate) async fn write_all(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        let inner = self.inner.clone();
        let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut guard = inner
                .lock()
                .map_err(|_| std::io::Error::other("the terminal writer lock is poisoned"))?;
            let writer = guard
                .as_mut()
                .ok_or_else(|| std::io::Error::other("the terminal writer is closed"))?;
            writer.write_all(&bytes)?;
            writer.flush()
        })
        .await;
        match written {
            Ok(result) => result,
            Err(error) => Err(std::io::Error::other(error.to_string())),
        }
    }

    /// Releases the writer, sending EOF to the child. A second call is a no-op.
    pub(crate) fn close(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.take();
        }
    }
}

/// Opens a terminal and starts `spec` under it.
pub(crate) fn spawn(spec: PtySpec<'_>) -> Result<(PtyTerminal, PtyStreams), String> {
    let size = PtySize {
        rows: dimension(spec.environment, "LINES", DEFAULT_ROWS),
        cols: dimension(spec.environment, "COLUMNS", DEFAULT_COLUMNS),
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|error| format!("no terminal could be opened: {error}"))?;
    let mut builder = CommandBuilder::new(spec.program);
    for argument in spec.arguments {
        builder.arg(argument);
    }
    builder.cwd(spec.working_directory);
    for (key, value) in spec.environment {
        builder.env(key, value);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| format!("no terminal child could be started: {error}"))?;
    // The slave stays open only for the child: holding a copy here would keep
    // the reader from ever seeing the terminal close.
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("the terminal cannot be read: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("the terminal cannot be written: {error}"))?;
    let group = child.process_id().and_then(|id| i32::try_from(id).ok());
    Ok((
        PtyTerminal {
            child,
            group,
            _master: pair.master,
            exit: None,
        },
        PtyStreams {
            reader,
            writer: PtyWriter::new(writer),
        },
    ))
}

/// How a terminal child ended.
///
/// `portable-pty` reports a signalled child as exit code 1 with the signal name
/// beside it, while the pipe backend reports no code at all. Dropping the code
/// for a signalled child is what keeps the two backends telling one story.
fn exit_of(status: portable_pty::ExitStatus) -> ChildExit {
    if status.signal().is_some() {
        return ChildExit {
            code: None,
            success: false,
        };
    }
    ChildExit {
        code: i32::try_from(status.exit_code()).ok(),
        success: status.success(),
    }
}

/// One dimension of the visible display, read from the environment the session
/// already carries so the terminal and the child agree on the size.
fn dimension(environment: &BTreeMap<String, String>, key: &str, fallback: u16) -> u16 {
    environment
        .get(key)
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

impl PtyTerminal {
    /// Reports the child's exit status without blocking.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        if let Some(exit) = self.exit {
            return Ok(Some(exit));
        }
        let status = self.child.try_wait()?;
        let exit = status.map(exit_of);
        self.exit = exit;
        Ok(exit)
    }

    /// Waits for the child to exit.
    ///
    /// `portable-pty` only offers a blocking wait, and the child cannot be moved
    /// onto a blocking task while the ladder still needs it, so the wait polls
    /// the non-blocking status at the same interval the process group does.
    pub(crate) async fn wait(&mut self) -> std::io::Result<ChildExit> {
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            tokio::time::sleep(GROUP_POLL_INTERVAL).await;
        }
    }

    /// Shuts the child down, escalating from `from` until it exits.
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
                Ok(Ok(exit)) => return Ok(exit),
                Ok(Err(source)) => return Err(TerminationError::Wait(source.to_string())),
                Err(_) => {}
            }
        }
        Err(TerminationError::Deadline)
    }

    /// Reaps descendants that outlived the child.
    ///
    /// The child leads its own process group, so a grandchild that ignored the
    /// leader's exit is still addressable here rather than left running.
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
            if !crate::child::group_alive(group)? {
                return Ok(());
            }
            if !already_signaled {
                self.signal(false)?;
            }
            if crate::child::wait_for_group_exit(group, grace).await? {
                return Ok(());
            }
            self.signal(true)?;
            if crate::child::wait_for_group_exit(group, grace).await? {
                return Ok(());
            }
            Err(TerminationError::Deadline)
        }
        #[cfg(windows)]
        {
            // ConPTY hands the child no job object, so the leader is the only
            // member this backend can address; the shutdown ladder confirms it.
            if !already_signaled {
                self.signal(true)?;
            }
            self.shut_down(grace, Rung::Wait).await.map(drop)
        }
    }

    /// Signals the whole group, falling back to the child when none is owned.
    pub(crate) fn signal(&mut self, force: bool) -> Result<(), TerminationError> {
        #[cfg(unix)]
        if let Some(group) = self.group {
            return crate::child::signal_group(group, force);
        }
        let _ = force;
        self.child
            .kill()
            .map_err(|error| TerminationError::Signal(error.to_string()))
    }
}
