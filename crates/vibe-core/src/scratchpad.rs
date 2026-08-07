//! The per-session directory a tool may always write to.
//!
//! Reference `vibe/core/scratchpad.py` opens one temporary directory per
//! agent-loop runtime and names it `vibe-scratchpad-<short session id>-<random>`.
//! Every file tool consults it first: a path inside it is granted outright,
//! before any list and before the working-directory boundary, because it is the
//! runtime's own capability rather than the operator's workspace.
//!
//! This port derives the name from the session identifier alone rather than
//! from a random suffix, so a resumed session finds the directory the previous
//! process left and the two halves of one conversation share a scratchpad. The
//! prefix and the parent directory are the reference's.

use std::path::{Path, PathBuf};

/// The prefix the reference names every scratchpad with.
pub const SCRATCHPAD_PREFIX: &str = "vibe-scratchpad-";

/// Where `session_id`'s scratchpad lives, whether or not it exists yet.
#[must_use]
pub fn scratchpad_path(session_id: &str) -> PathBuf {
    let short = session_id.chars().take(8).collect::<String>();
    std::env::temp_dir().join(format!("{SCRATCHPAD_PREFIX}{short}"))
}

/// Opens `session_id`'s scratchpad, or [`None`] when the filesystem refuses.
///
/// Reference `init_scratchpad` answers `None` on an `OSError` rather than
/// failing the runtime: a session without a scratchpad still runs, it just has
/// no directory that skips the permission chain.
#[must_use]
pub fn init_scratchpad(session_id: &str) -> Option<PathBuf> {
    let path = scratchpad_path(session_id);
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

/// Removes `scratchpad` and everything in it, ignoring an absent directory.
///
/// Reference `cleanup_scratchpad`, which swallows both a missing directory and
/// a failure to remove one.
pub fn cleanup_scratchpad(scratchpad: Option<&Path>) {
    if let Some(path) = scratchpad {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Whether `path` sits inside `scratchpad`.
///
/// Reference `is_scratchpad_path` resolves both sides and answers `false` for
/// anything it cannot resolve, so an unresolvable path never buys the grant.
#[must_use]
pub fn is_scratchpad_path(path: &Path, scratchpad: Option<&Path>) -> bool {
    let Some(scratchpad) = scratchpad else {
        return false;
    };
    let Ok(root) = scratchpad.canonicalize() else {
        return false;
    };
    // A file that does not exist yet still belongs to the scratchpad when the
    // directory it would be created in does, which is what a write into it
    // needs. The resolution is the policy's own, so a `..` hidden behind a
    // missing directory cannot buy the grant here either.
    crate::policy::canonicalize_for_policy(path).is_ok_and(|resolved| resolved.starts_with(&root))
}

#[cfg(test)]
mod scratchpad_tests;
