//! The checkpoint engine, starting with the two things every later part of it
//! is addressed in: lines and opcodes.
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
//! Nothing here reads or writes a file: a [`FileState`] is bytes the caller
//! already holds, which is the split that lets the read model be exercised
//! without a filesystem.

mod lines;
mod matcher;

pub use lines::{FileState, decode_lines, split_lines};
pub use matcher::{Match, Opcode, SequenceMatcher, Tag};

#[cfg(test)]
mod checkpoint_parity_tests;
#[cfg(test)]
mod lines_tests;
#[cfg(test)]
mod matcher_tests;
