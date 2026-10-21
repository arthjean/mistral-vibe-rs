//! A temporary root a test opens a vibe home under, removed whole.
//!
//! A session start pins the builtin plugins into the package store under
//! `<vibe home>/logs/session/plugins/packages`, whose checkouts it leaves
//! writable by nobody, directories `0500` (`harden` in
//! `crates/vibe-app-server/src/plugins/package_store.rs`, as reference
//! `PluginPackageStore` does). The entries of a directory without its write bit
//! cannot be removed, and `TempDir` ignores that failure, so the tree would
//! outlive the test. [`TestRoot`] gives every directory under it its owner's
//! bits back first.
//!
//! The `vibe-cli` and `vibe-app-server` unit tests and the `vibe-acp`
//! integration tests compile this file too, through a `#[path]` attribute.

#![allow(
    dead_code,
    reason = "every test binary compiles this file and uses a part of it"
)]

use std::path::Path;

/// A [`tempfile::TempDir`] whose hardened subtrees are relaxed before it is
/// removed.
pub struct TestRoot(tempfile::TempDir);

/// A new [`TestRoot`], as [`tempfile::tempdir`] opens one.
pub fn tempdir() -> std::io::Result<TestRoot> {
    tempfile::tempdir().map(TestRoot)
}

impl TestRoot {
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        relax(self.0.path());
    }
}

/// Gives every directory under `root` its owner's read, write and search
/// bits back, without following a symbolic link.
pub fn relax(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let Ok(metadata) = std::fs::symlink_metadata(root) else {
            return;
        };
        if !metadata.is_dir() {
            return;
        }
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode | 0o700));
        }
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                relax(&entry.path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
}
