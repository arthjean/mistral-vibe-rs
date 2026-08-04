//! Name pattern matching for the configuration-driven tool filters.
//!
//! The reference matches `enabled_tools` and `disabled_tools` entries with
//! `name_matches` (`vibe/core/utils/matching.py`): an entry is a shell glob
//! unless it carries the `re:` prefix, in which case the remainder is a regular
//! expression that has to match the whole name. Both forms ignore case, blank
//! entries are skipped, and an expression that does not compile matches nothing
//! instead of failing the publication.
//!
//! A [`NameFilter`] compiles a list once so a publication over hundreds of tools
//! does not recompile the same expressions per name, and keeps the entries it
//! could not compile so the caller can report them.

use regex::Regex;

/// One compiled entry of a filter list.
#[derive(Debug, Clone)]
enum NamePattern {
    /// A glob, lowercased at compile time because matching ignores case.
    Glob(Vec<char>),
    Regex(Regex),
}

/// A compiled `enabled_tools` or `disabled_tools` list.
#[derive(Debug, Clone, Default)]
pub struct NameFilter {
    patterns: Vec<NamePattern>,
    invalid: Vec<String>,
}

impl NameFilter {
    /// Compiles `entries`, dropping blank ones and collecting the `re:` entries
    /// that do not compile into [`NameFilter::invalid`].
    #[must_use]
    pub fn new<S: AsRef<str>>(entries: &[S]) -> Self {
        let mut patterns = Vec::new();
        let mut invalid = Vec::new();
        for entry in entries {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            if let Some(expression) = entry.strip_prefix("re:") {
                // The reference compiles with `re.IGNORECASE` and calls
                // `fullmatch`, so the expression is anchored here rather than
                // relying on the caller to write `^...$`.
                match Regex::new(&format!("(?i)^(?:{expression})$")) {
                    Ok(compiled) => patterns.push(NamePattern::Regex(compiled)),
                    Err(_) => invalid.push(entry.to_owned()),
                }
            } else {
                patterns.push(NamePattern::Glob(entry.to_lowercase().chars().collect()));
            }
        }
        Self { patterns, invalid }
    }

    /// Whether the list carries no usable entry, which is what tells
    /// `enabled_tools` to narrow nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The entries that could not compile, in the order they were given.
    #[must_use]
    pub fn invalid(&self) -> &[String] {
        &self.invalid
    }

    /// Whether any entry matches `name`.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        let lowered = name.to_lowercase().chars().collect::<Vec<_>>();
        self.patterns.iter().any(|pattern| match pattern {
            NamePattern::Glob(glob) => matches_glob(glob, &lowered),
            NamePattern::Regex(expression) => expression.is_match(name),
        })
    }
}

/// `fnmatch` semantics over already-lowercased characters: `*` stands for any
/// run, `?` for exactly one, `[abc]` and `[a-z]` for a set, `[!abc]` for its
/// complement, and an unterminated `[` is a literal bracket.
fn matches_glob(pattern: &[char], value: &[char]) -> bool {
    let (mut pattern_index, mut value_index) = (0_usize, 0_usize);
    // Where to resume when a `*` has to swallow one more character.
    let mut star: Option<(usize, usize)> = None;
    loop {
        let candidate = value.get(value_index).copied();
        let matched = match pattern.get(pattern_index) {
            Some('*') => {
                star = Some((pattern_index, value_index));
                pattern_index = pattern_index.saturating_add(1);
                continue;
            }
            Some('?') => candidate.is_some(),
            Some('[') => match class_end(pattern, pattern_index) {
                Some(end) => {
                    let matched = candidate.is_some_and(|candidate| {
                        class_matches(
                            pattern
                                .get(pattern_index.saturating_add(1)..end)
                                .unwrap_or_default(),
                            candidate,
                        )
                    });
                    if matched {
                        pattern_index = end;
                    }
                    matched
                }
                None => candidate == Some('['),
            },
            Some(literal) => candidate == Some(*literal),
            None => {
                if value_index == value.len() {
                    return true;
                }
                false
            }
        };
        if matched {
            pattern_index = pattern_index.saturating_add(1);
            value_index = value_index.saturating_add(1);
            continue;
        }
        // Backtrack: the last `*` takes one more character, if there is one.
        let Some((star_index, consumed)) = star else {
            return false;
        };
        if consumed >= value.len() {
            return false;
        }
        pattern_index = star_index.saturating_add(1);
        value_index = consumed.saturating_add(1);
        star = Some((star_index, value_index));
    }
}

/// The index of the `]` closing the class opened at `start`, or `None` when the
/// pattern never closes it and the `[` is therefore a literal.
fn class_end(pattern: &[char], start: usize) -> Option<usize> {
    let mut index = start.saturating_add(1);
    if pattern.get(index) == Some(&'!') {
        index = index.saturating_add(1);
    }
    // A `]` in first position is a member of the class, not its terminator.
    if pattern.get(index) == Some(&']') {
        index = index.saturating_add(1);
    }
    while let Some(character) = pattern.get(index) {
        if *character == ']' {
            return Some(index);
        }
        index = index.saturating_add(1);
    }
    None
}

/// Whether `candidate` belongs to the class `body`, which excludes the brackets
/// and may open with `!` to negate the set.
fn class_matches(body: &[char], candidate: char) -> bool {
    let (negated, body) = match body.split_first() {
        Some(('!', rest)) => (true, rest),
        _ => (false, body),
    };
    let mut index = 0_usize;
    let mut found = false;
    while let Some(member) = body.get(index) {
        let range_end = if body.get(index.saturating_add(1)) == Some(&'-') {
            body.get(index.saturating_add(2)).copied()
        } else {
            None
        };
        if let Some(range_end) = range_end {
            if (*member..=range_end).contains(&candidate) {
                found = true;
            }
            index = index.saturating_add(3);
        } else {
            if *member == candidate {
                found = true;
            }
            index = index.saturating_add(1);
        }
    }
    found != negated
}

/// Every verdict asserted here was measured against the reference
/// `name_matches` at `vibe/core/utils/matching.py:16` on the pinned checkout,
/// including the cases where `fnmatch` is subtle: a dash in last position is a
/// class member rather than a range, and an unterminated `[` is a literal.
#[cfg(test)]
mod tests {
    use super::*;

    fn filter(entries: &[&str]) -> NameFilter {
        NameFilter::new(entries)
    }

    #[test]
    fn a_glob_entry_matches_every_name_it_covers() {
        let serena = filter(&["serena_*"]);
        assert!(serena.matches("serena_find"));
        assert!(serena.matches("serena_replace_symbol_body"));
        assert!(!serena.matches("read_file"));
        assert!(!serena.matches("mcp_serena_find"));
    }

    #[test]
    fn a_glob_entry_carries_the_single_character_and_class_forms() {
        assert!(filter(&["bash_?"]).matches("bash_1"));
        assert!(!filter(&["bash_?"]).matches("bash_12"));
        assert!(filter(&["[rw]ead*"]).matches("read_file"));
        assert!(!filter(&["[!rw]ead*"]).matches("read_file"));
        assert!(filter(&["read_file[0-9]"]).matches("read_file7"));
        // A dash in last position is a member, so `[a-]` never spans a range.
        assert!(!filter(&["[a-]b"]).matches("a-b"));
        // A `]` in first position is a member too, not the terminator.
        assert!(filter(&["[]]"]).matches("]"));
        assert!(!filter(&["[]]"]).matches("x"));
        // An unterminated class is a literal bracket, as it is in `fnmatch`.
        assert!(filter(&["read[file"]).matches("read[file"));
    }

    #[test]
    fn an_entry_prefixed_with_re_is_a_full_match_regular_expression() {
        let expression = filter(&["re:serena.*"]);
        assert!(expression.matches("serena_find"));
        assert!(!expression.matches("mcp_serena_find"));
        // `fullmatch` is what anchors the tail: a prefix is not enough.
        assert!(!filter(&["re:read"]).matches("read_file"));
        assert!(filter(&["re:read_(file|many)"]).matches("read_many"));
    }

    #[test]
    fn matching_ignores_case_in_both_forms() {
        assert!(filter(&["SERENA_*"]).matches("serena_find"));
        assert!(filter(&["serena_find"]).matches("Serena_Find"));
        assert!(filter(&["re:SERENA_.*"]).matches("serena_find"));
    }

    #[test]
    fn blank_entries_are_skipped_and_leave_the_list_empty() {
        let blank = filter(&["", "   "]);
        assert!(blank.is_empty());
        assert!(!blank.matches("read_file"));
        assert!(blank.invalid().is_empty());
        // Surrounding whitespace is stripped rather than making the entry miss.
        assert!(filter(&["  serena_*  "]).matches("serena_find"));
    }

    #[test]
    fn an_expression_that_does_not_compile_is_reported_and_matches_nothing() {
        let broken = filter(&["re:[", "read_file"]);
        assert_eq!(broken.invalid(), ["re:[".to_owned()]);
        assert!(broken.matches("read_file"));
        assert!(!broken.matches("["));
        assert!(!broken.is_empty());
    }

    #[test]
    fn a_star_matches_across_any_run_including_none() {
        assert!(filter(&["*"]).matches("anything"));
        assert!(filter(&["a*b"]).matches("ab"));
        assert!(filter(&["a*b"]).matches("azzb"));
        assert!(!filter(&["a*b"]).matches("zab"));
        assert!(filter(&["*_file"]).matches("read_file"));
        assert!(filter(&["*a*b*"]).matches("xaybz"));
    }
}
