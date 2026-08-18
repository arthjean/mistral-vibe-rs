//! The wall clock, read in one place.
//!
//! Every layer needs the current time and every layer used to re-derive it,
//! with clock failures handled a different way at each site. A host whose clock
//! predates the epoch reports `0` here, so a broken clock degrades timestamps
//! instead of failing the operation that asked for one.

use std::time::{SystemTime, UNIX_EPOCH};

fn since_epoch() -> Option<std::time::Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_millis() -> u64 {
    since_epoch().map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn now_seconds() -> u64 {
    since_epoch().map_or(0, |elapsed| elapsed.as_secs())
}

/// Seconds since the Unix epoch, for the interfaces that carry a signed stamp.
#[must_use]
pub fn now_seconds_signed() -> i64 {
    i64::try_from(now_seconds()).unwrap_or(i64::MAX)
}

/// Nanoseconds since the Unix epoch.
///
/// Callers use it to make a temporary file name or an operation id unique
/// rather than to tell the time, so a host whose clock predates the epoch
/// yields `0` and leaves uniqueness to the caller's own suffix.
#[must_use]
pub fn now_nanos() -> u128 {
    since_epoch().map_or(0, |elapsed| elapsed.as_nanos())
}
