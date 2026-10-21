//! The one policy the workspace's in-memory state locks follow when a holder
//! panics.
//!
//! A holder makes one whole change under the guard, so what a panicking
//! holder left behind is still a state to act on. A panic already failed the
//! task that raised it; refusing every later call over it, or silently
//! skipping the update a caller asked for, would only spread that failure to
//! work that did nothing wrong. Every lock of this workspace is therefore
//! taken through these functions, which recover the guard. A critical section
//! that can leave its data half-updated documents its own refusal at the lock
//! instead (`vibe_app_server::projects::loops` is the one such site).

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Takes `mutex`, recovering it from a holder that panicked.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads `lock`, recovering it from a writer that panicked.
pub fn read<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Writes `lock`, recovering it from a holder that panicked.
pub fn write<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
