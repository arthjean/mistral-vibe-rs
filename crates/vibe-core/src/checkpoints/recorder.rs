//! The write shell: the only part of the engine that reads disk.
//!
//! A turn's regions are the difference between what each touched path held
//! before the turn and what it holds after, so something has to decide when
//! those two readings happen. That is all this is, and the two rules it follows
//! are both about not losing a change:
//!
//! - A turn starts by re-reading every path the previous turn tracked. A tool
//!   that mutates a file without announcing it leaves no snapshot behind, and
//!   without this reading its change would be attributed to whichever later
//!   turn happened to touch the file next.
//! - A turn seals by reading each path on its own. One unreadable file skips
//!   itself and nothing else: sealing the rest under a single failed read would
//!   default their after states to their before states, and their changes would
//!   drop out of the log while still sitting on disk.
//!
//! Mirrors `vibe/core/checkpoints/recorder.py`. The reference's agent loop
//! shares one log between this shell and the read shells, and so does this
//! port: nothing here owns a log, every entry point takes the one its caller
//! owns.

use super::checkpointer::Checkpointer;
use super::files::{CheckpointFiles, FileAccessError, FileStore};
use super::lines::FileState;
use super::models::CheckpointError;

/// What went wrong driving a turn boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecorderError {
    /// The log refused the recording.
    #[error(transparent)]
    Log(#[from] CheckpointError),
    /// A path could not be read.
    #[error(transparent)]
    File(#[from] FileAccessError),
}

/// Drives the per-turn capture lifecycle over a log it does not own.
#[derive(Debug, Clone)]
pub struct CheckpointRecorder<F: CheckpointFiles> {
    files: FileStore<F>,
}

impl<F: CheckpointFiles> CheckpointRecorder<F> {
    /// A recorder reading and writing through `files`.
    pub const fn new(files: FileStore<F>) -> Self {
        Self { files }
    }

    /// The store this records through, for a caller that also needs to read a
    /// state or apply a plan.
    pub const fn files(&self) -> &FileStore<F> {
        &self.files
    }

    /// Opens a turn under `turn_id` and re-reads what the previous turn
    /// tracked.
    ///
    /// # Errors
    ///
    /// Fails when a turn is already open, leaving the log untouched, and when a
    /// carried path exists but cannot be read, leaving the turn open with what
    /// it had recorded so far.
    pub fn create_checkpoint(
        &self,
        log: &mut Checkpointer,
        turn_id: u64,
    ) -> Result<(), RecorderError> {
        let carried = log.history().last_turn_paths();
        log.begin_turn(turn_id)?;
        for path in carried {
            let state = self.files.read(&path)?;
            log.record_pre_edit(&path, state)?;
        }
        Ok(())
    }

    /// Records the state a tool read before it wrote to `path`.
    ///
    /// # Errors
    ///
    /// Fails when no turn is open.
    pub fn add_snapshot(
        &self,
        log: &mut Checkpointer,
        path: &str,
        state: FileState,
    ) -> Result<(), RecorderError> {
        log.record_pre_edit(path, state)?;
        Ok(())
    }

    /// Reads every tracked path and seals the turn.
    ///
    /// The seal happens whatever the reads answered, so a turn never stays open
    /// because a file went unreadable while it ran. The paths that could not be
    /// read are returned rather than swallowed: their change this turn is not
    /// in the log, and the caller is the only one that can say so.
    pub fn seal_turn(&self, log: &mut Checkpointer) -> Vec<FileAccessError> {
        let mut failures = Vec::new();
        for path in log.history().last_turn_paths() {
            match self.files.read(&path) {
                Ok(state) => {
                    // The log is open here by construction, and a refusal would
                    // mean the turn closed under us, which sealing handles.
                    let _ = log.record_post_edit(&path, state);
                }
                Err(error) => failures.push(error),
            }
        }
        log.seal_turn();
        failures
    }
}
