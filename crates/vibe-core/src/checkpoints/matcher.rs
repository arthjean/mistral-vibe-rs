//! The sequence matcher every region identity is numbered by.
//!
//! The reference builds its regions, its dependency provenance and its hunk
//! anchors from `difflib.SequenceMatcher(autojunk=False).get_opcodes()`, at
//! `vibe/core/checkpoints/history.py:53`, `:67` and `:675`. A region's ordinal
//! is its position in that opcode list, so reproducing the list is reproducing
//! the identity a client sends back; a diff of equal quality but different
//! shape would be a different protocol.
//!
//! This is a first-party implementation of the algorithm CPython publishes, not
//! a translation of reference source, and it is verified against opcodes
//! captured from the pinned checkout rather than trusted. Two properties of
//! that algorithm are load-bearing and easy to lose:
//!
//! - The match search is leftmost-longest. Ties are broken by the earliest
//!   position on both sides, which is why the length comparison below is a
//!   strict improvement and the loops run in ascending order.
//! - The popular-element heuristic is not applied. CPython drops any element of
//!   `b` occurring in more than one percent of a sequence of 200 or more, which
//!   speeds up matching at the cost of finding a different match; the reference
//!   turns that off at every call site, so it is absent here rather than
//!   configurable. A file of 200 lines is ordinary, so this is not an edge.
//!
//! Nothing here iterates a hash map, so two runs over the same input produce
//! the same opcodes.

use std::collections::HashMap;
use std::hash::Hash;

/// A run of `size` elements equal in both sequences, starting at `a` and `b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    /// Where the run starts in the first sequence.
    pub a: usize,
    /// Where the run starts in the second sequence.
    pub b: usize,
    /// How many elements the run covers, zero only for the terminating
    /// sentinel.
    pub size: usize,
}

/// What one opcode does to the span it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    /// Both spans hold the same elements.
    Equal,
    /// Both spans are non-empty and differ.
    Replace,
    /// The first sequence's span is dropped.
    Delete,
    /// The second sequence's span is added.
    Insert,
}

impl Tag {
    /// The name the reference emits for this tag, which the corpus records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Replace => "replace",
            Self::Delete => "delete",
            Self::Insert => "insert",
        }
    }
}

/// One step turning the first sequence into the second, over the half-open
/// spans `[i1, i2)` and `[j1, j2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opcode {
    /// What this step does.
    pub tag: Tag,
    /// Where the covered span starts in the first sequence.
    pub i1: usize,
    /// Where it ends, exclusive.
    pub i2: usize,
    /// Where the covered span starts in the second sequence.
    pub j1: usize,
    /// Where it ends, exclusive.
    pub j2: usize,
}

/// A matcher over two sequences, with the popular-element heuristic disabled.
///
/// Construction indexes the second sequence, so comparing one sequence against
/// several others is cheapest with the shared one held as `b`, which is how the
/// reference uses it.
#[derive(Debug)]
pub struct SequenceMatcher<'a, T> {
    a: &'a [T],
    b: &'a [T],
    /// Every position each element of `b` occupies, ascending. CPython purges
    /// the popular entries from this map; the reference asks it not to, so
    /// nothing is purged here.
    b2j: HashMap<&'a T, Vec<usize>>,
}

impl<'a, T: Hash + Eq> SequenceMatcher<'a, T> {
    /// A matcher turning `a` into `b`.
    #[must_use]
    pub fn new(a: &'a [T], b: &'a [T]) -> Self {
        let mut b2j: HashMap<&'a T, Vec<usize>> = HashMap::new();
        for (index, element) in b.iter().enumerate() {
            b2j.entry(element).or_default().push(index);
        }
        Self { a, b, b2j }
    }

    /// The longest run equal in `a[alo..ahi]` and `b[blo..bhi]`, earliest in `a`
    /// and then earliest in `b` among the runs of that length.
    ///
    /// The returned run is maximal: once the longest run is found it is grown
    /// outward while the neighbouring elements still agree, which is what keeps
    /// a run from being reported as two.
    #[must_use]
    pub fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
        // How long a run ending at each position of `b` is, for the row of `a`
        // just processed. Rebuilt per row, so a run only extends when both
        // sides advance by one.
        let mut lengths: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut next: HashMap<usize, usize> = HashMap::new();
            if let Some(positions) = self.a.get(i).and_then(|element| self.b2j.get(element)) {
                for &j in positions {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let run = j
                        .checked_sub(1)
                        .and_then(|previous| lengths.get(&previous).copied())
                        .unwrap_or(0)
                        + 1;
                    next.insert(j, run);
                    if run > bestsize {
                        besti = i + 1 - run;
                        bestj = j + 1 - run;
                        bestsize = run;
                    }
                }
            }
            lengths = next;
        }
        while besti > alo && bestj > blo && self.a.get(besti - 1) == self.b.get(bestj - 1) {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a.get(besti + bestsize) == self.b.get(bestj + bestsize)
        {
            bestsize += 1;
        }
        Match {
            a: besti,
            b: bestj,
            size: bestsize,
        }
    }

    /// Every matching run, in order, terminated by the empty sentinel
    /// `(a.len(), b.len(), 0)`.
    ///
    /// The sentinel is not decoration: [`Self::opcodes`] reads it to emit the
    /// trailing insert, delete or replace after the last match, and a caller
    /// walking the runs pairwise needs the same terminator.
    #[must_use]
    pub fn matching_blocks(&self) -> Vec<Match> {
        let mut found = Vec::new();
        let mut pending = vec![(0, self.a.len(), 0, self.b.len())];
        while let Some((alo, ahi, blo, bhi)) = pending.pop() {
            let matched = self.find_longest_match(alo, ahi, blo, bhi);
            if matched.size == 0 {
                continue;
            }
            found.push(matched);
            if alo < matched.a && blo < matched.b {
                pending.push((alo, matched.a, blo, matched.b));
            }
            if matched.a + matched.size < ahi && matched.b + matched.size < bhi {
                pending.push((matched.a + matched.size, ahi, matched.b + matched.size, bhi));
            }
        }
        found.sort_unstable();

        // Recursion can leave two runs touching, which would surface as two
        // `equal` opcodes in a row. Collapsing them is what makes the opcode
        // list canonical.
        let mut collapsed: Vec<Match> = Vec::with_capacity(found.len() + 1);
        for block in found {
            match collapsed.last_mut() {
                Some(previous)
                    if previous.a + previous.size == block.a
                        && previous.b + previous.size == block.b =>
                {
                    previous.size += block.size;
                }
                _ => collapsed.push(block),
            }
        }
        collapsed.push(Match {
            a: self.a.len(),
            b: self.b.len(),
            size: 0,
        });
        collapsed
    }

    /// The ordered steps turning `a` into `b`.
    ///
    /// The spans tile both sequences with no gap and no overlap, and no two
    /// adjacent steps carry the same tag, so a caller can number the non-equal
    /// steps and use the position as an identity.
    #[must_use]
    pub fn opcodes(&self) -> Vec<Opcode> {
        let mut opcodes = Vec::new();
        let (mut i, mut j) = (0, 0);
        for block in self.matching_blocks() {
            let tag = match (i < block.a, j < block.b) {
                (true, true) => Some(Tag::Replace),
                (true, false) => Some(Tag::Delete),
                (false, true) => Some(Tag::Insert),
                (false, false) => None,
            };
            if let Some(tag) = tag {
                opcodes.push(Opcode {
                    tag,
                    i1: i,
                    i2: block.a,
                    j1: j,
                    j2: block.b,
                });
            }
            i = block.a + block.size;
            j = block.b + block.size;
            if block.size > 0 {
                opcodes.push(Opcode {
                    tag: Tag::Equal,
                    i1: block.a,
                    i2: i,
                    j1: block.b,
                    j2: j,
                });
            }
        }
        opcodes
    }
}
