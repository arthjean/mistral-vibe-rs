//! Canonical durable-write primitives.
//!
//! Every persisted artifact in this crate (session metadata, message logs,
//! configuration, transaction journals, pointers) needs the same guarantee: an
//! interrupted process must leave either the previous content or the new one,
//! never a half-written file. The recipe is always identical, so it lives here
//! once instead of being restated at each call site.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Creates a file that must not already exist, restricted to the current user.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path)
}

/// Opens (creating if needed) a file usable as an advisory lock.
pub(crate) fn open_private_lock(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path)
}

/// An advisory exclusive lock on `path`, released when the value is dropped.
///
/// Two subsystems serialize their durable writes this way (the session store
/// and the configuration store), and both need the same three things: a private
/// lock file, an exclusive hold, and a release that survives an early return.
/// Owning the guard here is what keeps the second one from being a slightly
/// different copy of the first.
///
/// Errors surface as [`std::io::Error`] so each caller maps them into its own
/// error type; a caller that wants to distinguish "already held" reads
/// [`std::io::ErrorKind::WouldBlock`] off [`FileLock::try_acquire`].
pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    /// Blocks until the lock is available.
    pub(crate) fn acquire(path: &Path) -> std::io::Result<Self> {
        use fs2::FileExt as _;

        let file = open_private_lock(path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }

    /// Fails with [`std::io::ErrorKind::WouldBlock`] rather than waiting when
    /// another holder owns the lock.
    pub(crate) fn try_acquire(path: &Path) -> std::io::Result<Self> {
        use fs2::FileExt as _;

        let file = open_private_lock(path)?;
        file.try_lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Creates `path` and every missing parent, restricted to the current user.
pub(crate) fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_permissions(path)
}

/// Creates `path`, failing if it already exists, restricted to the current user.
pub(crate) fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)?;
    set_private_permissions(path)
}

fn set_private_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Flushes a directory entry so a preceding rename survives a crash.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path).and_then(|directory| directory.sync_all())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Reserves a unique sidecar path next to `destination`.
///
/// The name is process-unique, so two concurrent writers of the same
/// destination never collide on their staging file.
pub(crate) fn temporary_sibling(
    destination: &Path,
    prefix: &str,
) -> std::io::Result<std::path::PathBuf> {
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".{prefix}.{sequence}.tmp")))
}

/// Writes `bytes` to `destination` atomically and durably.
///
/// Content is staged in a private sibling file, flushed, renamed over the
/// destination, and the containing directory is flushed in turn. A failure at
/// any step removes the staging file and leaves `destination` untouched.
///
/// The error carries the path that actually failed, which the caller maps into
/// its own error type.
pub(crate) fn write_atomically(
    destination: &Path,
    prefix: &str,
    bytes: &[u8],
) -> Result<(), AtomicWriteError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AtomicWriteError::no_parent(destination))?;
    let temporary = temporary_sibling(destination, prefix).map_err(|source| AtomicWriteError {
        path: destination.to_path_buf(),
        source,
    })?;
    let result = (|| {
        let mut file = create_private_file(&temporary).map_err(|source| AtomicWriteError {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| AtomicWriteError {
                path: temporary.clone(),
                source,
            })?;
        drop(file);
        fs::rename(&temporary, destination).map_err(|source| AtomicWriteError {
            path: destination.to_path_buf(),
            source,
        })?;
        sync_directory(parent).map_err(|source| AtomicWriteError {
            path: parent.to_path_buf(),
            source,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// An I/O failure together with the path that produced it.
#[derive(Debug)]
pub(crate) struct AtomicWriteError {
    pub(crate) path: std::path::PathBuf,
    pub(crate) source: std::io::Error,
}

impl AtomicWriteError {
    fn no_parent(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_replace_content_without_leaving_sidecars() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("value.txt");
        write_atomically(&path, "value", b"first").expect("first write");
        write_atomically(&path, "value", b"second").expect("second write");

        assert_eq!(
            std::fs::read_to_string(&path).expect("persisted content"),
            "second"
        );
        let leftovers = std::fs::read_dir(directory.path())
            .expect("directory listing")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_missing_parent_directory_fails_and_creates_nothing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("missing");
        let path = parent.join("value.txt");

        let error = write_atomically(&path, "value", b"content").expect_err("missing parent");

        assert_eq!(error.path.parent(), Some(parent.as_path()));
        assert!(!parent.exists());
    }
}
