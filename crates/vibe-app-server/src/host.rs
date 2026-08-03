//! The ambient host facts the app-server depends on: the wall clock and the
//! Vibe home directory.
//!
//! Both used to be re-derived in every module that needed them, with clock
//! failures handled three different ways. A clock that predates the epoch is
//! reported as `0` so a broken host degrades timestamps instead of failing
//! requests.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn since_epoch() -> Option<std::time::Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

#[must_use]
pub(crate) fn now_millis() -> u64 {
    since_epoch().map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

#[must_use]
pub(crate) fn now_seconds() -> u64 {
    since_epoch().map_or(0, |elapsed| elapsed.as_secs())
}

/// Resolves the Vibe home directory, preferring an explicit `VIBE_HOME` over
/// the user's home and falling back to a workspace-local directory.
#[must_use]
pub(crate) fn vibe_home() -> PathBuf {
    std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibe"))
        })
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".vibe")))
        .unwrap_or_else(|| PathBuf::from(".vibe"))
}
