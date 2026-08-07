//! The read model: everything the log can answer, resolved without touching it.
//!
//! The log records what happened. This module decides what that means, and it
//! decides it every time it is asked rather than storing an answer that could
//! go stale. Four resolutions carry the design:
//!
//! - A region is one non-equal opcode of one edit, numbered by its position in
//!   that edit's opcode list. Files with no line structure resolve to a single
//!   whole-file unit instead, because a binary rewrite and a deletion are still
//!   things a reviewer accepts or refuses.
//! - A region's dependencies are the earlier regions whose lines it replaced,
//!   read off a per-line provenance the projection carries. They are computed
//!   once, when the edit is appended, because they describe the state that edit
//!   was written against.
//! - An effective decision is derived by closure: a region is reverted when it
//!   was reverted, or when anything it was built on is. Nothing is stored for
//!   the dragged ones, so a later edit cannot resurrect a stale answer.
//! - A projection replays the applied regions of each edit in turn, splicing
//!   each edit's spans through its own before state onto the running result.
//!   Selecting only the regions whose dependencies are also applied is what
//!   makes every splice land on the exact base it was computed against.
//!
//! Mirrors `vibe/core/checkpoints/history.py`. The memo caches live here rather
//! than in the log for the reason the reference gives them a fresh `History`
//! per read: a cache that cannot outlive the borrow of the events it summarizes
//! cannot go stale.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::events::{Edit, Event};
use super::lines::{FileState, decode_lines};
use super::matcher::{SequenceMatcher, Tag};
use super::models::{
    Change, Decision, HunkAnchor, HunkSide, OpaqueChange, OpaqueReason, Owner, Region, RegionId,
    TurnRegion,
};
use crate::workspace::text_file::{decode, encode};

/// The changes of one edit, memoized by that edit's sequence number.
type Changes = Arc<Vec<(usize, Change)>>;
/// Every region of one file with the decision in force for it.
type Effective = Arc<BTreeMap<RegionId, Decision>>;

/// The keepends lines of a state, with binary and absent both answering none.
///
/// The distinction between the two survives in [`opaque_reason`]; here they are
/// alike, because neither has lines to diff.
fn lines_of(state: &FileState) -> Vec<String> {
    decode_lines(state).unwrap_or_default()
}

/// Re-encodes reconstructed `text` the way `reference` was encoded.
///
/// A partial revert of a CRLF file written in a single-byte codec rewrites the
/// lines the decision touched; rewriting the rest to UTF-8 with LF at the same
/// time would make the diff the reviewer sees unrelated to the decision they
/// took.
fn reencode(text: &str, reference: &FileState) -> FileState {
    let Some(bytes) = reference.data() else {
        return FileState::from_text(text);
    };
    let decoded = decode(bytes);
    FileState::from_bytes(encode(text, decoded.encoding, decoded.newline))
}

/// The line hunks turning `base_lines` into `cur_lines`, in opcode order.
fn regions_between(base_lines: &[String], cur_lines: &[String]) -> Vec<Region> {
    SequenceMatcher::new(base_lines, cur_lines)
        .opcodes()
        .into_iter()
        .filter(|opcode| opcode.tag != Tag::Equal)
        .map(|opcode| Region {
            baseline_start: opcode.i1,
            baseline_lines: base_lines[opcode.i1..opcode.i2].to_vec(),
            current_start: opcode.j1,
            current_lines: cur_lines[opcode.j1..opcode.j2].to_vec(),
        })
        .collect()
}

/// Where each line boundary of `base` sits in `result`.
///
/// The map has one more entry than `base` has lines, because a span's end is a
/// boundary too. A non-equal opcode pins only its two ends, which is all a
/// rebased span needs: the lines between them were rewritten, so no boundary
/// inside them means anything in `result`.
fn base_to_result(base: &[String], result: &[String]) -> Vec<usize> {
    let mut mapping = vec![0_usize; base.len() + 1];
    for opcode in SequenceMatcher::new(base, result).opcodes() {
        if opcode.tag == Tag::Equal {
            for offset in 0..=(opcode.i2 - opcode.i1) {
                mapping[opcode.i1 + offset] = opcode.j1 + offset;
            }
        } else {
            mapping[opcode.i1] = opcode.j1;
            mapping[opcode.i2] = opcode.j2;
        }
    }
    mapping
}

/// A rebased span, as a range of a `length`-long projection that can be
/// spliced.
///
/// Rebasing pins each end of a span independently and the projection is
/// rewritten as the splices run, so a span can come back inverted or reaching
/// past what is left of the result. Both only happen off the dependency-closed
/// path, where a decision selected a region whose base the projection no longer
/// carries. The reference expresses the splice as a slice assignment and Python
/// resolves those two cases rather than refusing them: an index past the end
/// clamps to the end, and an end before the start becomes the start, which
/// turns the hunk into an insertion at that point. Resolving them the same way
/// here is what keeps the answer the reference's answer instead of a panic.
fn rebased(start: usize, end: usize, length: usize) -> std::ops::Range<usize> {
    let start = start.min(length);
    start..end.clamp(start, length)
}

/// Applies `hunks`, expressed against `base`, onto `result`.
///
/// Each span is rebased through `base -> result` and the splices run right to
/// left, so an earlier one never shifts a later one's coordinates.
fn splice(result: &mut Vec<String>, base: &[String], hunks: &[&Region]) {
    let mapping = base_to_result(base, result);
    let mut edits: Vec<(usize, usize, &[String])> = hunks
        .iter()
        .map(|hunk| {
            (
                mapping[hunk.baseline_start],
                mapping[hunk.baseline_end()],
                hunk.current_lines.as_slice(),
            )
        })
        .collect();
    edits.sort_unstable();
    for (start, end, lines) in edits.into_iter().rev() {
        let span = rebased(start, end, result.len());
        result.splice(span, lines.iter().cloned());
    }
}

/// [`splice`], carrying along which region wrote each line of the result.
fn splice_with_prov(
    result: &mut Vec<String>,
    prov: &mut Vec<Option<RegionId>>,
    base: &[String],
    hunks: &[(RegionId, &Region)],
) {
    let mapping = base_to_result(base, result);
    let mut items: Vec<(usize, usize, &[String], RegionId)> = hunks
        .iter()
        .map(|(region_id, hunk)| {
            (
                mapping[hunk.baseline_start],
                mapping[hunk.baseline_end()],
                hunk.current_lines.as_slice(),
                *region_id,
            )
        })
        .collect();
    items.sort_unstable();
    for (start, end, lines, region_id) in items.into_iter().rev() {
        // The two vectors are the same length by construction, so one span
        // splices both and the provenance stays beside its line.
        let span = rebased(start, end, result.len());
        result.splice(span.clone(), lines.iter().cloned());
        prov.splice(span, std::iter::repeat_n(Some(region_id), lines.len()));
    }
}

/// Which pending deletions produced a rendered delete block.
///
/// A deleted line leaves nothing on the current side to carry provenance, so
/// the block is attributed by its content instead. Each deletion claims one
/// distinct run and claims it once across the whole diff, which is what keeps
/// two independent deletions of identical text from being decided together.
fn match_deletions(
    removed: &[String],
    deletions: &[(RegionId, Vec<String>)],
    consumed: &mut BTreeSet<RegionId>,
) -> BTreeSet<RegionId> {
    let mut used = vec![false; removed.len()];
    let mut seeds = BTreeSet::new();
    for (region_id, content) in deletions {
        if consumed.contains(region_id) || content.is_empty() || content.len() > removed.len() {
            continue;
        }
        let span = content.len();
        for start in 0..=(removed.len() - span) {
            if used[start..start + span].iter().any(|taken| *taken)
                || &removed[start..start + span] != content.as_slice()
            {
                continue;
            }
            seeds.insert(*region_id);
            consumed.insert(*region_id);
            used[start..start + span].fill(true);
            break;
        }
    }
    seeds
}

/// Which kind of whole-file change this is.
fn opaque_reason(before: &FileState, after: &FileState) -> OpaqueReason {
    if before.exists() && after.exists() {
        OpaqueReason::BinaryOrUndecodable
    } else {
        OpaqueReason::Missing
    }
}

/// Whether `before -> after` has to be reviewed as one whole-file unit.
fn is_opaque_change(before: &FileState, after: &FileState) -> bool {
    let (Some(before_lines), Some(after_lines)) = (decode_lines(before), decode_lines(after))
    else {
        return true;
    };
    if !after.exists() {
        return true;
    }
    // An existence toggle with no textual difference, such as creating an empty
    // file, has no line hunk to show a reviewer, so the toggle itself is the
    // reviewable unit.
    before.exists() != after.exists() && before_lines == after_lines
}

/// The changes of one edit: its line hunks, or one whole-file unit.
fn compute_changes(edit: &Edit) -> Vec<(usize, Change)> {
    if is_opaque_change(&edit.before, &edit.after) {
        return vec![(
            0,
            Change::Opaque(OpaqueChange {
                reason: opaque_reason(&edit.before, &edit.after),
                baseline: edit.before.clone(),
                current: edit.after.clone(),
            }),
        )];
    }
    regions_between(&lines_of(&edit.before), &lines_of(&edit.after))
        .into_iter()
        .map(Change::Text)
        .enumerate()
        .collect()
}

/// The state a reconstruction takes its codec and line ending from: the
/// earliest concrete before, else the earliest concrete after.
fn reference_state(edits: &[&Edit]) -> FileState {
    edits
        .iter()
        .find(|edit| edit.before.exists())
        .map(|edit| edit.before.clone())
        .or_else(|| {
            edits
                .iter()
                .find(|edit| edit.after.exists())
                .map(|edit| edit.after.clone())
        })
        .unwrap_or_else(FileState::absent)
}

/// Everything the log can be asked, over a borrowed slice of it.
///
/// The borrow is the guarantee: a read model cannot exist while the log it
/// summarizes is being appended to, so its memo caches describe a log that
/// cannot have moved under them.
#[derive(Debug)]
pub struct History<'a> {
    events: &'a [Event],
    edits: RefCell<HashMap<String, Arc<Vec<&'a Edit>>>>,
    changes: RefCell<HashMap<u64, Changes>>,
    effective: RefCell<HashMap<String, Effective>>,
}

impl<'a> History<'a> {
    /// A read model over `events`.
    pub(super) fn new(events: &'a [Event]) -> Self {
        Self {
            events,
            edits: RefCell::new(HashMap::new()),
            changes: RefCell::new(HashMap::new()),
            effective: RefCell::new(HashMap::new()),
        }
    }

    /// How many edits this read model has already diffed.
    ///
    /// The memoization is otherwise invisible, and a cache that silently
    /// stopped working would only show up as a latency regression, so the test
    /// that asserts it reads the count directly.
    #[cfg(test)]
    pub(super) fn memoized_edits(&self) -> usize {
        self.changes.borrow().len()
    }

    /// Whether anything ever changed `path`.
    #[must_use]
    pub fn has_edits(&self, path: &str) -> bool {
        !self.edits_for(path).is_empty()
    }

    /// The file as the applied regions leave it.
    ///
    /// With `only_kept` the answer is the accepted baseline, holding the kept
    /// regions and nothing else; otherwise it is what belongs on disk, holding
    /// everything not reverted.
    #[must_use]
    pub fn project(&self, path: &str, only_kept: bool) -> FileState {
        self.reconstruct(path, &self.applied(path, only_kept))
    }

    /// What belongs on disk: every region except the reverted ones.
    #[must_use]
    pub fn content(&self, path: &str) -> FileState {
        self.project(path, false)
    }

    /// The baseline a reviewer has accepted: the kept regions only.
    #[must_use]
    pub fn accepted_baseline(&self, path: &str) -> FileState {
        self.project(path, true)
    }

    /// Whether every region of `path` has been decided.
    #[must_use]
    pub fn is_fully_reviewed(&self, path: &str) -> bool {
        self.effective(path)
            .values()
            .all(|decision| *decision != Decision::Pending)
    }

    /// The state `path` held before anything in this log touched it.
    #[must_use]
    pub fn original(&self, path: &str) -> FileState {
        for event in self.events {
            match event {
                Event::Edit(edit) if edit.path == path => return edit.before.clone(),
                Event::TurnMark(mark) => {
                    if let Some(state) = mark.pre.get(path) {
                        return state.clone();
                    }
                }
                _ => {}
            }
        }
        FileState::absent()
    }

    /// Every region of `path` in log order, each with the decision in force.
    #[must_use]
    pub fn regions(&self, path: &str) -> Vec<TurnRegion> {
        let effective = self.effective(path);
        let mut out = Vec::new();
        for edit in self.edits_for(path).iter() {
            for (ordinal, change) in self.changes_of(edit).iter() {
                let region_id = RegionId::new(edit.seq, *ordinal);
                out.push(TurnRegion {
                    region_id,
                    owner: edit.owner,
                    change: change.clone(),
                    decision: effective
                        .get(&region_id)
                        .copied()
                        .unwrap_or(Decision::Pending),
                    depends_on: edit.deps_of(*ordinal).to_vec(),
                });
            }
        }
        out
    }

    /// Each region of `path` with the decision in force for it.
    ///
    /// A region is reverted when it was reverted explicitly or when anything it
    /// depends on is; a region is kept when it was kept explicitly and nothing
    /// it depends on is reverted; everything else is pending.
    #[must_use]
    pub fn effective(&self, path: &str) -> Effective {
        let cached = self.effective.borrow().get(path).cloned();
        if let Some(cached) = cached {
            return cached;
        }
        let explicit = self.explicit_decisions(path);
        let order = self.hunk_order(path);
        let mut reverted: BTreeSet<RegionId> = BTreeSet::new();
        for (region_id, deps) in &order {
            if explicit.get(region_id) == Some(&Decision::Revert)
                || deps.iter().any(|dep| reverted.contains(dep))
            {
                reverted.insert(*region_id);
            }
        }
        let mut resolved = BTreeMap::new();
        for (region_id, _deps) in &order {
            let decision = if reverted.contains(region_id) {
                Decision::Revert
            } else if explicit.get(region_id) == Some(&Decision::Keep) {
                Decision::Keep
            } else {
                Decision::Pending
            };
            resolved.insert(*region_id, decision);
        }
        let resolved = Arc::new(resolved);
        self.effective
            .borrow_mut()
            .insert(path.to_owned(), Arc::clone(&resolved));
        resolved
    }

    /// Every region of `path` in log order with what it was built on.
    ///
    /// A region's dependencies always precede it here, which is what lets the
    /// closures below run in one pass.
    #[must_use]
    pub fn hunk_order(&self, path: &str) -> Vec<(RegionId, Vec<RegionId>)> {
        let mut out = Vec::new();
        for edit in self.edits_for(path).iter() {
            for (ordinal, _change) in self.changes_of(edit).iter() {
                out.push((
                    RegionId::new(edit.seq, *ordinal),
                    edit.deps_of(*ordinal).to_vec(),
                ));
            }
        }
        out
    }

    /// Every region of `path` with the owner that produced it.
    #[must_use]
    pub fn owned_hunks(&self, path: &str) -> Vec<(RegionId, Owner)> {
        let mut out = Vec::new();
        for edit in self.edits_for(path).iter() {
            for (ordinal, _change) in self.changes_of(edit).iter() {
                out.push((RegionId::new(edit.seq, *ordinal), edit.owner));
            }
        }
        out
    }

    /// What a new edit `before -> after` on `path` would be built on.
    ///
    /// `before` is the current projection, so its lines carry the provenance
    /// this reads. A hunk that replaces lines depends on whoever wrote them; a
    /// hunk that only inserts depends on the line it was anchored to, which is
    /// the line above unless it inserts at the very top. A change with no line
    /// structure is a whole-file barrier and depends on everything applied.
    #[must_use]
    pub fn compute_deps(
        &self,
        path: &str,
        before: &FileState,
        after: &FileState,
    ) -> BTreeMap<usize, Vec<RegionId>> {
        let applied = self.applied(path, false);
        if is_opaque_change(before, after) {
            return BTreeMap::from([(0, applied.into_iter().collect())]);
        }
        let before_lines = lines_of(before);
        let after_lines = lines_of(after);

        let prov = if self.has_edits(path) {
            self.reconstruct_with_prov(path, &applied).1
        } else {
            vec![None; before_lines.len()]
        };
        // A text edit written on top of an applied whole-file barrier depends on
        // it, so reverting the barrier drags the edit rather than resurrecting a
        // state that never existed.
        let barriers = self.applied_barriers(path, &applied);

        let mut deps = BTreeMap::new();
        for (ordinal, region) in regions_between(&before_lines, &after_lines)
            .into_iter()
            .enumerate()
        {
            let start = region.baseline_start;
            let end = region.baseline_end();
            let mut ids: BTreeSet<RegionId> = if end > start {
                (start..end)
                    .filter_map(|line| prov.get(line).copied().flatten())
                    .collect()
            } else {
                let anchor = start.saturating_sub(1);
                prov.get(anchor).copied().flatten().into_iter().collect()
            };
            ids.extend(barriers.iter().copied());
            deps.insert(ordinal, ids.into_iter().collect());
        }
        deps
    }

    /// The scope's still-pending change: its kept regions against its kept plus
    /// pending ones.
    ///
    /// [`None`] when the owner never touched the path, or has nothing left
    /// pending in it.
    #[must_use]
    pub fn scope_pending_diff(&self, path: &str, owner: Owner) -> Option<(FileState, FileState)> {
        let (position, kept, pending) = self.scope_split(path, owner)?;
        let prefix = self.applied_before(path, position);
        let baseline: BTreeSet<RegionId> = prefix.union(&kept).copied().collect();
        let after: BTreeSet<RegionId> = baseline.union(&pending).copied().collect();
        Some((
            self.reconstruct(path, &baseline),
            self.reconstruct(path, &after),
        ))
    }

    /// Every pending change of `path`, located in a rendered diff.
    ///
    /// With no owner the diff is the accepted baseline against what belongs on
    /// disk; with one it is that owner's own scope diff.
    #[must_use]
    pub fn pending_hunks(&self, path: &str, owner: Option<Owner>) -> Vec<HunkAnchor> {
        let view = match owner {
            None => self.all_view(path),
            Some(owner) => self.scope_view(path, owner),
        };
        let Some((baseline, current, pending)) = view else {
            return Vec::new();
        };
        self.anchor_pending(path, &baseline, &current, &pending)
    }

    // -- The log's shape -----------------------------------------------------

    /// Every path this log has touched, in the order it first touched it.
    ///
    /// A path counts as touched once a turn recorded a state for it, even when
    /// the turn went on to change nothing, because that recording is what a
    /// later drift is measured against.
    #[must_use]
    pub fn tracked_paths(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        let mut push = |path: &str| {
            if !seen.iter().any(|known| known == path) {
                seen.push(path.to_owned());
            }
        };
        for event in self.events {
            match event {
                Event::Edit(edit) => push(&edit.path),
                Event::TurnMark(mark) => {
                    for (path, _state) in mark.pre.iter() {
                        push(path);
                    }
                }
                Event::Decide(_) => {}
            }
        }
        seen
    }

    /// The paths the newest turn recorded a state for.
    ///
    /// This is what the next turn re-reads, so a file a tool mutated without
    /// announcing it is still captured.
    #[must_use]
    pub fn last_turn_paths(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(Event::as_mark)
            .next_back()
            .map(|mark| {
                mark.pre
                    .iter()
                    .map(|(path, _state)| path.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every owner that produced a change, in log order.
    ///
    /// This is the review slot list, and its order is a contract: a slot keeps
    /// its position for the life of the log, so resolving one never renumbers
    /// another under a client that is looking at both.
    #[must_use]
    pub fn scopes(&self) -> Vec<Owner> {
        let mut seen: Vec<Owner> = Vec::new();
        for edit in self.events.iter().filter_map(Event::as_edit) {
            if !seen.contains(&edit.owner) {
                seen.push(edit.owner);
            }
        }
        seen
    }

    /// Every turn identifier the log carries, in mark order.
    ///
    /// A reset can leave two marks carrying the same identifier, so this is a
    /// list rather than a set: it is the turn sequence a "last N turns" control
    /// counts back through, and dropping a repeat would make it count too far.
    #[must_use]
    pub fn turn_ids(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter_map(Event::as_mark)
            .map(|mark| mark.turn_id)
            .collect()
    }

    /// How many leading turns have every one of their regions kept.
    ///
    /// This is where a bulk decision starts counting: a turn a reviewer already
    /// accepted in full is behind the frontier, and asking for the last two
    /// turns means the last two that still have something to decide. A turn that
    /// produced no region at all is trivially accepted and does not stop the
    /// scan.
    #[must_use]
    pub fn accepted_turn_frontier(&self) -> usize {
        self.turn_ids()
            .into_iter()
            .take_while(|turn_id| self.turn_fully_kept(*turn_id))
            .count()
    }

    /// Whether every region the turn produced is in force as a keep.
    fn turn_fully_kept(&self, turn_id: u64) -> bool {
        let target = Owner::Agent { turn_id };
        self.tracked_paths().into_iter().all(|path| {
            let effective = self.effective(&path);
            self.owned_hunks(&path)
                .into_iter()
                .filter(|(_region_id, owner)| *owner == target)
                .all(|(region_id, _owner)| effective.get(&region_id) == Some(&Decision::Keep))
        })
    }

    /// Whether any mark carries `turn_id`.
    #[must_use]
    pub fn has_turn(&self, turn_id: u64) -> bool {
        self.events
            .iter()
            .filter_map(Event::as_mark)
            .any(|mark| mark.turn_id == turn_id)
    }

    /// Where a truncation to `turn_id` cuts the log.
    ///
    /// The newest exact match wins, because a transcript reset can reuse an
    /// identifier while an older mark still carries it. With no exact match the
    /// earliest later turn is the cut, so rewinding to a turn that never opened
    /// still drops everything after where it would have been.
    #[must_use]
    pub fn event_index_of_turn(&self, turn_id: u64) -> Option<usize> {
        let mut exact = None;
        let mut later = None;
        for (index, event) in self.events.iter().enumerate() {
            let Some(mark) = event.as_mark() else {
                continue;
            };
            if mark.turn_id == turn_id {
                exact = Some(index);
            } else if mark.turn_id > turn_id && later.is_none() {
                later = Some(index);
            }
        }
        exact.or(later)
    }

    /// What each file affected by a truncation at `index` should hold once the
    /// cut is applied, in the order the dropped events named them.
    ///
    /// A file a dropped turn touched restores to that turn's own recording,
    /// taking the earliest dropped turn's, which is its state when the cut
    /// began. A file only a dropped manual edit touched has no such recording,
    /// so it restores to what the kept log projects.
    #[must_use]
    pub fn restore_plan(&self, index: usize) -> Vec<(String, FileState)> {
        let index = index.min(self.events.len());
        let kept = Self::new(&self.events[..index]);
        let dropped = &self.events[index..];
        let mut plan: Vec<(String, FileState)> = Vec::new();
        for mark in dropped.iter().filter_map(Event::as_mark) {
            for (path, pre) in mark.pre.iter() {
                if !plan.iter().any(|(known, _state)| known == path) {
                    plan.push((path.to_owned(), pre.clone()));
                }
            }
        }
        for edit in dropped.iter().filter_map(Event::as_edit) {
            if !plan.iter().any(|(known, _state)| *known == edit.path) {
                plan.push((edit.path.clone(), kept.project(&edit.path, false)));
            }
        }
        plan
    }

    /// The restore plan for truncating at `turn_id`, empty when the log carries
    /// no such turn.
    #[must_use]
    pub fn restore_plan_to_turn(&self, turn_id: u64) -> Vec<(String, FileState)> {
        if !self.has_turn(turn_id) {
            return Vec::new();
        }
        self.event_index_of_turn(turn_id)
            .map(|index| self.restore_plan(index))
            .unwrap_or_default()
    }

    // -- Resolution ----------------------------------------------------------

    /// Every edit of `path`, in log order.
    fn edits_for(&self, path: &str) -> Arc<Vec<&'a Edit>> {
        let cached = self.edits.borrow().get(path).cloned();
        if let Some(cached) = cached {
            return cached;
        }
        let computed: Arc<Vec<&'a Edit>> = Arc::new(
            self.events
                .iter()
                .filter_map(Event::as_edit)
                .filter(|edit| edit.path == path)
                .collect(),
        );
        self.edits
            .borrow_mut()
            .insert(path.to_owned(), Arc::clone(&computed));
        computed
    }

    /// The changes of `edit`, diffed at most once per read model.
    fn changes_of(&self, edit: &Edit) -> Changes {
        let cached = self.changes.borrow().get(&edit.seq).cloned();
        if let Some(cached) = cached {
            return cached;
        }
        let computed = Arc::new(compute_changes(edit));
        self.changes
            .borrow_mut()
            .insert(edit.seq, Arc::clone(&computed));
        computed
    }

    /// Whether `edit` is a whole-file unit rather than a set of line hunks.
    fn is_opaque(&self, edit: &Edit) -> bool {
        self.changes_of(edit)
            .first()
            .is_some_and(|(_ordinal, change)| change.is_opaque())
    }

    /// What was recorded against each region of `path`.
    ///
    /// Revert is a ratchet: once a region is reverted, a later keep on it is
    /// ignored rather than resurrecting a change whose dependents may already
    /// have been dropped.
    fn explicit_decisions(&self, path: &str) -> BTreeMap<RegionId, Decision> {
        let mut decisions: BTreeMap<RegionId, Decision> = BTreeMap::new();
        for event in self.events {
            let Event::Decide(decided) = event else {
                continue;
            };
            if decided.path != path || decisions.get(&decided.hunk) == Some(&Decision::Revert) {
                continue;
            }
            decisions.insert(
                decided.hunk,
                if decided.keep {
                    Decision::Keep
                } else {
                    Decision::Revert
                },
            );
        }
        decisions
    }

    /// The regions a projection applies, dependency-closed.
    ///
    /// A region whose dependencies are not all applied is left out, because its
    /// span was computed against lines the projection does not carry.
    fn applied(&self, path: &str, only_kept: bool) -> BTreeSet<RegionId> {
        let effective = self.effective(path);
        let mut applied = BTreeSet::new();
        for (region_id, deps) in self.hunk_order(path) {
            let decision = effective
                .get(&region_id)
                .copied()
                .unwrap_or(Decision::Pending);
            if decision == Decision::Revert || (only_kept && decision != Decision::Keep) {
                continue;
            }
            if deps.iter().all(|dep| applied.contains(dep)) {
                applied.insert(region_id);
            }
        }
        applied
    }

    /// The regions applied by the edits preceding `position` in log order,
    /// which is the baseline one owner's own change sits on.
    ///
    /// List order rather than sequence order, because a drift captured during a
    /// turn is sealed before that turn's mark while carrying a higher sequence.
    fn applied_before(&self, path: &str, position: usize) -> BTreeSet<RegionId> {
        let effective = self.effective(path);
        let mut applied = BTreeSet::new();
        for edit in self.edits_for(path).iter().take(position) {
            for (ordinal, _change) in self.changes_of(edit).iter() {
                let region_id = RegionId::new(edit.seq, *ordinal);
                if effective.get(&region_id) == Some(&Decision::Revert) {
                    continue;
                }
                if edit
                    .deps_of(*ordinal)
                    .iter()
                    .all(|dep| applied.contains(dep))
                {
                    applied.insert(region_id);
                }
            }
        }
        applied
    }

    /// The applied whole-file barriers of `path`.
    fn applied_barriers(&self, path: &str, applied: &BTreeSet<RegionId>) -> BTreeSet<RegionId> {
        self.edits_for(path)
            .iter()
            .filter(|edit| self.is_opaque(edit))
            .map(|edit| RegionId::new(edit.seq, 0))
            .filter(|region_id| applied.contains(region_id))
            .collect()
    }

    /// The file rebuilt from the regions in `applied`.
    fn reconstruct(&self, path: &str, applied: &BTreeSet<RegionId>) -> FileState {
        let edits = self.edits_for(path);
        let Some(first) = edits.first() else {
            return self.original(path);
        };
        let reference = reference_state(&edits);
        let mut result = first.before.clone();
        for edit in edits.iter() {
            let changes = self.changes_of(edit);
            let here: Vec<&(usize, Change)> = changes
                .iter()
                .filter(|(ordinal, _change)| applied.contains(&RegionId::new(edit.seq, *ordinal)))
                .collect();
            if here.is_empty() {
                continue;
            }
            if self.is_opaque(edit) {
                result = edit.after.clone();
                continue;
            }
            let mut lines = lines_of(&result);
            let hunks: Vec<&Region> = here
                .iter()
                .filter_map(|(_ordinal, change)| change.as_text())
                .collect();
            splice(&mut lines, &lines_of(&edit.before), &hunks);
            result = reencode(&lines.concat(), &reference);
        }
        result
    }

    /// The text projection of `path`, with the region that last wrote each
    /// line beside it.
    ///
    /// An original line has no producer, which is how an insertion into an
    /// untouched file ends up depending on nothing.
    fn reconstruct_with_prov(
        &self,
        path: &str,
        applied: &BTreeSet<RegionId>,
    ) -> (Vec<String>, Vec<Option<RegionId>>) {
        let edits = self.edits_for(path);
        let mut result = edits
            .first()
            .map(|edit| lines_of(&edit.before))
            .unwrap_or_default();
        let mut prov: Vec<Option<RegionId>> = vec![None; result.len()];
        for edit in edits.iter() {
            let changes = self.changes_of(edit);
            let here: Vec<(RegionId, &Change)> = changes
                .iter()
                .map(|(ordinal, change)| (RegionId::new(edit.seq, *ordinal), change))
                .filter(|(region_id, _change)| applied.contains(region_id))
                .collect();
            if here.is_empty() {
                continue;
            }
            if self.is_opaque(edit) {
                result = lines_of(&edit.after);
                prov = vec![Some(RegionId::new(edit.seq, 0)); result.len()];
                continue;
            }
            let hunks: Vec<(RegionId, &Region)> = here
                .iter()
                .filter_map(|(region_id, change)| {
                    change.as_text().map(|region| (*region_id, region))
                })
                .collect();
            splice_with_prov(&mut result, &mut prov, &lines_of(&edit.before), &hunks);
        }
        (result, prov)
    }

    /// The baseline, current and pending region sets of the whole-file diff.
    fn all_view(
        &self,
        path: &str,
    ) -> Option<(BTreeSet<RegionId>, BTreeSet<RegionId>, BTreeSet<RegionId>)> {
        if !self.has_edits(path) {
            return None;
        }
        let effective = self.effective(path);
        let current = self.applied(path, false);
        let baseline = self.applied(path, true);
        let pending = current
            .iter()
            .copied()
            .filter(|region_id| effective.get(region_id) == Some(&Decision::Pending))
            .collect();
        Some((baseline, current, pending))
    }

    /// The same three sets for one owner's scope diff.
    fn scope_view(
        &self,
        path: &str,
        owner: Owner,
    ) -> Option<(BTreeSet<RegionId>, BTreeSet<RegionId>, BTreeSet<RegionId>)> {
        let (position, kept, pending) = self.scope_split(path, owner)?;
        let prefix = self.applied_before(path, position);
        let baseline: BTreeSet<RegionId> = prefix.union(&kept).copied().collect();
        let current: BTreeSet<RegionId> = baseline.union(&pending).copied().collect();
        Some((baseline, current, pending))
    }

    /// Where `owner`'s edit sits in the file's edit list, with its kept and its
    /// pending regions. [`None`] when it never touched the path or has nothing
    /// pending left in it.
    fn scope_split(
        &self,
        path: &str,
        owner: Owner,
    ) -> Option<(usize, BTreeSet<RegionId>, BTreeSet<RegionId>)> {
        let edits = self.edits_for(path);
        let position = edits.iter().position(|edit| edit.owner == owner)?;
        let edit = edits.get(position)?;
        let effective = self.effective(path);
        let mut kept = BTreeSet::new();
        let mut pending = BTreeSet::new();
        for (ordinal, _change) in self.changes_of(edit).iter() {
            let region_id = RegionId::new(edit.seq, *ordinal);
            match effective.get(&region_id) {
                Some(Decision::Keep) => {
                    kept.insert(region_id);
                }
                Some(Decision::Pending) => {
                    pending.insert(region_id);
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            return None;
        }
        Some((position, kept, pending))
    }

    /// The pending regions grouped into dependency-connected components.
    ///
    /// A control rendered on one hunk decides its whole component, so reverting
    /// it drags every hunk built on the same lines and keeping it pulls in the
    /// ones it was built on.
    fn pending_components(
        &self,
        path: &str,
        pending: &BTreeSet<RegionId>,
    ) -> BTreeMap<RegionId, BTreeSet<RegionId>> {
        let order: Vec<(RegionId, Vec<RegionId>)> = self
            .hunk_order(path)
            .into_iter()
            .filter(|(region_id, _deps)| pending.contains(region_id))
            .collect();
        let mut parent: HashMap<RegionId, RegionId> = order
            .iter()
            .map(|(region_id, _)| (*region_id, *region_id))
            .collect();

        fn find(parent: &mut HashMap<RegionId, RegionId>, start: RegionId) -> RegionId {
            let mut node = start;
            while let Some(&up) = parent.get(&node) {
                if up == node {
                    return node;
                }
                let grand = parent.get(&up).copied().unwrap_or(up);
                parent.insert(node, grand);
                node = grand;
            }
            node
        }

        for (region_id, deps) in &order {
            for dep in deps {
                if !parent.contains_key(dep) {
                    continue;
                }
                let root = find(&mut parent, *region_id);
                let target = find(&mut parent, *dep);
                parent.insert(root, target);
            }
        }
        let mut components: BTreeMap<RegionId, BTreeSet<RegionId>> = BTreeMap::new();
        for (region_id, _deps) in &order {
            let root = find(&mut parent, *region_id);
            components.entry(root).or_default().insert(*region_id);
        }
        order
            .iter()
            .map(|(region_id, _deps)| {
                let root = find(&mut parent, *region_id);
                (
                    *region_id,
                    components.get(&root).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Locates every pending region of `path` in the rendered
    /// `baseline -> current` diff.
    fn anchor_pending(
        &self,
        path: &str,
        baseline: &BTreeSet<RegionId>,
        current: &BTreeSet<RegionId>,
        pending: &BTreeSet<RegionId>,
    ) -> Vec<HunkAnchor> {
        let base_lines = lines_of(&self.reconstruct(path, baseline));
        let (cur_lines, cur_prov) = self.reconstruct_with_prov(path, current);
        let components = self.pending_components(path, pending);
        let deletions: Vec<(RegionId, Vec<String>)> = self
            .edits_for(path)
            .iter()
            .flat_map(|edit| {
                self.changes_of(edit)
                    .iter()
                    .filter_map(|(ordinal, change)| {
                        let region_id = RegionId::new(edit.seq, *ordinal);
                        let region = change.as_text()?;
                        (pending.contains(&region_id) && region.current_lines.is_empty())
                            .then(|| (region_id, region.baseline_lines.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        let mut anchors = Vec::new();
        let mut consumed = BTreeSet::new();
        for opcode in SequenceMatcher::new(&base_lines, &cur_lines).opcodes() {
            if opcode.tag == Tag::Equal {
                continue;
            }
            let mut seeds: BTreeSet<RegionId> = (opcode.j1..opcode.j2)
                .filter_map(|line| cur_prov.get(line).copied().flatten())
                .filter(|region_id| pending.contains(region_id))
                .collect();
            // The anchor sits on the block's last line, so an inline control
            // renders after the whole change rather than inside it. Both spans
            // of a non-equal opcode that reaches here are non-empty on the side
            // being read, so the subtraction never underflows.
            let (side, line) = if opcode.tag == Tag::Delete {
                seeds.extend(match_deletions(
                    &base_lines[opcode.i1..opcode.i2],
                    &deletions,
                    &mut consumed,
                ));
                (HunkSide::Deletions, opcode.i2.saturating_sub(1))
            } else {
                (HunkSide::Additions, opcode.j2.saturating_sub(1))
            };
            let mut regions: BTreeSet<RegionId> = BTreeSet::new();
            for seed in seeds {
                match components.get(&seed) {
                    Some(component) => regions.extend(component.iter().copied()),
                    None => {
                        regions.insert(seed);
                    }
                }
            }
            if !regions.is_empty() {
                anchors.push(HunkAnchor {
                    side,
                    line,
                    regions: regions.into_iter().collect(),
                });
            }
        }
        anchors
    }
}
