//! Ranking indexed paths against the text after `@`: reference
//! `PathCompleter._score_matches` and the entry masks that let a query skip
//! the matcher for an entry that cannot match.

use super::super::CompletionCandidate;
use super::super::fuzzy::fuzzy_match_score;
use super::{IndexedPath, MAX_PATH_MATCHES, mention_candidate};

/// The codepoint the reference's entry mask stops at, so the mask is one bit
/// per ASCII codepoint and a wider codepoint contributes none.
pub(super) const ASCII_CODEPOINT_LIMIT: u32 = 128;

/// One bit per ASCII codepoint in `value`, matching `build_ascii_mask`.
pub(super) fn build_ascii_mask(value: &str) -> u128 {
    value
        .chars()
        .map(u32::from)
        .filter(|codepoint| *codepoint < ASCII_CODEPOINT_LIMIT)
        .fold(0_u128, |mask, codepoint| mask | (1_u128 << codepoint))
}

/// The mask a query must have every bit of, or `None` when the query carries a
/// codepoint the mask cannot represent and no filtering may apply.
pub(super) fn query_ascii_mask(pattern: &str) -> Option<u128> {
    if pattern
        .chars()
        .any(|character| u32::from(character) >= ASCII_CODEPOINT_LIMIT)
    {
        return None;
    }
    Some(build_ascii_mask(&pattern.to_lowercase()))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct PathMatchRank {
    pub(super) exact_directory: bool,
    pub(super) immediate_child_of_exact_path: bool,
    pub(super) exact_filename: bool,
    pub(super) preferred_stem_match: bool,
    pub(super) exact_stem: bool,
    pub(super) stem_prefix: bool,
    pub(super) name_prefix: bool,
    pub(super) extension_match: bool,
    pub(super) fuzzy_score: i64,
    pub(super) shallow_path: i32,
}

pub(super) struct PathSearchContext<'a> {
    pub(super) suffix: &'a str,
    pub(super) search_pattern: &'a str,
    pub(super) path_prefix: &'a str,
    pub(super) immediate_only: bool,
    pub(super) search_pattern_ascii_mask: Option<u128>,
}

#[cfg(test)]
pub(super) fn rank_indexed_paths<'a>(
    entries: impl IntoIterator<Item = &'a IndexedPath>,
    raw_query: &str,
    mask_filter: bool,
) -> Vec<CompletionCandidate> {
    rank_with_context(entries, &path_search_context(raw_query), mask_filter)
}

/// Reference `PathCompleter._score_matches`: every entry is ranked, an empty
/// pattern stops at the target count, and the answer is sorted by label and
/// then by descending rank, so equal ranks keep the label order.
pub(super) fn rank_with_context<'a>(
    entries: impl IntoIterator<Item = &'a IndexedPath>,
    context: &PathSearchContext<'_>,
    mask_filter: bool,
) -> Vec<CompletionCandidate> {
    let mut matches = Vec::<(CompletionCandidate, PathMatchRank)>::new();
    for entry in entries {
        if !path_matches_prefix(entry, context)
            || entry.name.starts_with('.') && !context.suffix.starts_with('.')
        {
            continue;
        }
        if context.search_pattern.is_empty() {
            matches.push((
                mention_candidate(entry.rel.clone(), entry.is_directory),
                path_match_rank(entry, context, 0),
            ));
            if matches.len() >= MAX_PATH_MATCHES {
                break;
            }
            continue;
        }
        if mask_filter && !can_possibly_fuzzy_match(entry, context) {
            continue;
        }
        let Some(score) = fuzzy_match_score(context.search_pattern, &entry.rel) else {
            continue;
        };
        matches.push((
            mention_candidate(entry.rel.clone(), entry.is_directory),
            path_match_rank(entry, context, score),
        ));
    }
    matches.sort_by(|left, right| left.0.label.cmp(&right.0.label));
    matches.sort_by(|left, right| right.1.cmp(&left.1));
    matches
        .into_iter()
        .take(MAX_PATH_MATCHES)
        .map(|(candidate, _)| candidate)
        .collect()
}

pub(super) fn path_search_context(raw_query: &str) -> PathSearchContext<'_> {
    let suffix = raw_query.rsplit('/').next().unwrap_or(raw_query);
    if raw_query.is_empty() {
        return PathSearchContext {
            suffix,
            search_pattern: "",
            path_prefix: "",
            immediate_only: true,
            search_pattern_ascii_mask: None,
        };
    }
    if raw_query.ends_with('/') {
        return PathSearchContext {
            suffix,
            search_pattern: "",
            path_prefix: raw_query,
            immediate_only: true,
            search_pattern_ascii_mask: None,
        };
    }
    PathSearchContext {
        suffix,
        search_pattern: raw_query,
        path_prefix: "",
        immediate_only: false,
        search_pattern_ascii_mask: query_ascii_mask(raw_query),
    }
}

/// Whether `entry` carries every ASCII codepoint the query needs. A query the
/// mask cannot represent filters nothing, so every entry reaches the matcher.
pub(super) fn can_possibly_fuzzy_match(
    entry: &IndexedPath,
    context: &PathSearchContext<'_>,
) -> bool {
    context
        .search_pattern_ascii_mask
        .is_none_or(|mask| entry.ascii_mask & mask == mask)
}

fn path_matches_prefix(entry: &IndexedPath, context: &PathSearchContext<'_>) -> bool {
    if !context.path_prefix.is_empty() {
        let prefix = context.path_prefix.trim_end_matches('/');
        if entry.rel == prefix && entry.is_directory {
            return false;
        }
        return is_immediate_child(&entry.rel, context.path_prefix);
    }
    !context.immediate_only || !entry.rel.contains('/')
}

fn is_immediate_child(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    let prefix_with_slash = format!("{prefix}/");
    let after_prefix = if let Some(value) = path.strip_prefix(&prefix_with_slash) {
        value
    } else if let Some(index) = path.find(&prefix_with_slash) {
        if index > 0 && path.as_bytes().get(index.saturating_sub(1)) != Some(&b'/') {
            return false;
        }
        &path[index.saturating_add(prefix_with_slash.len())..]
    } else {
        return false;
    };
    !after_prefix.is_empty() && !after_prefix.contains('/')
}

pub(super) fn path_match_rank(
    entry: &IndexedPath,
    context: &PathSearchContext<'_>,
    fuzzy_score: i64,
) -> PathMatchRank {
    let query = context.suffix.to_lowercase();
    let depth = i32::try_from(entry.rel.matches('/').count()).unwrap_or(i32::MAX);
    if query.is_empty() {
        return PathMatchRank {
            exact_directory: false,
            immediate_child_of_exact_path: false,
            exact_filename: false,
            preferred_stem_match: false,
            exact_stem: false,
            stem_prefix: false,
            name_prefix: false,
            extension_match: false,
            fuzzy_score,
            shallow_path: depth.saturating_neg(),
        };
    }
    let name = entry.name.to_lowercase();
    let (stem, extension) = stem_and_extension(&name);
    let (query_stem, query_extension) = stem_and_extension(&query);
    let query_looks_like_filename = query.contains('.');
    let query_looks_like_path = context.search_pattern.contains('/');
    let search_lower = context.search_pattern.to_lowercase();
    PathMatchRank {
        exact_directory: entry.is_directory && entry.rel_lower == search_lower,
        immediate_child_of_exact_path: query_looks_like_path
            && is_immediate_child(&entry.rel_lower, &search_lower),
        exact_filename: query_looks_like_filename && name == query,
        preferred_stem_match: stem == query && extension != ".lock",
        exact_stem: stem == query || query_looks_like_filename && stem == query_stem,
        stem_prefix: stem.starts_with(if query_looks_like_filename {
            &query_stem
        } else {
            &query
        }),
        name_prefix: name.starts_with(&query),
        extension_match: !query_extension.is_empty() && extension == query_extension,
        fuzzy_score,
        shallow_path: depth.saturating_neg(),
    }
}

/// Splits a path component into its stem and its extension, the way the
/// reference's `Path(value).stem` and `.suffix` do.
///
/// `pathlib` reads the component's name first, and the name of `.` is empty,
/// so a query of `@.` has an empty stem upstream rather than a stem of `.`.
/// Ranking then reports `stem_prefix` for every candidate, since every stem
/// starts with the empty string. The autocompletion corpus measures that case.
fn stem_and_extension(name: &str) -> (String, String) {
    let name = if name == "." { "" } else { name };
    let Some(dot) = name.rfind('.') else {
        return (name.to_owned(), String::new());
    };
    if dot == 0 || dot == name.len().saturating_sub(1) {
        return (name.to_owned(), String::new());
    }
    (name[..dot].to_owned(), name[dot..].to_owned())
}
