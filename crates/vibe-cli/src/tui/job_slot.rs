//! The one-job slot a background worker thread drains: a job submitted before
//! the worker took the previous one replaces it, so only the latest is ever
//! run.

use std::sync::{Condvar, Mutex, PoisonError};

use vibe_core::sync::lock;

#[derive(Debug)]
pub(super) struct LatestJobSlot<T> {
    state: Mutex<SlotState<T>>,
    ready: Condvar,
}

#[derive(Debug)]
struct SlotState<T> {
    pending: Option<T>,
    shutdown: bool,
}

impl<T> LatestJobSlot<T> {
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(SlotState {
                pending: None,
                shutdown: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Replaces the job waiting, if any, and wakes the worker.
    pub(super) fn submit(&self, job: T) {
        lock(&self.state).pending = Some(job);
        self.ready.notify_one();
    }

    /// Drops the job waiting, if any.
    pub(super) fn cancel(&self) {
        lock(&self.state).pending = None;
    }

    /// Drops the job waiting and wakes the worker to exit.
    pub(super) fn shutdown(&self) {
        let mut state = lock(&self.state);
        state.shutdown = true;
        state.pending = None;
        self.ready.notify_one();
    }

    /// Blocks the worker until a job arrives, or returns `None` once the slot
    /// is shut down.
    pub(super) fn wait(&self) -> Option<T> {
        let mut state = self
            .ready
            .wait_while(lock(&self.state), |state| {
                state.pending.is_none() && !state.shutdown
            })
            .unwrap_or_else(PoisonError::into_inner);
        if state.shutdown {
            return None;
        }
        state.pending.take()
    }
}
