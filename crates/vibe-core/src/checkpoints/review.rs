//! What a reviewer is shown, and what a reviewer decides.
//!
//! [`super::history`] answers in the engine's own vocabulary: a [`TurnRegion`]
//! carries the lines it replaced and the lines it wrote, because a
//! reconstruction needs them. A review panel needs none of that. It needs the
//! identity, the owner, the spans and the decision, so this module is the
//! narrowing: every type here is what one of the six review methods publishes,
//! and nothing here holds content a client did not ask for.
//!
//! The projection is pure. It reads a borrowed [`History`] and the states a
//! caller already read from disk, and it hands back plain data. The impure half,
//! the one that reads disk before projecting and writes it after deciding, lives
//! in `crate::workspace::review` where the filesystem already is.
//!
//! [`decide_target`] is the other half of the surface: the seven granularities a
//! client can address a decision at, resolved against the log into the paths the
//! decision touched. It takes the log rather than the read model because
//! deciding appends, and it returns paths rather than writing them, because the
//! write has to fail the decision with it.
//!
//! Mirrors `vibe/core/review/manager.py` and the wire projection in
//! `vibe/app_server/_review.py`. Every name here is the reference's; every field
//! is one the app-server census records.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::checkpointer::Checkpointer;
use super::files::FileAccessError;
use super::history::History;
use super::lines::FileState;
use super::models::{
    Change, CheckpointError, Decision, HunkSide, OpaqueReason, Owner, RegionId, TurnRegion,
};
use crate::workspace::text_file::decode;

/// What went wrong answering or deciding a review.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReviewError {
    /// The log refused the decision.
    #[error(transparent)]
    Log(#[from] CheckpointError),
    /// A file could not be read or written.
    #[error(transparent)]
    File(#[from] FileAccessError),
    /// Another thread poisoned the log.
    #[error("the checkpoint log lock is poisoned")]
    LockPoisoned,
}

/// How a file stands relative to the review baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    /// The file existed before and still does.
    Modified,
    /// Nothing was there before this log's first edit.
    Created,
    /// Nothing is there now.
    Deleted,
    /// At least one change to it has no lines to render.
    BinaryOrUndecodable,
}

/// One line hunk as a review panel renders it.
///
/// The lines themselves are not here: a client renders them from the baseline
/// and the diff it already asked for, and the spans are what pins a control to
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextReviewRegion {
    /// The producing edit's sequence number.
    pub version_index: u64,
    /// The hunk's position in that edit's opcode list.
    pub ordinal: usize,
    /// The turn or the hand edit that produced it.
    pub owner: Owner,
    /// Where the replaced lines start in the edit's before state.
    pub baseline_start: usize,
    /// How many lines were replaced.
    pub baseline_line_count: usize,
    /// Where the written lines start in the edit's after state.
    pub current_start: usize,
    /// How many lines were written.
    pub current_line_count: usize,
    /// The decision in force after dragging.
    pub decision: Decision,
    /// The earlier hunks this one was built on.
    pub depends_on: Vec<RegionId>,
}

/// One whole-file change as a review panel renders it.
///
/// A binary rewrite and a deletion have no line coordinates to pin a control to,
/// so the control covers the file and the reason says why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpaqueReviewRegion {
    /// The producing edit's sequence number.
    pub version_index: u64,
    /// The hunk's position in that edit's opcode list, which is always zero
    /// because an opaque edit contributes one unit.
    pub ordinal: usize,
    /// The turn or the hand edit that produced it.
    pub owner: Owner,
    /// Why this change has no lines.
    pub reason: OpaqueReason,
    /// The decision in force after dragging.
    pub decision: Decision,
    /// The earlier hunks this one was built on.
    pub depends_on: Vec<RegionId>,
}

/// One reviewable change: a line hunk or a whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ReviewRegion {
    /// A line hunk.
    Text(TextReviewRegion),
    /// A whole-file unit.
    Opaque(OpaqueReviewRegion),
}

impl ReviewRegion {
    /// The owner that produced this change.
    #[must_use]
    pub const fn owner(&self) -> Owner {
        match self {
            Self::Text(region) => region.owner,
            Self::Opaque(region) => region.owner,
        }
    }

    /// The decision in force for this change.
    #[must_use]
    pub const fn decision(&self) -> Decision {
        match self {
            Self::Text(region) => region.decision,
            Self::Opaque(region) => region.decision,
        }
    }
}

/// One file with something left to review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFile {
    /// The path, in the display form the log keys by.
    pub path: String,
    /// How the file stands.
    pub status: ReviewFileStatus,
    /// Every region of the file, decided ones included, in log order.
    pub regions: Vec<ReviewRegion>,
}

/// One file inside one owner's review slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScopeFile {
    /// The path.
    pub path: String,
    /// How the file stands.
    pub status: ReviewFileStatus,
    /// How many of this owner's regions in it are still pending.
    pub region_count: usize,
}

/// One review slot: an agent turn or a hand edit, with what it still has
/// pending.
///
/// A slot keeps its position for the life of the log, so resolving one never
/// renumbers another under a client looking at both. A fully decided slot stays,
/// carrying no files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    /// Whose slot this is.
    pub owner: Owner,
    /// The files it still has pending regions in.
    pub files: Vec<ReviewScopeFile>,
}

/// Everything `review/state` publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewState {
    /// The files carrying at least one pending region.
    pub files: Vec<ReviewFile>,
    /// Every owner's slot, in log order.
    pub scopes: Vec<ReviewScope>,
}

/// One owner's own change to one file, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnFileDiff {
    /// The owner's own contribution, not the file's overall standing.
    pub status: ReviewFileStatus,
    /// The file as it stood before this owner's pending regions.
    pub baseline: String,
    /// The file as it stands with them.
    pub current: String,
}

/// One pending change located in a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewHunk {
    /// Which side of the diff the line belongs to.
    pub side: HunkSide,
    /// The zero-based index of the block's last line on that side.
    pub line: usize,
    /// The regions a control at this anchor decides together.
    pub regions: Vec<RegionId>,
}

/// The granularity a decision addresses.
///
/// The seven variants are the reference's, and their wire spelling is what a
/// client sends: a `kind` discriminator beside the variant's own fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ReviewTarget {
    /// One hunk of one file.
    Region {
        /// The file.
        path: String,
        /// The producing edit's sequence number.
        version_index: u64,
        /// The hunk's position in that edit's opcode list.
        ordinal: usize,
    },
    /// Several hunks of one file, decided together.
    Regions {
        /// The file.
        path: String,
        /// The hunks.
        regions: Vec<RegionId>,
    },
    /// Everything one owner produced, across every file it touched.
    Scope {
        /// The owner.
        owner: Owner,
    },
    /// One owner's own changes to one file.
    ScopeFile {
        /// The owner.
        owner: Owner,
        /// The file.
        path: String,
    },
    /// Every still-pending hunk of one file.
    File {
        /// The file.
        path: String,
    },
    /// Every file carrying a pending hunk.
    All,
    /// The newest turns past the accepted frontier.
    LastTurns {
        /// How many of them. Zero or less decides nothing.
        count: i64,
    },
}

/// The projection of one engine region.
fn project_region(region: &TurnRegion) -> ReviewRegion {
    let depends_on = region.depends_on.clone();
    match &region.change {
        Change::Opaque(change) => ReviewRegion::Opaque(OpaqueReviewRegion {
            version_index: region.region_id.version_index,
            ordinal: region.region_id.ordinal,
            owner: region.owner,
            reason: change.reason,
            decision: region.decision,
            depends_on,
        }),
        Change::Text(change) => ReviewRegion::Text(TextReviewRegion {
            version_index: region.region_id.version_index,
            ordinal: region.region_id.ordinal,
            owner: region.owner,
            baseline_start: change.baseline_start,
            baseline_line_count: change.baseline_lines.len(),
            current_start: change.current_start,
            current_line_count: change.current_lines.len(),
            decision: region.decision,
            depends_on,
        }),
    }
}

/// How `path` stands, given what disk holds and what the log recorded.
///
/// The order is the reference's and each step subsumes the ones after it: a file
/// that is gone is deleted whatever its regions look like, and a file carrying
/// an undecodable change cannot be rendered as a line diff even if it was
/// created by this log.
fn project_status(
    history: &History<'_>,
    path: &str,
    current: &FileState,
    view: &[TurnRegion],
) -> ReviewFileStatus {
    if current.data().is_none() {
        return ReviewFileStatus::Deleted;
    }
    let undecodable = view.iter().any(|region| match &region.change {
        Change::Opaque(change) => change.reason == OpaqueReason::BinaryOrUndecodable,
        Change::Text(_) => false,
    });
    if undecodable {
        return ReviewFileStatus::BinaryOrUndecodable;
    }
    if history.original(path).data().is_none() {
        return ReviewFileStatus::Created;
    }
    ReviewFileStatus::Modified
}

/// One file's entry, or [`None`] when it has nothing left to review.
///
/// A file with no region was never changed, and a file whose every region is
/// decided is resolved: the kept ones live in the accepted baseline and the
/// reverted ones are already back on disk.
fn project_file(
    history: &History<'_>,
    path: &str,
    current: &FileState,
    view: &[TurnRegion],
) -> Option<ReviewFile> {
    if view.is_empty()
        || view
            .iter()
            .all(|region| region.decision != Decision::Pending)
    {
        return None;
    }
    Some(ReviewFile {
        path: path.to_owned(),
        status: project_status(history, path, current, view),
        regions: view.iter().map(project_region).collect(),
    })
}

/// Every owner's slot, in log order, with the files it still has pending.
fn project_scopes(history: &History<'_>, files: &[ReviewFile]) -> Vec<ReviewScope> {
    history
        .scopes()
        .into_iter()
        .map(|owner| ReviewScope {
            owner,
            files: files
                .iter()
                .filter_map(|file| {
                    let region_count = file
                        .regions
                        .iter()
                        .filter(|region| {
                            region.owner() == owner && region.decision() == Decision::Pending
                        })
                        .count();
                    (region_count > 0).then(|| ReviewScopeFile {
                        path: file.path.clone(),
                        status: file.status,
                        region_count,
                    })
                })
                .collect(),
        })
        .collect()
}

/// Everything `review/state` publishes, over the states the caller read.
///
/// A tracked path missing from `current` is treated as absent, which is the
/// same answer the port gives for a path that is not there.
#[must_use]
pub fn project_state(history: &History<'_>, current: &BTreeMap<String, FileState>) -> ReviewState {
    let absent = FileState::absent();
    let files: Vec<ReviewFile> = history
        .tracked_paths()
        .into_iter()
        .filter_map(|path| {
            let view = history.regions(&path);
            let state = current.get(&path).unwrap_or(&absent);
            project_file(history, &path, state, &view)
        })
        .collect();
    let scopes = project_scopes(history, &files);
    ReviewState { files, scopes }
}

/// One owner's own change to `path`.
///
/// An owner that never touched the path, or has nothing left pending in it,
/// answers `modified` with both sides empty rather than failing, because a panel
/// asking about every slot asks about slots that touched other files.
#[must_use]
pub fn project_scope_diff(history: &History<'_>, path: &str, owner: Owner) -> TurnFileDiff {
    let Some((baseline, current)) = history.scope_pending_diff(path, owner) else {
        return TurnFileDiff {
            status: ReviewFileStatus::Modified,
            baseline: String::new(),
            current: String::new(),
        };
    };
    let (baseline_text, baseline_opaque) = diff_side(&baseline);
    let (current_text, current_opaque) = diff_side(&current);
    if baseline_opaque || current_opaque {
        return TurnFileDiff {
            status: ReviewFileStatus::BinaryOrUndecodable,
            baseline: String::new(),
            current: String::new(),
        };
    }
    let status = if baseline.data().is_none() {
        ReviewFileStatus::Created
    } else if current.data().is_none() {
        ReviewFileStatus::Deleted
    } else {
        ReviewFileStatus::Modified
    };
    TurnFileDiff {
        status,
        baseline: baseline_text,
        current: current_text,
    }
}

/// The decoded text of `state`, plus whether it has no lines to render.
///
/// An absent file decodes to empty text and is not opaque: nothing to render is
/// not the same as nothing renderable.
fn diff_side(state: &FileState) -> (String, bool) {
    match state.data() {
        None => (String::new(), false),
        Some(_) if state.is_binary() => (String::new(), true),
        Some(bytes) => (decode(bytes).text, false),
    }
}

/// The decoded text of `state`, empty when nothing is there.
#[must_use]
pub fn state_text(state: &FileState) -> String {
    state
        .data()
        .map(|bytes| decode(bytes).text)
        .unwrap_or_default()
}

/// Records `decision` at the granularity `target` names, and answers the paths
/// it touched, in the order it touched them.
///
/// Nothing here writes: the caller persists the paths this returns, and the two
/// have to fail together, which is why the write is the caller's and not this
/// function's.
///
/// # Errors
///
/// Refuses while a turn is open, refuses a pending decision, and refuses a
/// region the named file does not carry.
pub fn decide_target(
    log: &mut Checkpointer,
    target: &ReviewTarget,
    decision: Decision,
) -> Result<Vec<String>, CheckpointError> {
    match target {
        ReviewTarget::Region {
            path,
            version_index,
            ordinal,
        } => {
            log.decide_region(path, RegionId::new(*version_index, *ordinal), decision)?;
            Ok(vec![path.clone()])
        }
        ReviewTarget::Regions { path, regions } => {
            for region in regions {
                log.decide_region(path, *region, decision)?;
            }
            Ok(vec![path.clone()])
        }
        ReviewTarget::File { path } => {
            log.decide_file(path, decision)?;
            Ok(vec![path.clone()])
        }
        ReviewTarget::ScopeFile { owner, path } => {
            log.decide_scope(path, *owner, decision)?;
            Ok(vec![path.clone()])
        }
        ReviewTarget::Scope { owner } => decide_scope_everywhere(log, *owner, decision),
        ReviewTarget::All => {
            let paths = paths_with_pending(log);
            for path in &paths {
                log.decide_file(path, decision)?;
            }
            Ok(paths)
        }
        ReviewTarget::LastTurns { count } => decide_last_turns(log, *count, decision),
    }
}

/// Decides everything `owner` produced, wherever it produced it.
fn decide_scope_everywhere(
    log: &mut Checkpointer,
    owner: Owner,
    decision: Decision,
) -> Result<Vec<String>, CheckpointError> {
    let tracked = log.history().tracked_paths();
    let mut touched = Vec::new();
    for path in tracked {
        if log
            .history()
            .regions(&path)
            .iter()
            .any(|region| region.owner == owner)
        {
            touched.push(path);
        }
    }
    for path in &touched {
        log.decide_scope(path, owner, decision)?;
    }
    Ok(touched)
}

/// Decides the newest `count` turns that sit past the accepted frontier.
///
/// The frontier is where a client's "last N turns" control starts counting:
/// turns already fully kept are behind it and a later decision never revisits
/// them.
fn decide_last_turns(
    log: &mut Checkpointer,
    count: i64,
    decision: Decision,
) -> Result<Vec<String>, CheckpointError> {
    let Ok(count) = usize::try_from(count) else {
        return Ok(Vec::new());
    };
    if count == 0 {
        return Ok(Vec::new());
    }
    let (frontier, turn_ids) = {
        let history = log.history();
        (history.accepted_turn_frontier(), history.turn_ids())
    };
    let pending = turn_ids.get(frontier..).unwrap_or_default();
    let selected = pending
        .get(pending.len().saturating_sub(count)..)
        .unwrap_or_default()
        .to_vec();
    let mut affected = Vec::new();
    for turn_id in selected {
        affected.extend(decide_scope_everywhere(
            log,
            Owner::Agent { turn_id },
            decision,
        )?);
    }
    Ok(affected)
}

/// Every tracked path carrying at least one pending region.
fn paths_with_pending(log: &Checkpointer) -> Vec<String> {
    let history = log.history();
    history
        .tracked_paths()
        .into_iter()
        .filter(|path| {
            history
                .effective(path)
                .values()
                .any(|decision| *decision == Decision::Pending)
        })
        .collect()
}
