//! The port the engine reaches disk through, and the store that speaks it.
//!
//! The engine itself never reads or writes a file: it takes the bytes a caller
//! read and hands back the bytes a caller should write. That split is what lets
//! every rule in [`super::history`] be exercised without a filesystem, and it
//! only holds if the two operations the engine does need, reading a state and
//! applying a plan, sit behind an interface rather than behind a concrete
//! directory.
//!
//! One distinction carries the port. A path that does not exist reads as an
//! absent state, and a path that exists but cannot be read fails. Collapsing
//! the two would report a file whose permissions changed as deleted, and the
//! engine would record a deletion nobody performed and offer to revert it.
//!
//! Mirrors `vibe/core/checkpoints/fs.py` and
//! `vibe/core/checkpoints/file_store.py`.

use super::lines::FileState;

/// A file operation that did not succeed, named by the path it was reaching
/// for.
///
/// The cause is carried as text rather than as a source error because the
/// implementations behind this port answer with unrelated error types, and
/// every consumer of this one either logs it or reports it to a client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{operation} {path}: {cause}")]
pub struct FileAccessError {
    /// What was being attempted, phrased to read after the error's subject.
    pub operation: &'static str,
    /// The path the operation named.
    pub path: String,
    /// Why it failed.
    pub cause: String,
}

impl FileAccessError {
    /// A failure reaching `path` while doing `operation`.
    pub fn new(operation: &'static str, path: impl Into<String>, cause: impl ToString) -> Self {
        Self {
            operation,
            path: path.into(),
            cause: cause.to_string(),
        }
    }
}

/// The disk operations the checkpoint store needs.
///
/// An implementation confines every path it is handed; nothing here validates
/// one, because the engine's paths are the ones its caller already resolved.
pub trait CheckpointFiles {
    /// The bytes at `path`, or [`None`] when nothing is there.
    ///
    /// # Errors
    ///
    /// Fails when the path exists but cannot be read, which is never the same
    /// answer as absence.
    fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>, FileAccessError>;

    /// Writes `data` to `path`, creating what it needs to.
    ///
    /// # Errors
    ///
    /// Fails when the write does not land.
    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), FileAccessError>;

    /// Deletes `path`.
    ///
    /// # Errors
    ///
    /// Fails when the deletion does not land.
    fn remove(&self, path: &str) -> Result<(), FileAccessError>;

    /// Whether anything is at `path`.
    fn exists(&self, path: &str) -> bool;
}

/// What applying a plan did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// One entry per path the plan could not put in the state it asked for.
    pub errors: Vec<FileAccessError>,
    /// The paths whose content actually changed, in plan order.
    pub restored: Vec<String>,
}

/// Reads checkpoint states and writes restore plans, through a port.
#[derive(Debug, Clone)]
pub struct FileStore<F: CheckpointFiles> {
    files: F,
}

impl<F: CheckpointFiles> FileStore<F> {
    /// A store over `files`.
    pub const fn new(files: F) -> Self {
        Self { files }
    }

    /// The state `path` holds now.
    ///
    /// # Errors
    ///
    /// Fails when the path exists but cannot be read.
    pub fn read(&self, path: &str) -> Result<FileState, FileAccessError> {
        Ok(self
            .files
            .read_bytes(path)?
            .map_or_else(FileState::absent, FileState::from_bytes))
    }

    /// Puts every path of `plan` in the state the plan names.
    ///
    /// An absent target is deleted, a present one is written, and a path
    /// already holding its target is left alone so its modification time does
    /// not move. Every path is attempted: one failure is reported and the rest
    /// still land, because a plan that stopped halfway would leave the
    /// workspace in a state no version of it ever had.
    ///
    /// A path whose current content cannot be read is written rather than
    /// skipped: the check exists to avoid a pointless write, and failing it is
    /// not evidence that the write is pointless.
    pub fn apply(&self, plan: &[(String, FileState)]) -> RestoreOutcome {
        let mut outcome = RestoreOutcome::default();
        for (path, target) in plan {
            match target.data() {
                None => {
                    if !self.files.exists(path) {
                        continue;
                    }
                    match self.files.remove(path) {
                        Ok(()) => outcome.restored.push(path.clone()),
                        Err(error) => outcome.errors.push(error),
                    }
                }
                Some(bytes) => {
                    if self.read(path).is_ok_and(|current| current == *target) {
                        continue;
                    }
                    match self.files.write_bytes(path, bytes) {
                        Ok(()) => outcome.restored.push(path.clone()),
                        Err(error) => outcome.errors.push(error),
                    }
                }
            }
        }
        outcome
    }

    /// The paths of `plan` whose content would actually change.
    ///
    /// A path that cannot be read counts as differing, on the same reasoning
    /// [`Self::apply`] writes it.
    pub fn diverging_paths(&self, plan: &[(String, FileState)]) -> Vec<String> {
        plan.iter()
            .filter(|(path, target)| !self.read(path).is_ok_and(|current| current == *target))
            .map(|(path, _target)| path.clone())
            .collect()
    }
}
