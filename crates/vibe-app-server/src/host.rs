//! The ambient host facts the app-server depends on: the wall clock and the
//! Vibe home directory.
//!
//! Both used to be re-derived in every module that needed them. The clock now
//! lives in [`vibe_core::clock`], where every layer reads it; this module
//! re-exports it so call sites keep naming one host surface.

use std::path::PathBuf;

pub(crate) use vibe_core::clock::{now_millis, now_seconds};

/// `Path.expanduser`: a leading `~` is the user's home directory. A path that
/// needs a home directory this host cannot resolve is returned unchanged, so
/// the caller reports the path it was given rather than a silently different
/// one.
pub(crate) use vibe_core::platform::expand_home;

/// The Vibe home directory, from the resolver every entry point shares
/// ([`vibe_core::platform::vibe_home`]); the binary hosting this server has
/// already refused to start where it cannot be placed.
#[must_use]
pub(crate) fn vibe_home() -> PathBuf {
    let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    vibe_core::platform::vibe_home_or_local(&working_directory)
}

#[cfg(test)]
mod host_tests;
