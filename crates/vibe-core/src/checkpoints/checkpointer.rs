//! The log itself: what may be appended, when, and what a failure undoes.
//!
//! Every read in this engine resolves from one ordered list, so the only thing
//! this type has to get right is what goes into it. Three rules do most of the
//! work:
//!
//! - A turn is a scope. Recording outside one is refused, deciding inside one is
//!   refused, and beginning one while another is open is refused. The reference
//!   makes the same three refusals, and they are what keeps an in-flight turn
//!   from being reviewed against a disk it is still writing.
//! - The first touch of a path in a turn owns that turn's before state. A second
//!   tool writing the same file is building on the first one's output, so
//!   recording it again would attribute the first tool's work to nothing.
//! - Nothing is ever rewritten. A decision appends, a revert appends, and the
//!   only way back is truncation. That is what makes a region identity
//!   permanent.
//!
//! Mirrors `vibe/core/checkpoints/checkpointer.py`.

use std::collections::{BTreeSet, HashMap};

use super::events::{Decide, Edit, Event, StateMap, TurnMark};
use super::history::History;
use super::lines::FileState;
use super::models::{CheckpointError, Decision, Owner, RegionId};

/// The turn currently running, if one is.
#[derive(Debug, Clone)]
struct OpenTurn {
    /// The sequence number of this turn's mark, which is where its pre-edit
    /// states are recorded.
    mark_seq: u64,
    /// The identifier the turn runs under.
    turn_id: u64,
    /// Each touched path's state as the turn last left it, replaced on every
    /// recording because the last write is what the turn produced.
    post: StateMap,
}

/// An append-only log of turns, edits and decisions.
///
/// Nothing here touches disk: a caller feeds in the content it read and applies
/// the content this hands back. That is what lets the whole model be exercised
/// without a filesystem, and it is the split the reference draws in the same
/// place.
#[derive(Debug, Default)]
pub struct Checkpointer {
    events: Vec<Event>,
    seq: u64,
    manual_index: usize,
    open: Option<OpenTurn>,
}

impl Checkpointer {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a turn is currently open.
    #[must_use]
    pub const fn has_open_turn(&self) -> bool {
        self.open.is_some()
    }

    /// The read model over this log.
    #[must_use]
    pub fn history(&self) -> History<'_> {
        History::new(&self.events)
    }

    // -- Turn lifecycle ------------------------------------------------------

    /// Opens a turn under `turn_id`.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::TurnAlreadyOpen`] when a turn is still open,
    /// leaving the log untouched.
    pub fn begin_turn(&mut self, turn_id: u64) -> Result<(), CheckpointError> {
        if self.open.is_some() {
            return Err(CheckpointError::TurnAlreadyOpen);
        }
        let seq = self.bump();
        self.events.push(Event::TurnMark(TurnMark {
            seq,
            turn_id,
            pre: StateMap::default(),
        }));
        self.open = Some(OpenTurn {
            mark_seq: seq,
            turn_id,
            post: StateMap::default(),
        });
        Ok(())
    }

    /// Records the state `path` held before the running turn wrote to it.
    ///
    /// Only the first recording of a path counts for the turn.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::NoOpenTurn`] when no turn is running.
    pub fn record_pre_edit(&mut self, path: &str, pre: FileState) -> Result<(), CheckpointError> {
        let Some(mark) = self.open_mark_mut() else {
            return Err(CheckpointError::NoOpenTurn {
                operation: "a pre-edit state",
            });
        };
        mark.pre.insert_if_absent(path, pre);
        Ok(())
    }

    /// Records the state `path` holds now, replacing any earlier recording for
    /// this turn.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::NoOpenTurn`] when no turn is running.
    pub fn record_post_edit(&mut self, path: &str, post: FileState) -> Result<(), CheckpointError> {
        let Some(open) = self.open.as_mut() else {
            return Err(CheckpointError::NoOpenTurn {
                operation: "a post-edit state",
            });
        };
        open.post.set(path, post);
        Ok(())
    }

    /// Closes the running turn, appending one edit per path it actually
    /// changed.
    ///
    /// A path recorded before the turn wrote to it but never after defaults to
    /// its own before state, so a tool that reported an intent it did not carry
    /// out leaves nothing behind. Sealing with no turn open does nothing, which
    /// is what lets a caller seal on every exit path.
    pub fn seal_turn(&mut self) {
        let Some(turn) = self.open.take() else {
            return;
        };
        let pre: Vec<(String, FileState)> = self
            .mark(turn.mark_seq)
            .map(|mark| {
                mark.pre
                    .iter()
                    .map(|(path, state)| (path.to_owned(), state.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (path, before) in pre {
            let after = turn
                .post
                .get(&path)
                .cloned()
                .unwrap_or_else(|| before.clone());
            self.append_edit(
                Owner::Agent {
                    turn_id: turn.turn_id,
                },
                &path,
                before,
                after,
            );
        }
    }

    /// Empties the log and everything counted against it.
    pub fn clear(&mut self) {
        self.events.clear();
        self.seq = 0;
        self.manual_index = 0;
        self.open = None;
    }

    /// Runs `body`, restoring the log to its prior state if it fails.
    ///
    /// A review decision is durable only once whatever it implies on disk has
    /// succeeded, so the two have to fail together. Restoring the sequence
    /// counter as well is what keeps a retried decision from being numbered
    /// past the events it was rolled back over.
    ///
    /// # Errors
    ///
    /// Returns whatever `body` returned, after the rollback.
    pub fn atomic<T, E>(&mut self, body: impl FnOnce(&mut Self) -> Result<T, E>) -> Result<T, E> {
        let events = self.events.clone();
        let seq = self.seq;
        let manual_index = self.manual_index;
        let open = self.open.clone();
        match body(self) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.events = events;
                self.seq = seq;
                self.manual_index = manual_index;
                self.open = open;
                Err(error)
            }
        }
    }

    // -- Review decisions ----------------------------------------------------

    /// Decides one region of `path`.
    ///
    /// # Errors
    ///
    /// Refuses while a turn is open, refuses a pending decision, and refuses a
    /// region the file does not carry, leaving the log untouched in each case.
    pub fn decide_region(
        &mut self,
        path: &str,
        region_id: RegionId,
        decision: Decision,
    ) -> Result<(), CheckpointError> {
        self.ensure_reviewable()?;
        self.decide(path, &[region_id], decision.as_keep()?)
    }

    /// Decides every region of `path` that `owner` produced and nothing else
    /// has decided.
    ///
    /// # Errors
    ///
    /// Refuses while a turn is open, and refuses a pending decision.
    pub fn decide_scope(
        &mut self,
        path: &str,
        owner: Owner,
        decision: Decision,
    ) -> Result<(), CheckpointError> {
        self.ensure_reviewable()?;
        let keep = decision.as_keep()?;
        let targets: Vec<RegionId> = {
            let history = self.history();
            let effective = history.effective(path);
            history
                .owned_hunks(path)
                .into_iter()
                .filter(|(region_id, produced_by)| {
                    *produced_by == owner && effective.get(region_id) == Some(&Decision::Pending)
                })
                .map(|(region_id, _owner)| region_id)
                .collect()
        };
        self.decide(path, &targets, keep)
    }

    /// Decides every still-pending region of `path`.
    ///
    /// # Errors
    ///
    /// Refuses while a turn is open, and refuses a pending decision.
    pub fn decide_file(&mut self, path: &str, decision: Decision) -> Result<(), CheckpointError> {
        self.ensure_reviewable()?;
        let keep = decision.as_keep()?;
        let targets: Vec<RegionId> = {
            let history = self.history();
            let effective = history.effective(path);
            history
                .hunk_order(path)
                .into_iter()
                .filter(|(region_id, _deps)| effective.get(region_id) == Some(&Decision::Pending))
                .map(|(region_id, _deps)| region_id)
                .collect()
        };
        self.decide(path, &targets, keep)
    }

    // -- Internals -----------------------------------------------------------

    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The mark numbered `seq`, newest first because a transcript reset can
    /// leave an older mark carrying the same turn identifier.
    fn mark(&self, seq: u64) -> Option<&TurnMark> {
        self.events
            .iter()
            .rev()
            .filter_map(Event::as_mark)
            .find(|mark| mark.seq == seq)
    }

    fn open_mark_mut(&mut self) -> Option<&mut TurnMark> {
        let seq = self.open.as_ref()?.mark_seq;
        self.events.iter_mut().rev().find_map(|event| match event {
            Event::TurnMark(mark) if mark.seq == seq => Some(mark),
            _ => None,
        })
    }

    /// Appends one edit, unless nothing changed.
    fn append_edit(&mut self, owner: Owner, path: &str, before: FileState, after: FileState) {
        if before == after {
            return;
        }
        let deps = self.history().compute_deps(path, &before, &after);
        let seq = self.bump();
        self.events.push(Event::Edit(Edit {
            seq,
            owner,
            path: path.to_owned(),
            before,
            after,
            deps,
        }));
    }

    fn ensure_reviewable(&self) -> Result<(), CheckpointError> {
        if self.open.is_some() {
            return Err(CheckpointError::DecisionDuringTurn);
        }
        Ok(())
    }

    /// Records a decision over `region_ids`.
    ///
    /// Keeping a region pulls in the pending regions it was built on, because
    /// an accepted baseline that skipped them would splice onto lines it does
    /// not carry. Reverting records only the target: its dependents are dragged
    /// when the log is read, so recording them would freeze an answer that a
    /// later truncation should undo.
    fn decide(
        &mut self,
        path: &str,
        region_ids: &[RegionId],
        keep: bool,
    ) -> Result<(), CheckpointError> {
        let to_append: Vec<RegionId> = {
            let history = self.history();
            let order = history.hunk_order(path);
            let valid: BTreeSet<RegionId> =
                order.iter().map(|(region_id, _deps)| *region_id).collect();
            if let Some(unknown) = region_ids
                .iter()
                .find(|region_id| !valid.contains(region_id))
            {
                return Err(CheckpointError::UnknownRegion {
                    path: path.to_owned(),
                    region: *unknown,
                });
            }
            let effective = history.effective(path);
            if keep {
                let deps_of: HashMap<RegionId, Vec<RegionId>> = order.into_iter().collect();
                let mut to_keep: BTreeSet<RegionId> = BTreeSet::new();
                let mut stack: Vec<RegionId> = region_ids.to_vec();
                while let Some(region_id) = stack.pop() {
                    if to_keep.contains(&region_id)
                        || effective.get(&region_id) != Some(&Decision::Pending)
                    {
                        continue;
                    }
                    to_keep.insert(region_id);
                    stack.extend(deps_of.get(&region_id).into_iter().flatten().copied());
                }
                to_keep.into_iter().collect()
            } else {
                region_ids
                    .iter()
                    .copied()
                    .filter(|region_id| effective.get(region_id) != Some(&Decision::Revert))
                    .collect()
            }
        };
        for region_id in to_append {
            let seq = self.bump();
            self.events.push(Event::Decide(Decide {
                seq,
                path: path.to_owned(),
                hunk: region_id,
                keep,
            }));
        }
        Ok(())
    }
}
