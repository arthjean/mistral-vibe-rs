use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::fuzzy::fuzzy_match_score;
use super::{CompletionCandidate, CompletionKind};
use crate::tui::input::InputError;

const MAX_INDEXED_ENTRIES: usize = 32_000;
const MAX_PATH_MATCHES: usize = 100;

#[derive(Debug)]
struct IndexedPath {
    rel: String,
    rel_lower: String,
    name: String,
    is_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PathMatchRank {
    exact_directory: bool,
    immediate_child_of_exact_path: bool,
    exact_filename: bool,
    preferred_stem_match: bool,
    exact_stem: bool,
    stem_prefix: bool,
    name_prefix: bool,
    extension_match: bool,
    fuzzy_score: i64,
    shallow_path: i32,
}

struct PathSearchContext<'a> {
    suffix: &'a str,
    search_pattern: &'a str,
    path_prefix: &'a str,
    immediate_only: bool,
}

#[derive(Debug, Default)]
pub(super) struct WorkspaceIndex {
    root: Option<PathBuf>,
    entries: Vec<IndexedPath>,
    rebuilds: usize,
}

impl WorkspaceIndex {
    pub(super) fn candidates(
        &mut self,
        workspace: &Path,
        raw_query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        let root = fs::canonicalize(workspace)
            .map_err(|error| InputError::Workspace(error.to_string()))?;
        if self.root.as_deref() != Some(root.as_path()) {
            let mut entries = index_workspace(&root);
            entries.sort_by(|left, right| left.rel.cmp(&right.rel));
            self.root = Some(root);
            self.entries = entries;
            self.rebuilds = self.rebuilds.saturating_add(1);
        }
        Ok(rank_indexed_paths(&self.entries, raw_query))
    }

    #[cfg(test)]
    fn rebuilds(&self) -> usize {
        self.rebuilds
    }
}

pub(super) fn path_candidates(
    workspace: &Path,
    raw_query: &str,
) -> Result<Vec<CompletionCandidate>, InputError> {
    WorkspaceIndex::default().candidates(workspace, raw_query)
}

fn rank_indexed_paths(entries: &[IndexedPath], raw_query: &str) -> Vec<CompletionCandidate> {
    let context = path_search_context(raw_query);
    let mut matches = Vec::<(CompletionCandidate, PathMatchRank)>::new();
    for entry in entries.iter().take(MAX_INDEXED_ENTRIES) {
        if !path_matches_prefix(entry, &context)
            || entry.name.starts_with('.') && !context.suffix.starts_with('.')
        {
            continue;
        }
        if context.search_pattern.is_empty() {
            matches.push((
                mention_candidate(entry.rel.clone(), entry.is_directory),
                path_match_rank(entry, &context, 0),
            ));
            if matches.len() >= MAX_PATH_MATCHES {
                break;
            }
            continue;
        }
        let Some(score) = fuzzy_match_score(context.search_pattern, &entry.rel) else {
            continue;
        };
        matches.push((
            mention_candidate(entry.rel.clone(), entry.is_directory),
            path_match_rank(entry, &context, score),
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

fn path_search_context(raw_query: &str) -> PathSearchContext<'_> {
    let suffix = raw_query.rsplit('/').next().unwrap_or(raw_query);
    if raw_query.is_empty() {
        return PathSearchContext {
            suffix,
            search_pattern: "",
            path_prefix: "",
            immediate_only: true,
        };
    }
    if raw_query.ends_with('/') {
        return PathSearchContext {
            suffix,
            search_pattern: "",
            path_prefix: raw_query,
            immediate_only: true,
        };
    }
    PathSearchContext {
        suffix,
        search_pattern: raw_query,
        path_prefix: "",
        immediate_only: false,
    }
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

fn path_match_rank(
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

fn index_workspace(root: &Path) -> Vec<IndexedPath> {
    let rules = IgnoreRules::load(root);
    let mut entries = Vec::new();
    walk_workspace(root, "", &rules, &mut entries);
    entries
}

fn walk_workspace(
    directory: &Path,
    prefix: &str,
    rules: &IgnoreRules,
    output: &mut Vec<IndexedPath>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_directory = file_type.is_dir();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if rules.should_ignore(&rel, &name, is_directory) {
            continue;
        }
        output.push(IndexedPath {
            rel_lower: rel.to_lowercase(),
            rel: rel.clone(),
            name,
            is_directory,
        });
        // Never follow symlinks. This includes the link as a file-like entry
        // while making cycles impossible, matching `follow_symlinks=False`.
        if is_directory && !file_type.is_symlink() {
            walk_workspace(&entry.path(), &rel, rules, output);
        }
    }
}

const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".git/",
    "__pycache__/",
    "node_modules/",
    ".DS_Store",
    "*.pyc",
    "*.log",
    ".vscode/",
    ".idea/",
    "/build/",
    "dist/",
    "target/",
    ".next/",
    ".nuxt/",
    "coverage/",
    ".nyc_output/",
    "*.egg-info",
    ".pytest_cache/",
    ".tox/",
    "vendor/",
    "third_party/",
    "deps/",
    "*.min.js",
    "*.min.css",
    "*.bundle.js",
    "*.chunk.js",
    ".cache/",
    "tmp/",
    "temp/",
    "logs/",
    ".uv-cache/",
    ".ruff_cache/",
    ".venv/",
    "venv/",
    ".mypy_cache/",
    "htmlcov/",
    ".coverage",
];

struct IgnoreRule {
    pattern: String,
    excludes: bool,
    directory_only: bool,
    name_only: bool,
    anchored_at_root: bool,
}

struct IgnoreRules {
    rules: Vec<IgnoreRule>,
}

impl IgnoreRules {
    fn load(root: &Path) -> Self {
        let mut rules = DEFAULT_IGNORE_PATTERNS
            .iter()
            .filter_map(|pattern| IgnoreRule::parse(pattern, true))
            .collect::<Vec<_>>();
        if let Ok(contents) = fs::read_to_string(root.join(".gitignore")) {
            for line in contents.lines() {
                let mut raw = line.trim();
                if raw.is_empty() || raw.starts_with('#') {
                    continue;
                }
                if let Some((before, _)) = raw.split_once('#') {
                    raw = before.trim_end();
                }
                if raw.is_empty() {
                    continue;
                }
                let excludes = !raw.starts_with('!');
                if !excludes {
                    raw = raw.trim_start_matches('!').trim_start();
                }
                if let Some(rule) = IgnoreRule::parse(raw, excludes) {
                    rules.push(rule);
                }
            }
        }
        Self { rules }
    }

    fn should_ignore(&self, rel: &str, name: &str, is_directory: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(rel, name, is_directory) {
                ignored = rule.excludes;
            }
        }
        ignored
    }
}

impl IgnoreRule {
    fn parse(raw: &str, excludes: bool) -> Option<Self> {
        let anchored_at_root = raw.starts_with('/');
        let raw = raw.trim_start_matches('/');
        let directory_only = raw.ends_with('/');
        let pattern = raw.trim_end_matches('/');
        if pattern.is_empty() {
            return None;
        }
        Some(Self {
            pattern: pattern.to_owned(),
            excludes,
            directory_only,
            name_only: !pattern.contains('/'),
            anchored_at_root,
        })
    }

    fn matches(&self, rel: &str, name: &str, is_directory: bool) -> bool {
        if self.directory_only && !is_directory {
            return false;
        }
        let target = if self.name_only {
            if self.anchored_at_root && rel.contains('/') {
                return false;
            }
            name
        } else {
            rel
        };
        glob_matches(&self.pattern, target)
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    fn visit(
        pattern: &[char],
        value: &[char],
        pattern_index: usize,
        value_index: usize,
        memo: &mut BTreeMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, value_index)) {
            return *result;
        }
        let result = match pattern.get(pattern_index) {
            None => value_index == value.len(),
            Some('*') => {
                visit(
                    pattern,
                    value,
                    pattern_index.saturating_add(1),
                    value_index,
                    memo,
                ) || value_index < value.len()
                    && visit(
                        pattern,
                        value,
                        pattern_index,
                        value_index.saturating_add(1),
                        memo,
                    )
            }
            Some('?') => {
                value_index < value.len()
                    && visit(
                        pattern,
                        value,
                        pattern_index.saturating_add(1),
                        value_index.saturating_add(1),
                        memo,
                    )
            }
            Some('[') => {
                if let Some((end, matches)) =
                    glob_character_class(pattern, pattern_index, value.get(value_index).copied())
                {
                    matches
                        && visit(
                            pattern,
                            value,
                            end.saturating_add(1),
                            value_index.saturating_add(1),
                            memo,
                        )
                } else {
                    value.get(value_index) == Some(&'[')
                        && visit(
                            pattern,
                            value,
                            pattern_index.saturating_add(1),
                            value_index.saturating_add(1),
                            memo,
                        )
                }
            }
            Some(character) => {
                value.get(value_index) == Some(character)
                    && visit(
                        pattern,
                        value,
                        pattern_index.saturating_add(1),
                        value_index.saturating_add(1),
                        memo,
                    )
            }
        };
        memo.insert((pattern_index, value_index), result);
        result
    }

    visit(
        &pattern.chars().collect::<Vec<_>>(),
        &value.chars().collect::<Vec<_>>(),
        0,
        0,
        &mut BTreeMap::new(),
    )
}

fn glob_character_class(
    pattern: &[char],
    open: usize,
    value: Option<char>,
) -> Option<(usize, bool)> {
    let mut cursor = open.saturating_add(1);
    let negated = pattern.get(cursor) == Some(&'!');
    if negated {
        cursor = cursor.saturating_add(1);
    }
    let content_start = cursor;
    if pattern.get(cursor) == Some(&']') {
        cursor = cursor.saturating_add(1);
    }
    let close = (cursor..pattern.len()).find(|index| pattern[*index] == ']')?;
    if close == content_start {
        return None;
    }
    let value = value?;
    let mut matched = false;
    let mut index = content_start;
    while index < close {
        if index.saturating_add(2) < close && pattern[index.saturating_add(1)] == '-' {
            matched |= pattern[index] <= value && value <= pattern[index.saturating_add(2)];
            index = index.saturating_add(3);
        } else {
            matched |= pattern[index] == value;
            index = index.saturating_add(1);
        }
    }
    Some((close, matched != negated))
}

fn mention_candidate(path: String, is_directory: bool) -> CompletionCandidate {
    let suffix = if is_directory { "/" } else { "" };
    let insertion = format!("@{path}{suffix}");
    CompletionCandidate {
        id: format!("mention:{insertion}"),
        kind: CompletionKind::Mention,
        label: insertion.clone(),
        insertion,
        description: String::new(),
    }
}

#[cfg(test)]
mod autocompletion_parity_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_are_bounded_after_the_reference_index_window() {
        let mut entries = (0..MAX_INDEXED_ENTRIES)
            .map(|index| {
                let rel = format!("a{index:05}.txt");
                IndexedPath {
                    rel_lower: rel.clone(),
                    name: rel.clone(),
                    rel,
                    is_directory: false,
                }
            })
            .collect::<Vec<_>>();
        entries.push(IndexedPath {
            rel: "zzzz-needle.txt".to_owned(),
            rel_lower: "zzzz-needle.txt".to_owned(),
            name: "zzzz-needle.txt".to_owned(),
            is_directory: false,
        });
        assert!(rank_indexed_paths(&entries, "needle").is_empty());
    }

    #[test]
    fn workspace_index_is_reused_across_queries() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
        let mut index = WorkspaceIndex::default();

        assert_eq!(index.candidates(temporary.path(), "a").unwrap().len(), 1);
        assert_eq!(index.candidates(temporary.path(), "al").unwrap().len(), 1);
        assert_eq!(index.rebuilds(), 1);
    }

    #[test]
    fn default_and_gitignore_rules_match_the_reference_precedence() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(
            temporary.path().join(".gitignore"),
            "ignored/\n*.log\n!keep.log\nreport-[0-9].txt\n[!a]lpha.tmp\n",
        )
        .expect("ignore fixture");
        let rules = IgnoreRules::load(temporary.path());
        assert!(rules.should_ignore("target", "target", true));
        assert!(rules.should_ignore("ignored", "ignored", true));
        assert!(rules.should_ignore("build.log", "build.log", false));
        assert!(!rules.should_ignore("keep.log", "keep.log", false));
        assert!(rules.should_ignore("report-7.txt", "report-7.txt", false));
        assert!(!rules.should_ignore("report-x.txt", "report-x.txt", false));
        assert!(rules.should_ignore("blpha.tmp", "blpha.tmp", false));
        assert!(!rules.should_ignore("alpha.tmp", "alpha.tmp", false));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_are_listed_without_being_followed() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary workspace");
        symlink(temporary.path(), temporary.path().join("loop")).expect("cycle fixture");
        let candidates = path_candidates(temporary.path(), "loop").expect("cycle-safe scan");
        assert_eq!(
            candidates.first().map(|candidate| candidate.label.as_str()),
            Some("@loop")
        );
    }
}
