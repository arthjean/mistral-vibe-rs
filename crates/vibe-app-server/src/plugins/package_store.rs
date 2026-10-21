//! The content-addressed package checkouts a session's plugins run from.
//!
//! Reference `PluginPackageStore`
//! (`harness/runtimes/python/.../vibe/plugins/_store.py`), whose blob and
//! manifest layers this port does not keep (row 36 owns the unified session
//! storage that reads them back).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vibe_core::platform::resolve_lenient;
use vibe_core::plugins::content::{digest_plugin_tree, launched_executables};

/// The package checkouts and the per-session `${PLUGIN_DATA}` roots under one
/// session storage root.
pub(super) struct PackageStore {
    root: PathBuf,
}

impl PackageStore {
    pub(super) fn new(storage_root: &Path) -> Self {
        Self {
            root: resolve_lenient(storage_root).join("plugins"),
        }
    }

    pub(super) fn data_root(&self, session_id: &str) -> PathBuf {
        self.root.join("data").join(session_id)
    }

    /// The checkout of `source` at `packages/<ab>/<digest>`, copied the first
    /// time and verified against `digest`.
    pub(super) fn checkout(
        &self,
        source: &Path,
        digest: &str,
        ignored: &BTreeSet<String>,
    ) -> io::Result<PathBuf> {
        let shard = digest.get(..2).unwrap_or(digest);
        let target = self.root.join("packages").join(shard).join(digest);
        if target.is_dir() {
            return Ok(target);
        }
        let temporary_root = self.root.join("tmp");
        fs::create_dir_all(&temporary_root)?;
        let staging = temporary_root.join(format!(
            "package-{}-{}",
            digest.get(..16).unwrap_or(digest),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&staging);
        let outcome = copy_package(source, &staging, ignored).and_then(|()| {
            let rebuilt = digest_plugin_tree(&staging, &BTreeSet::new())?;
            if rebuilt != digest {
                return Err(io::Error::other(format!(
                    "the copied package digests to {rebuilt}, not {digest}"
                )));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            match fs::rename(&staging, &target) {
                Err(_) if target.is_dir() => Ok(()),
                Err(error) => Err(error),
                Ok(()) => harden(&target, &launched_executables(&target)),
            }
        });
        let _ = fs::remove_dir_all(&staging);
        outcome.map(|()| target)
    }
}

/// Leaves a checkout readable by its owner alone and writable by nobody:
/// files `0400`, directories `0500`, as the reference's store leaves them.
/// Only the files the package's own `mcp.json` launches with a `./` command
/// stay runnable, at `0500`, as the reference's executable blobs are.
#[cfg(unix)]
fn harden(root: &Path, executables: &BTreeSet<PathBuf>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            harden(&entry.path(), executables)?;
        } else if kind.is_file() {
            let mode = if executables.contains(&entry.path()) {
                0o500
            } else {
                0o400
            };
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode))?;
        }
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o500))
}

#[cfg(not(unix))]
fn harden(_root: &Path, _executables: &BTreeSet<PathBuf>) -> io::Result<()> {
    Ok(())
}

/// Copies a package tree, keeping symbolic links as links and leaving out
/// every entry an ignored name covers.
fn copy_package(source: &Path, target: &Path, ignored: &BTreeSet<String>) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let mut entries: Vec<_> = fs::read_dir(source)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if ignored.contains(name.to_string_lossy().as_ref()) {
            continue;
        }
        let path = entry.path();
        let destination = target.join(&name);
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            symlink(&fs::read_link(&path)?, &destination)?;
        } else if kind.is_dir() {
            copy_package(&path, &destination, ignored)?;
        } else if kind.is_file() {
            fs::copy(&path, &destination)?;
        } else {
            return Err(io::Error::other(format!(
                "{} is neither a file nor a symbolic link",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}
