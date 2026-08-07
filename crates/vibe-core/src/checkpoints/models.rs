//! The vocabulary every later part of the engine names things in.
//!
//! Two of these types are contracts rather than conveniences. [`RegionId`] is
//! the identity a client sends back when it accepts or reverts one hunk, so its
//! two halves have to mean what the reference says they mean: the sequence
//! number of the edit that produced the hunk, and the hunk's position in that
//! edit's opcode list. [`Owner`] is the other one, because a change the user
//! made by hand keeps its own permanent review slot beside the agent's turns
//! rather than being folded into whichever turn happened to run next.
//!
//! Everything here is plain data with no behavior beyond the conversions the
//! log needs. Mirrors `vibe/core/checkpoints/models.py`.

use serde::{Deserialize, Serialize};

use super::lines::FileState;

/// What went wrong in the log, in the terms the caller can act on.
///
/// The reference raises `TurnStateError` for lifecycle misuse and
/// `FileStateError` for everything else; the split survives here as variants,
/// so a surface can answer a turn-state refusal differently from a bad target
/// without matching on a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// A turn was begun while another was still open.
    #[error("a turn is already open, so another cannot begin before it is sealed")]
    TurnAlreadyOpen,
    /// A recording arrived with no turn to attribute it to.
    #[error("no turn is open, so there is nothing to record {operation} against")]
    NoOpenTurn {
        /// Which recording was refused, named for the caller.
        operation: &'static str,
    },
    /// A review decision arrived while a turn was still running.
    #[error("review decisions are refused while a turn is open")]
    DecisionDuringTurn,
    /// A decision named a region its file does not carry.
    #[error("{path} carries no region {}.{}", .region.version_index, .region.ordinal)]
    UnknownRegion {
        /// The path the decision named.
        path: String,
        /// The region identity the decision named.
        region: RegionId,
    },
    /// A decision was recorded as pending, which decides nothing.
    #[error("a decision is a keep or a revert, never pending")]
    PendingDecision,
    /// The log already retains as many file bytes as it is allowed to, so the
    /// capture was refused rather than making room by dropping an event.
    #[error(
        "file history for this session has reached its {limit}-byte limit; new changes are no longer tracked"
    )]
    RetentionExhausted {
        /// The ceiling that was reached, named so a reader can size it.
        limit: usize,
    },
}

/// Why a change is reviewable only as a whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpaqueReason {
    /// One side of the change does not exist: a creation or a deletion.
    Missing,
    /// One side carries bytes that have no lines to diff.
    BinaryOrUndecodable,
}

/// One line hunk of one edit.
///
/// Both spans are half-open: `[baseline_start, baseline_start + baseline_lines)`
/// indexes the keepends lines of the edit's before state, and the current span
/// indexes its after state. The lines are carried rather than referenced,
/// because a reconstruction splices them onto a projection that no longer
/// resembles either side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// Where the replaced lines start in the edit's before state.
    pub baseline_start: usize,
    /// The lines this hunk replaced, which a revert puts back.
    pub baseline_lines: Vec<String>,
    /// Where the written lines start in the edit's after state.
    pub current_start: usize,
    /// The lines this hunk wrote, which an approval keeps.
    pub current_lines: Vec<String>,
}

impl Region {
    /// One past the last replaced line, in the edit's before state.
    #[must_use]
    pub fn baseline_end(&self) -> usize {
        self.baseline_start + self.baseline_lines.len()
    }

    /// One past the last written line, in the edit's after state.
    #[must_use]
    pub fn current_end(&self) -> usize {
        self.current_start + self.current_lines.len()
    }
}

/// A change with no line structure, reviewable only as one unit.
///
/// Both sides' states are kept, so reverting a deletion restores the bytes and
/// reverting a binary rewrite restores the earlier bytes, neither of which the
/// line model could express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueChange {
    /// Why this change has no lines.
    pub reason: OpaqueReason,
    /// The state before the edit.
    pub baseline: FileState,
    /// The state after it.
    pub current: FileState,
}

/// What one edit contributed at one ordinal: a line hunk or the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A line hunk.
    Text(Region),
    /// A whole-file unit.
    Opaque(OpaqueChange),
}

impl Change {
    /// The line hunk, or [`None`] when this change covers the whole file.
    #[must_use]
    pub const fn as_text(&self) -> Option<&Region> {
        match self {
            Self::Text(region) => Some(region),
            Self::Opaque(_) => None,
        }
    }

    /// Whether this change covers the whole file.
    #[must_use]
    pub const fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque(_))
    }
}

/// The stable identity of one hunk.
///
/// `version_index` is the sequence number of the edit that produced the hunk
/// and `ordinal` its position in that edit's opcode list. Neither moves as the
/// log grows, which is what lets a client accept a hunk it was shown several
/// turns ago. The ordering is the pair's, and it is the order every dependency
/// list, anchor target and decision batch is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionId {
    /// The producing edit's sequence number.
    pub version_index: u64,
    /// The hunk's position in that edit's opcode list.
    pub ordinal: usize,
}

impl RegionId {
    /// The identity of the hunk at `ordinal` of the edit numbered
    /// `version_index`.
    #[must_use]
    pub const fn new(version_index: u64, ordinal: usize) -> Self {
        Self {
            version_index,
            ordinal,
        }
    }
}

impl std::fmt::Display for RegionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}", self.version_index, self.ordinal)
    }
}

/// Who produced a change.
///
/// A hand edit is not an anonymous drift folded into the next turn: it owns its
/// change the way a turn owns one, so a revert of the turn leaves it standing
/// unless it was built on the reverted lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Owner {
    /// One agent turn, named by the identifier the turn ran under.
    Agent {
        /// The turn's identifier.
        turn_id: u64,
    },
    /// One change the user made by hand, in its own one-based slot.
    Manual {
        /// The slot, counted from one in the order the edits were captured.
        index: usize,
    },
}

/// What a reviewer decided about one hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Nothing has been decided yet.
    Pending,
    /// The hunk stays.
    Keep,
    /// The hunk goes, and so does everything built on it.
    Revert,
}

impl Decision {
    /// Whether this decision keeps, refusing the one value that decides
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::PendingDecision`] for [`Decision::Pending`],
    /// rather than defaulting it to either answer.
    pub const fn as_keep(self) -> Result<bool, CheckpointError> {
        match self {
            Self::Pending => Err(CheckpointError::PendingDecision),
            Self::Keep => Ok(true),
            Self::Revert => Ok(false),
        }
    }
}

/// One hunk as a reader sees it: its identity, its owner, its content, the
/// decision in force and what it was built on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRegion {
    /// The hunk's stable identity.
    pub region_id: RegionId,
    /// Who produced it.
    pub owner: Owner,
    /// What it changed.
    pub change: Change,
    /// The decision in force after dragging, which is not always the one
    /// recorded against it.
    pub decision: Decision,
    /// The earlier hunks it was built on, in identity order.
    pub depends_on: Vec<RegionId>,
}

/// Which side of a rendered diff an anchor sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HunkSide {
    /// The current side, where added and rewritten lines are rendered.
    Additions,
    /// The baseline side, which is the only place a pure deletion is visible.
    Deletions,
}

/// One pending change located in a rendered diff, with what a control pinned
/// there would decide.
///
/// `line` is zero-based and points at the block's last line, so an inline
/// control renders after the whole change rather than inside it. `regions` is
/// the full dependency component, so accepting one anchor never strands a hunk
/// it was built on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkAnchor {
    /// Which side of the diff the line belongs to.
    pub side: HunkSide,
    /// The zero-based index of the block's last line on that side.
    pub line: usize,
    /// The hunks a control at this anchor decides together, in identity order.
    pub regions: Vec<RegionId>,
}
