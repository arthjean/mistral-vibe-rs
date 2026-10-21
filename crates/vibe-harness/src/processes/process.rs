//! One background process: its lifecycle, the output it has written, and the
//! signals that end it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_core::sync::lock;

use super::Status;

/// Output kept per process; older bytes are dropped from the front.
const RETAINED: usize = 64 * 1024 * 1024;
/// How long a stop waits after each signal.
const TERMINATION_WAIT: Duration = Duration::from_secs(2);
/// How often a waiting read looks again.
const POLL: Duration = Duration::from_millis(250);

/// What a process has written so far.
#[derive(Default)]
pub(super) struct Output {
    /// The retained bytes, starting at `start`.
    bytes: Vec<u8>,
    start: u64,
    /// Whether the terminal has closed and nothing more will come.
    pub(super) finished: bool,
}

impl Output {
    /// The output of a process that will write nothing more.
    pub(super) fn finished() -> Self {
        Self {
            finished: true,
            ..Self::default()
        }
    }

    fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }
}

pub(super) struct Lifecycle {
    pub(super) status: Status,
    pub(super) exit_code: Option<i64>,
    /// A stop is under way: an exit from now on counts as stopped.
    pub(super) stopping: bool,
}

/// One process.
pub(super) struct Process {
    pub(super) id: String,
    pub(super) command: String,
    pub(super) created_at: u64,
    pub(super) lifecycle: Mutex<Lifecycle>,
    pub(super) changed: Condvar,
    pub(super) output: Mutex<Output>,
    pub(super) writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// The process group, the root's own ID, which on Windows names the tree
    /// a stop ends.
    pub(super) group: Option<i32>,
    pub(super) killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    pub(super) directory: Option<PathBuf>,
}

/// One page of a process's output.
pub(super) struct Page {
    pub(super) bytes: Vec<u8>,
    pub(super) start: u64,
    pub(super) next: u64,
    pub(super) end: u64,
    pub(super) has_more: bool,
    pub(super) truncated_before: bool,
}

impl Process {
    pub(super) fn state(&self) -> (Status, Option<i64>) {
        let lifecycle = lock(&self.lifecycle);
        (lifecycle.status, lifecycle.exit_code)
    }

    pub(super) fn append(&self, bytes: &[u8]) {
        let mut output = lock(&self.output);
        let first = output.bytes.is_empty() && output.start == 0;
        output.bytes.extend_from_slice(bytes);
        if output.bytes.len() > RETAINED {
            let dropped = output.bytes.len() - RETAINED;
            output.bytes.drain(..dropped);
            output.start += dropped as u64;
        }
        drop(output);
        if let Some(directory) = &self.directory {
            let segments = directory.join("segments");
            if first {
                let _ = std::fs::create_dir_all(&segments);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(segments.join(format!("{:020}.bin", 0)))
            {
                let _ = file.write_all(bytes);
            }
        }
        self.changed.notify_all();
    }

    /// Writes `output-state.json` beside the segments.
    pub(super) fn persist_output_state(&self) {
        let Some(directory) = &self.directory else {
            return;
        };
        let (start, end) = {
            let output = lock(&self.output);
            (output.start, output.end())
        };
        let finalized = self.state().0 != Status::Running;
        let state = json!({
            "finalized": finalized,
            "output_available": true,
            "output_end_cursor": end,
            "output_start_cursor": start,
            "output_state_version": 1,
            "process_id": self.id,
        });
        let _ = std::fs::write(
            directory.join("output-state.json"),
            format!("{}\n", serde_json::to_string(&state).unwrap_or_default()),
        );
    }

    /// Waits until the process ends or `wait` passes.
    pub(super) fn settle(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let mut lifecycle = lock(&self.lifecycle);
        while lifecycle.status == Status::Running {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            lifecycle = self
                .changed
                .wait_timeout(lifecycle, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Waits until bytes lie past `cursor`, the process ends, or `wait`
    /// passes.
    pub(super) fn wait_past(&self, cursor: u64, wait: Duration) {
        let deadline = Instant::now() + wait;
        loop {
            let past = lock(&self.output).end() > cursor;
            if past || self.state().0 != Status::Running || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    pub(super) fn page(&self, from_end: bool, cursor: u64, limit: usize) -> Result<Page, Value> {
        let output = lock(&self.output);
        let (start, end) = (output.start, output.end());
        let limit = limit as u64;
        let (read_start, read_end) = if from_end {
            let read_start = start.max(end.saturating_sub(limit));
            (read_start, end)
        } else {
            if cursor > end {
                return Err(json!({
                    "processId": self.id,
                    "cursor": cursor,
                    "outputStartCursor": start,
                    "bytesAvailable": end,
                }));
            }
            let read_start = cursor.max(start);
            (read_start, end.min(read_start + limit))
        };
        let offset = |at: u64| usize::try_from(at - start).unwrap_or_default();
        let bytes = output.bytes[offset(read_start)..offset(read_end)].to_vec();
        let next = if from_end {
            end
        } else {
            read_start + bytes.len() as u64
        };
        Ok(Page {
            has_more: if from_end {
                read_start > start
            } else {
                next < end
            },
            truncated_before: if from_end { start > 0 } else { cursor < start },
            bytes,
            start,
            next,
            end,
        })
    }

    /// Ends the process group: a polite signal, then a forced one.
    pub(super) fn terminate(&self) {
        {
            let mut lifecycle = lock(&self.lifecycle);
            if lifecycle.status != Status::Running {
                return;
            }
            lifecycle.stopping = true;
        }
        for force in [false, true] {
            self.signal(force);
            let deadline = Instant::now() + TERMINATION_WAIT;
            let mut lifecycle = lock(&self.lifecycle);
            while lifecycle.status == Status::Running {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                lifecycle = self
                    .changed
                    .wait_timeout(lifecycle, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            if lifecycle.status != Status::Running {
                return;
            }
        }
    }

    #[cfg(unix)]
    fn signal(&self, force: bool) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let signal = if force {
            Signal::SIGKILL
        } else {
            Signal::SIGTERM
        };
        let sent = self
            .group
            .is_some_and(|group| killpg(Pid::from_raw(group), signal).is_ok());
        if !sent && let Some(killer) = lock(&self.killer).as_mut() {
            let _ = killer.kill();
        }
    }

    /// Reference `request_termination` forces on Windows too: the root's whole
    /// tree, then the root itself should the tree kill miss it.
    #[cfg(windows)]
    fn signal(&self, _force: bool) {
        if let Some(pid) = self.group.and_then(|pid| u32::try_from(pid).ok()) {
            let _ = vibe_core::pty::kill_tree(pid);
        }
        if let Some(killer) = lock(&self.killer).as_mut() {
            let _ = killer.kill();
        }
    }
}
