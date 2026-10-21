//! The per-session directory a tool may always write to.
//!
//! Reference `vibe/core/scratchpad.py` opens one temporary directory per
//! agent-loop runtime and names it `vibe-scratchpad-<short session id>-<random>`.
//! Every file tool consults it first: a path inside it is granted outright,
//! before any list and before the working-directory boundary, because it is the
//! runtime's own capability rather than the operator's workspace.
//!
//! A [`Scratchpad`] is that directory as its runtime owns it: opened once,
//! and removed with everything in it when the runtime lets go of it, as
//! reference `cleanup_scratchpad` does on every agent-loop shutdown. A session
//! resumed by another runtime opens a new one.

use std::path::{Path, PathBuf};

use crate::storage::short_session_id;

/// The prefix the reference names every scratchpad with.
pub const SCRATCHPAD_PREFIX: &str = "vibe-scratchpad-";

/// How many random names an opening tries, each one found taken, before it
/// gives up.
const NAME_ATTEMPTS: usize = 16;

/// One runtime's scratchpad, removed with everything in it when dropped.
#[derive(Debug)]
pub struct Scratchpad {
    path: PathBuf,
}

impl Scratchpad {
    /// Opens a new scratchpad for `session_id`, or [`None`] when the
    /// filesystem refuses.
    ///
    /// Reference `init_scratchpad`: a `tempfile.mkdtemp` under the system
    /// temporary directory, private to its owner and named after the short
    /// session identifier plus a random part, so two runtimes of one session
    /// never share it. It answers `None` on an `OSError` rather than failing
    /// the runtime: a session without a scratchpad still runs, it just has no
    /// directory that skips the permission chain.
    #[must_use]
    pub fn open(session_id: &str) -> Option<Self> {
        let parent = std::env::temp_dir();
        let short = short_session_id(session_id);
        for _ in 0..NAME_ATTEMPTS {
            let random = crate::session_id::random_hex::<4>();
            let path = parent.join(format!("{SCRATCHPAD_PREFIX}{short}-{random}"));
            match crate::atomic_file::create_private_directory(&path) {
                Ok(()) => return Some(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratchpad {
    /// Reference `cleanup_scratchpad`, which swallows both a missing directory
    /// and a failure to remove one.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
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
    let Ok(root) = crate::platform::canonicalize(scratchpad) else {
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
