//! The fuzzy matcher slash and path completion rank by.
//!
//! Reference `fuzzy_match` (`vibe/cli/autocompletion/fuzzy.py`, rewritten at
//! v2.24.5) works in two passes: an alignment pass picks which text positions
//! the pattern characters land on, then a ranking pass scores that alignment
//! on a scale independent of the pattern's length. The alignment is a dynamic
//! program over floating-point weights, so it runs here in `f64` with the same
//! operations in the same order, which is what makes it pick the same indices
//! on a tie. Every ranking weight is a multiple of one half, so the ranking
//! score is exact in integer hundredths.

/// Alignment weights: a match, the cost of opening and of extending a gap, the
/// bonus for landing on a word or camel-case boundary or on the same case, and
/// the per-position cost of starting late.
const ALIGN_MATCH: f64 = 1.0;
const ALIGN_GAP_OPEN: f64 = 3.0;
const ALIGN_GAP_EXTEND: f64 = 1.0;
const ALIGN_WORD_BOUNDARY: f64 = 1.0;
const ALIGN_CAMEL_BOUNDARY: f64 = 0.8;
const ALIGN_SAME_CASE: f64 = 0.2;
const ALIGN_LATE_START: f64 = 0.05;

/// Ranking weights, in hundredths.
const RANK_BASE: i64 = 10_000;
const RANK_PREFIX: i64 = 5_000;
const RANK_LATE_START: i64 = 200;
const RANK_ADJACENT: i64 = 1_000;
const RANK_WORD_BOUNDARY: i64 = 500;
const RANK_CAMEL_BOUNDARY: i64 = 300;
const RANK_SAME_CASE: i64 = 200;
const RANK_GAP: i64 = 150;

fn separates_words(character: char) -> bool {
    matches!(character, '/' | '-' | '_' | '.')
}

/// A string as the reference reads it: its lowercase form, indexed by
/// position, beside the original, indexed by the same positions. Python
/// lowercases the whole string and then indexes both with one offset, so a
/// character whose lowercase is longer shifts the original out of step; the
/// original is read with a bounds check where the reference reads it with a
/// length guard.
struct Folded {
    lower: Vec<char>,
    original: Vec<char>,
}

impl Folded {
    fn new(value: &str) -> Self {
        Self {
            lower: value.to_lowercase().chars().collect(),
            original: value.chars().collect(),
        }
    }

    fn is_upper(&self, index: usize) -> bool {
        self.original
            .get(index)
            .is_some_and(|character| character.is_uppercase())
    }

    fn follows_separator(&self, index: usize) -> bool {
        index == 0
            || self
                .lower
                .get(index - 1)
                .copied()
                .is_some_and(separates_words)
    }

    fn same_case(&self, index: usize, pattern: &Self, pattern_index: usize) -> bool {
        match (
            pattern.original.get(pattern_index),
            self.original.get(index),
        ) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }
}

/// Scores `pattern` against `text` in hundredths, or [`None`] when the pattern
/// is no subsequence of the text. An empty pattern matches with no score.
pub(super) fn fuzzy_match_score(pattern: &str, text: &str) -> Option<i64> {
    if pattern.is_empty() {
        return Some(0);
    }
    let pattern = Folded::new(pattern);
    let text = Folded::new(text);
    if pattern.lower.len() > text.lower.len() {
        return None;
    }
    let indices = align(&pattern, &text)?;
    Some(rank(&pattern, &text, &indices))
}

/// The alignment bonus for landing on text position `index`.
fn boundary_weight(text: &Folded, index: usize) -> f64 {
    if text.follows_separator(index) {
        ALIGN_WORD_BOUNDARY
    } else if text.is_upper(index) && !text.is_upper(index - 1) {
        ALIGN_CAMEL_BOUNDARY
    } else {
        0.0
    }
}

/// The best-scoring placement of every pattern character, or [`None`] when
/// there is none.
///
/// `best[row][column]` is the best score for the pattern's first `row + 1`
/// characters with the last one on text position `column`, and `came_from`
/// the column the previous character sat on. A gap is carried as a running
/// maximum per row, decayed by the extension cost at every step and replaced
/// when opening a gap from two columns back scores higher, so each cell costs
/// constant time.
fn align(pattern: &Folded, text: &Folded) -> Option<Vec<usize>> {
    let rows = pattern.lower.len();
    let columns = text.lower.len();
    let mut best = vec![vec![f64::NEG_INFINITY; columns]; rows];
    let mut came_from = vec![vec![None::<usize>; columns]; rows];
    for row in 0..rows {
        let mut gap = f64::NEG_INFINITY;
        let mut gap_from = None;
        // The characters still to place need a column each after this one.
        let last = columns - (rows - 1 - row);
        for column in row..last {
            if row > 0 && column > row {
                if gap > f64::NEG_INFINITY {
                    gap -= ALIGN_GAP_EXTEND;
                }
                let opened_at = column - 2;
                let previous = best[row - 1][opened_at];
                if previous > f64::NEG_INFINITY {
                    let opened = previous - ALIGN_GAP_OPEN;
                    if opened > gap {
                        gap = opened;
                        gap_from = Some(opened_at);
                    }
                }
            }
            if text.lower[column] != pattern.lower[row] {
                continue;
            }
            let case = if text.same_case(column, pattern, row) {
                ALIGN_SAME_CASE
            } else {
                0.0
            };
            let weight = ALIGN_MATCH + boundary_weight(text, column) + case;
            if row == 0 {
                best[row][column] = weight - column as f64 * ALIGN_LATE_START;
                continue;
            }
            let adjacent = best[row - 1][column - 1];
            if adjacent >= gap && adjacent > f64::NEG_INFINITY {
                best[row][column] = adjacent + weight;
                came_from[row][column] = Some(column - 1);
            } else if gap > f64::NEG_INFINITY {
                best[row][column] = gap + weight;
                came_from[row][column] = gap_from;
            }
        }
    }
    // Python's `max` keeps the first of equal scores.
    let final_row = &best[rows - 1];
    let mut end = 0;
    for (column, score) in final_row.iter().enumerate() {
        if *score > final_row[end] {
            end = column;
        }
    }
    if final_row[end] == f64::NEG_INFINITY {
        return None;
    }
    let mut indices = vec![0; rows];
    let mut column = end;
    for row in (0..rows).rev() {
        indices[row] = column;
        if row > 0 {
            column = came_from[row][column]?;
        }
    }
    Some(indices)
}

/// The ranking score of one alignment: a base that favors an early start,
/// bonuses for adjacent characters, boundaries and matching case, and a cost
/// for every skipped character.
fn rank(pattern: &Folded, text: &Folded, indices: &[usize]) -> i64 {
    let Some(&first) = indices.first() else {
        return 0;
    };
    let late_start = i64::try_from(first).unwrap_or(i64::MAX);
    let mut score = if first == 0 {
        RANK_BASE + RANK_PREFIX
    } else {
        RANK_BASE.saturating_sub(late_start.saturating_mul(RANK_LATE_START))
    };
    for pair in indices.windows(2) {
        if pair[1] == pair[0] + 1 {
            score += RANK_ADJACENT;
        }
        let skipped = i64::try_from(pair[1] - pair[0] - 1).unwrap_or(i64::MAX);
        score = score.saturating_sub(skipped.saturating_mul(RANK_GAP));
    }
    for (pattern_index, &index) in indices.iter().enumerate() {
        if text.follows_separator(index) {
            score += RANK_WORD_BOUNDARY;
        } else if text.is_upper(index) && !text.is_upper(index - 1) {
            score += RANK_CAMEL_BOUNDARY;
        }
        if text.same_case(index, pattern, pattern_index) {
            score += RANK_SAME_CASE;
        }
    }
    score.max(0)
}

#[cfg(test)]
mod tests {
    use super::fuzzy_match_score;

    #[test]
    fn lowercase_expansion_keeps_fuzzy_indices_safe() {
        assert!(fuzzy_match_score("a", "İa").is_some());
        assert!(fuzzy_match_score("i", "İa").is_some());
    }

    /// Scores the v2.24.5 reference gives, read off its `fuzzy_match` at the
    /// pin, in hundredths.
    #[test]
    fn scores_follow_the_rewritten_reference_matcher() {
        assert_eq!(fuzzy_match_score("mp", "mcp"), Some(15_750));
        assert_eq!(fuzzy_match_score("mp", "compact"), Some(11_000));
        assert_eq!(fuzzy_match_score("/e", "docs/guide.md"), Some(9_000));
        assert_eq!(fuzzy_match_score("/e", "src/inner/deep.rs"), Some(8_450));
        assert_eq!(fuzzy_match_score("zz", "mcp"), None);
        assert_eq!(fuzzy_match_score("", "mcp"), Some(0));
    }
}
