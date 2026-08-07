//! The checkpoint engine: an append-only log of what happened to the tracked
//! files, and a read model that turns it into reviewable changes.
//!
//! [`Checkpointer`] owns the log and its lifecycle, and [`History`] answers
//! every question about it: which regions a file carries, what each was built
//! on, what decision is in force after dragging, what the file looks like with
//! those decisions applied, and where each pending change sits in a rendered
//! diff. Neither reads or writes a file. A caller feeds in the bytes it read
//! and applies the bytes it gets back, which is what lets the whole model be
//! exercised without a filesystem.
//!
//! The two halves below are what all of that is addressed in: lines and
//! opcodes.
//!
//! A region identity in this engine is a pair of a producing edit's sequence
//! number and that region's position in the edit's opcode list. Both halves are
//! sent back by a client across turns, so a different opcode sequence is a
//! different contract, not a different rendering. That makes the diff algorithm
//! a public boundary even though nothing outside this crate ever calls it
//! directly, and it is why the matcher lands before the log it will feed.
//!
//! The reference computes every region, anchor and dependency edge from
//! CPython's `difflib.SequenceMatcher` constructed with `autojunk=False`, over
//! lines produced by `str.splitlines(keepends=True)` on text `decode_safe` has
//! already normalized to `\n`. [`matcher`] reproduces the first from the
//! published CPython algorithm rather than from reference source, and [`lines`]
//! the second. `crates/vibe-core/tests/checkpoints/opcodes.json` holds what the
//! pinned reference answered for a set of fixtures, and
//! `checkpoint_parity_tests` replays it unconditionally.
//!
//! Nothing in the log or the read model reads or writes a file: a [`FileState`]
//! is bytes the caller already holds, which is the split that lets the read
//! model be exercised without a filesystem. The two parts that do need disk are
//! kept apart from both and reach it through one port: [`FileStore`] reads a
//! state and applies a restore plan, and [`CheckpointRecorder`] decides when a
//! turn's two readings happen. Neither knows what a workspace is.

mod checkpointer;
mod events;
mod files;
mod history;
mod lines;
mod matcher;
mod models;
mod recorder;

pub use checkpointer::Checkpointer;
pub use files::{CheckpointFiles, FileAccessError, FileStore, RestoreOutcome};
pub use history::History;
pub use lines::{FileState, decode_lines, split_lines};
pub use matcher::{Match, Opcode, SequenceMatcher, Tag};
pub use models::{
    Change, CheckpointError, Decision, HunkAnchor, HunkSide, OpaqueChange, OpaqueReason, Owner,
    Region, RegionId, TurnRegion,
};
pub use recorder::{CheckpointRecorder, RecorderError};

#[cfg(test)]
mod checkpoint_parity_tests;
#[cfg(test)]
mod checkpointer_tests;
#[cfg(test)]
mod files_tests;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod lines_tests;
#[cfg(test)]
mod matcher_tests;
#[cfg(test)]
mod models_tests;
#[cfg(test)]
mod recorder_tests;
