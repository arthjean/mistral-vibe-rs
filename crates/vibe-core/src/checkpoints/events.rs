//! The three things the log records, and the ordered map a turn collects paths
//! in.
//!
//! Nothing outside this module ever sees an event. A caller appends through the
//! log's lifecycle and reads through the read model, which is what keeps the
//! two orders that matter honest: list order, which is when things happened,
//! and sequence order, which is what a region identity is numbered by. A drift
//! captured mid-turn is sealed before the turn it preceded while carrying a
//! higher sequence than that turn's mark, so the two orders are not the same
//! relation and neither one can stand in for the other.
//!
//! Mirrors `vibe/core/checkpoints/_events.py`.

use super::lines::FileState;
use super::models::{Owner, RegionId};
use std::collections::BTreeMap;

/// A turn boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnMark {
    /// This mark's sequence number.
    pub(super) seq: u64,
    /// The identifier the turn ran under.
    pub(super) turn_id: u64,
    /// Each touched path's state before the turn wrote to it, filled in as the
    /// turn runs and never overwritten once set.
    pub(super) pre: StateMap,
}

/// One file's `before -> after` change, owned by whoever produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Edit {
    /// This edit's sequence number, which is every one of its regions' version
    /// index.
    pub(super) seq: u64,
    /// Who produced the change.
    pub(super) owner: Owner,
    /// The path that changed.
    pub(super) path: String,
    /// The state the change was computed against.
    pub(super) before: FileState,
    /// The state it produced.
    pub(super) after: FileState,
    /// Per ordinal, the earlier regions that ordinal was built on. Fixed when
    /// the edit is appended, because a dependency is a fact about the state the
    /// edit was written against, not about the log's current shape.
    pub(super) deps: BTreeMap<usize, Vec<RegionId>>,
}

impl Edit {
    /// The dependencies recorded for `ordinal`, or nothing.
    pub(super) fn deps_of(&self, ordinal: usize) -> &[RegionId] {
        self.deps.get(&ordinal).map_or(&[], Vec::as_slice)
    }
}

/// A keep or a revert recorded against one region.
///
/// Dragging the dependents of a revert is derived when the log is read, so
/// nothing is recorded for them here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Decide {
    /// This decision's sequence number.
    pub(super) seq: u64,
    /// The file the region belongs to.
    pub(super) path: String,
    /// The region decided.
    pub(super) hunk: RegionId,
    /// Whether the region stays.
    pub(super) keep: bool,
}

/// One entry of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Event {
    /// A turn began here.
    TurnMark(TurnMark),
    /// A file changed here.
    Edit(Edit),
    /// A region was decided here.
    Decide(Decide),
}

impl Event {
    /// This event as an edit, or [`None`].
    pub(super) const fn as_edit(&self) -> Option<&Edit> {
        match self {
            Self::Edit(edit) => Some(edit),
            _ => None,
        }
    }

    /// This event as a turn mark, or [`None`].
    pub(super) const fn as_mark(&self) -> Option<&TurnMark> {
        match self {
            Self::TurnMark(mark) => Some(mark),
            _ => None,
        }
    }
}

/// Paths mapped to states, in the order they were first recorded.
///
/// A turn's pre-edit map is read back in that order to seal the turn and to
/// plan a restore, so the order is part of the answer rather than an
/// implementation detail. A turn touches a handful of paths, which is why this
/// is a vector rather than a dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StateMap {
    entries: Vec<(String, FileState)>,
}

impl StateMap {
    /// Records `state` for `path` unless the path is already recorded, and
    /// answers whether it was recorded now.
    ///
    /// The first touch in a turn owns that turn's pre-edit state: a later tool
    /// writing the same file is building on the first one's output, not on the
    /// state the turn started from.
    pub(super) fn insert_if_absent(&mut self, path: &str, state: FileState) -> bool {
        if self.contains(path) {
            return false;
        }
        self.entries.push((path.to_owned(), state));
        true
    }

    /// Records `state` for `path`, replacing any earlier value.
    pub(super) fn set(&mut self, path: &str, state: FileState) {
        match self.entries.iter_mut().find(|(known, _)| known == path) {
            Some((_, held)) => *held = state,
            None => self.entries.push((path.to_owned(), state)),
        }
    }

    /// The state recorded for `path`, or [`None`].
    pub(super) fn get(&self, path: &str) -> Option<&FileState> {
        self.entries
            .iter()
            .find(|(known, _)| known == path)
            .map(|(_, state)| state)
    }

    /// Whether `path` is recorded.
    pub(super) fn contains(&self, path: &str) -> bool {
        self.entries.iter().any(|(known, _)| known == path)
    }

    /// Every entry, in recording order.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&str, &FileState)> {
        self.entries
            .iter()
            .map(|(path, state)| (path.as_str(), state))
    }
}
