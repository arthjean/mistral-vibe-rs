//! The ambient host facts the app-server depends on: the wall clock and the
//! Vibe home directory.
//!
//! Both used to be re-derived in every module that needed them. The clock now
//! lives in [`vibe_core::clock`], where every layer reads it; this module
//! re-exports it so call sites keep naming one host surface.

use std::path::{Path, PathBuf};

pub(crate) use vibe_core::clock::{now_millis, now_seconds};

/// Replaces a leading `~` with the user's home directory, as the reference
/// `Path.expanduser` does. A path that needs a home directory this host cannot
/// resolve is returned unchanged, so the caller reports the path it was given
/// rather than a silently different one.
#[must_use]
pub(crate) fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return path.to_path_buf();
    };
    if first.as_os_str() != "~" {
        return path.to_path_buf();
    }
    let Some(mut home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    else {
        return path.to_path_buf();
    };
    home.extend(components);
    home
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
