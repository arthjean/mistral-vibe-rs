#[derive(Debug)]
struct FoldedText {
    lower: Vec<char>,
    original: Vec<char>,
    source_index: Vec<usize>,
}

impl FoldedText {
    fn new(value: &str) -> Self {
        let original = value.chars().collect::<Vec<_>>();
        let mut lower = Vec::new();
        let mut source_index = Vec::new();
        for (index, character) in original.iter().copied().enumerate() {
            for folded in character.to_lowercase() {
                lower.push(folded);
                source_index.push(index);
            }
        }
        Self {
            lower,
            original,
            source_index,
        }
    }

    fn original_at(&self, folded_index: usize) -> Option<char> {
        let source = *self.source_index.get(folded_index)?;
        self.original.get(source).copied()
    }

    fn is_case_boundary(&self, folded_index: usize) -> bool {
        if folded_index == 0 {
            return true;
        }
        let Some(current_source) = self.source_index.get(folded_index).copied() else {
            return false;
        };
        let Some(previous_source) = self
            .source_index
            .get(folded_index.saturating_sub(1))
            .copied()
        else {
            return false;
        };
        if current_source == previous_source {
            return false;
        }
        self.original
            .get(current_source)
            .zip(self.original.get(previous_source))
            .is_some_and(|(current, previous)| current.is_uppercase() && !previous.is_uppercase())
    }
}

/// Integer-exact port of the reference fuzzy matcher.
///
/// Scores use hundredths so the reference's `1.5`, `1.3`, and `1.8` factors
/// remain exact without partial floating-point ordering. Lowercase expansion
/// keeps a source-index map so Unicode filenames cannot desynchronize indices.
pub(super) fn fuzzy_match_score(pattern: &str, text: &str) -> Option<i64> {
    if pattern.is_empty() {
        return Some(0);
    }
    let pattern = FoldedText::new(pattern);
    let text = FoldedText::new(text);
    if pattern.lower.len() > text.lower.len() {
        return None;
    }
    if text.lower.starts_with(&pattern.lower) {
        let indices = (0..pattern.lower.len()).collect::<Vec<_>>();
        return Some(fuzzy_base_score(&pattern, &text, &indices) * 20);
    }

    let mut scores = Vec::new();
    if let Some(indices) = fuzzy_word_boundary_indices(&pattern.lower, &text) {
        scores.push(fuzzy_base_score(&pattern, &text, &indices) * 18);
    }
    if let Some(indices) = fuzzy_consecutive_indices(&pattern.lower, &text.lower) {
        scores.push(fuzzy_base_score(&pattern, &text, &indices) * 13);
    }
    if let Some(indices) = fuzzy_subsequence_indices(&pattern.lower, &text.lower) {
        scores.push(fuzzy_base_score(&pattern, &text, &indices) * 10);
    }
    scores.into_iter().max()
}

fn fuzzy_word_boundary_indices(pattern: &[char], text: &FoldedText) -> Option<Vec<usize>> {
    let mut indices = Vec::new();
    let mut pattern_index = 0usize;
    for (index, character) in text.lower.iter().enumerate() {
        if pattern_index >= pattern.len() {
            break;
        }
        let boundary = index == 0
            || matches!(text.lower[index - 1], '/' | '-' | '_' | '.')
            || text.is_case_boundary(index);
        if *character == pattern[pattern_index]
            && (boundary
                || indices.last().is_some_and(|last| index == *last + 1)
                || indices.is_empty())
        {
            indices.push(index);
            pattern_index = pattern_index.saturating_add(1);
        }
    }
    (pattern_index == pattern.len()).then_some(indices)
}

fn fuzzy_consecutive_indices(pattern: &[char], text: &[char]) -> Option<Vec<usize>> {
    let mut indices = Vec::new();
    let mut pattern_index = 0usize;
    for (index, character) in text.iter().enumerate() {
        if pattern_index >= pattern.len() {
            break;
        }
        if *character == pattern[pattern_index] {
            indices.push(index);
            pattern_index = pattern_index.saturating_add(1);
        } else if !indices.is_empty() {
            indices.clear();
            pattern_index = 0;
        }
    }
    (pattern_index == pattern.len()).then_some(indices)
}

fn fuzzy_subsequence_indices(pattern: &[char], text: &[char]) -> Option<Vec<usize>> {
    let mut indices = Vec::new();
    let mut pattern_index = 0usize;
    for (index, character) in text.iter().enumerate() {
        if pattern_index >= pattern.len() {
            break;
        }
        if *character == pattern[pattern_index] {
            indices.push(index);
            pattern_index = pattern_index.saturating_add(1);
        }
    }
    (pattern_index == pattern.len()).then_some(indices)
}

/// Returns the reference base score in tenths.
fn fuzzy_base_score(pattern: &FoldedText, text: &FoldedText, indices: &[usize]) -> i64 {
    let Some(first) = indices.first().copied() else {
        return 0;
    };
    let mut score = if first == 0 {
        1_500
    } else {
        1_000_i64.saturating_sub(i64::try_from(first).unwrap_or(i64::MAX) * 20)
    };
    for pair in indices.windows(2) {
        if pair[1] == pair[0].saturating_add(1) {
            score = score.saturating_add(100);
        }
        let gap = pair[1].saturating_sub(pair[0]).saturating_sub(1);
        score = score.saturating_sub(i64::try_from(gap).unwrap_or(i64::MAX) * 15);
    }
    for (pattern_index, text_index) in indices.iter().copied().enumerate() {
        if text_index == 0 || matches!(text.lower[text_index - 1], '/' | '-' | '_' | '.') {
            score = score.saturating_add(50);
        } else if text.is_case_boundary(text_index) {
            score = score.saturating_add(30);
        }
        if pattern.original_at(pattern_index) == text.original_at(text_index) {
            score = score.saturating_add(20);
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
}
