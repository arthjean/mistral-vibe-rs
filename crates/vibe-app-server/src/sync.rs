//! The one policy this crate's own state locks follow when a holder panics.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Takes `mutex`, recovering it from a holder that panicked.
///
/// Every holder of these locks makes one whole change under the guard, so
/// what a panicking holder left is still a state to act on, and a service
/// that refused every later call over one panic would strand every session it
/// serves.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
