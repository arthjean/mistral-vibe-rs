//! The rendered side of the read model: the diffs a review panel shows, and
//! where each pending region sits in them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{History, lines_of};
use crate::checkpoints::lines::FileState;
use crate::checkpoints::matcher::{SequenceMatcher, Tag};
use crate::checkpoints::models::{Decision, HunkAnchor, HunkSide, Owner, RegionId};

/// The region sets one rendered diff is built from: those applied on its
/// left-hand side, those applied on its right, and the pending ones among the
/// latter, which are the ones a control decides.
struct DiffView {
    baseline: BTreeSet<RegionId>,
    current: BTreeSet<RegionId>,
    pending: BTreeSet<RegionId>,
}

impl History<'_> {
    /// The scope's still-pending change: its kept regions against its kept plus
    /// pending ones.
    ///
    /// [`None`] when the owner never touched the path, or has nothing left
    /// pending in it.
    #[must_use]
    pub fn scope_pending_diff(&self, path: &str, owner: Owner) -> Option<(FileState, FileState)> {
        let view = self.scope_view(path, owner)?;
        Some((
            self.reconstruct(path, &view.baseline),
            self.reconstruct(path, &view.current),
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
        let Some(view) = view else {
            return Vec::new();
        };
        self.anchor_pending(path, &view)
    }

    /// The whole-file diff: the accepted baseline against what belongs on disk.
    fn all_view(&self, path: &str) -> Option<DiffView> {
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
        Some(DiffView {
            baseline,
            current,
            pending,
        })
    }

    /// One owner's scope diff: its kept regions against its kept plus pending
    /// ones, both on top of everything applied before its edit.
    fn scope_view(&self, path: &str, owner: Owner) -> Option<DiffView> {
        let (position, kept, pending) = self.scope_split(path, owner)?;
        let prefix = self.applied_before(path, position);
        let baseline: BTreeSet<RegionId> = prefix.union(&kept).copied().collect();
        let current: BTreeSet<RegionId> = baseline.union(&pending).copied().collect();
        Some(DiffView {
            baseline,
            current,
            pending,
        })
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
    /// `baseline -> current` diff of `view`.
    fn anchor_pending(&self, path: &str, view: &DiffView) -> Vec<HunkAnchor> {
        let pending = &view.pending;
        let base_lines = lines_of(&self.reconstruct(path, &view.baseline));
        let (cur_lines, cur_prov) = self.reconstruct_with_prov(path, &view.current);
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
