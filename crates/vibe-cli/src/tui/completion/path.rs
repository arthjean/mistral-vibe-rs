use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::fuzzy::fuzzy_match_score;
use super::{CompletionCandidate, CompletionKind};
use crate::tui::input::InputError;
use vibe_core::matching::pattern_matches;

mod watch;

use watch::{ChangeKind, WatchController};

const MAX_INDEXED_ENTRIES: usize = 32_000;
const MAX_PATH_MATCHES: usize = 100;
/// Past this many changes in one delivered batch, the reference replaces the
/// incremental path with one full rebuild.
const MASS_CHANGE_THRESHOLD: usize = 200;
/// The codepoint the reference's entry mask stops at, so the mask is one bit
/// per ASCII codepoint and a wider codepoint contributes none.
const ASCII_CODEPOINT_LIMIT: u32 = 128;

/// What the operator is told when the platform cannot watch the workspace.
/// Completion keeps answering from the last built index, so this is a notice
/// and never a failure.
const WATCH_UNAVAILABLE: &str = "Filesystem watching is unavailable; completion may not reflect \
                                 new files";

#[derive(Debug)]
struct IndexedPath {
    rel: String,
    rel_lower: String,
    name: String,
    is_directory: bool,
    /// One bit per ASCII codepoint present in `rel_lower`, which lets a query
    /// whose own mask carries a bit this entry lacks skip the matcher.
    ascii_mask: u128,
}

/// The two counters the reference's `FileIndexStats` carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct IndexStats {
    pub(super) rebuilds: u64,
    pub(super) incremental_updates: u64,
}

/// One bit per ASCII codepoint in `value`, matching `build_ascii_mask`.
fn build_ascii_mask(value: &str) -> u128 {
    value
        .chars()
        .map(u32::from)
        .filter(|codepoint| *codepoint < ASCII_CODEPOINT_LIMIT)
        .fold(0_u128, |mask, codepoint| mask | (1_u128 << codepoint))
}

/// The mask a query must have every bit of, or `None` when the query carries a
/// codepoint the mask cannot represent and no filtering may apply.
fn query_ascii_mask(pattern: &str) -> Option<u128> {
    if pattern
        .chars()
        .any(|character| u32::from(character) >= ASCII_CODEPOINT_LIMIT)
    {
        return None;
    }
    Some(build_ascii_mask(&pattern.to_lowercase()))
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
    search_pattern_ascii_mask: Option<u128>,
}

/// The workspace file index: the reference's `FileIndexer`, its `FileIndexStore`
/// and their shared ignore rules, held together because nothing here needs the
/// upstream split across three objects.
///
/// The reference's background rebuild executor, its per-root cancellation tasks
/// and its `_target_root` bookkeeping are not reproduced: this port already
/// rebuilds off the terminal event path, on the completion worker, so the
/// executor has no observable consequence for any caller. `docs/parity.md`
/// records that as an accepted divergence.
#[derive(Debug, Default)]
pub(super) struct WorkspaceIndex {
    root: Option<PathBuf>,
    rules: IgnoreRules,
    entries: BTreeMap<String, IndexedPath>,
    stats: IndexStats,
    watcher: WatchController,
    /// Read on every query, the way the reference reads its
    /// `should_enable_watcher` getter inside `get_index`.
    watch_gate: Arc<AtomicBool>,
    /// A watcher failure waiting to be surfaced, taken once per session.
    notice: Option<String>,
    watch_notice_shown: bool,
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
            // The previous root's watcher stops before the new root is built,
            // so no batch can arrive against an index it does not describe.
            self.watcher.stop();
            self.rebuild(&root);
        }
        self.sync_watcher(&root);
        self.drain_changes();
        Ok(rank_indexed_paths(self.entries.values(), raw_query, true))
    }

    /// Walks `root` from scratch, recompiling the ignore rules for it first.
    fn rebuild(&mut self, root: &Path) {
        self.rules.ensure_for_root(root);
        let mut entries = BTreeMap::new();
        walk_workspace(root, "", &self.rules, &mut entries);
        self.entries = entries;
        self.root = Some(root.to_path_buf());
        self.stats.rebuilds = self.stats.rebuilds.saturating_add(1);
    }

    /// Drops the built index and its compiled rules, so the next query rebuilds
    /// both. The counters survive, as they do upstream.
    ///
    /// The reference reaches the same code from `shutdown`, registered with
    /// `atexit`, which is why this port runs it from [`Drop`]: the watcher is
    /// stopped before the store is cleared rather than in field order.
    fn reset(&mut self) {
        self.watcher.stop();
        self.entries.clear();
        self.root = None;
        self.rules.reset();
    }

    /// Starts or stops the watcher for `root` under the configured key.
    fn sync_watcher(&mut self, root: &Path) {
        if !self.watch_gate.load(Ordering::Relaxed) {
            self.watcher.stop();
            return;
        }
        if let Err(reason) = self.watcher.start(root)
            && !self.watch_notice_shown
        {
            self.watch_notice_shown = true;
            self.notice = Some(format!("{WATCH_UNAVAILABLE} ({reason})"));
        }
    }

    /// Applies every batch the watcher has delivered since the last query.
    fn drain_changes(&mut self) {
        while let Some((root, changes)) = self.watcher.next_batch() {
            // A batch left over from a watcher this index has replaced names a
            // root the store no longer holds, which is the stale-root guard.
            if self.root.as_deref() != Some(root.as_path()) {
                continue;
            }
            self.apply_changes(&changes);
        }
    }

    /// Applies one delivered batch in place, or rebuilds when the batch is
    /// larger than the reference's threshold.
    fn apply_changes(&mut self, changes: &[(ChangeKind, PathBuf)]) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if changes.len() > MASS_CHANGE_THRESHOLD {
            self.rebuild(&root);
            return;
        }
        let mut modified = false;
        for (kind, path) in changes {
            let Some(rel) = relative_key(&root, path) else {
                continue;
            };
            if *kind == ChangeKind::Deleted {
                modified |= self.remove_entry(&rel);
                continue;
            }
            // Upstream reads existence and directory-ness through the resolved
            // path, so a broken link is skipped and a link to a directory is
            // walked, even though the walk itself never follows one.
            let Ok(metadata) = fs::metadata(path) else {
                continue;
            };
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !metadata.is_dir() {
                if let Some(entry) = self.create_entry(rel.clone(), name.to_owned(), false) {
                    self.entries.insert(rel, entry);
                    modified = true;
                }
                continue;
            }
            if let Some(entry) = self.create_entry(rel.clone(), name.to_owned(), true) {
                self.entries.insert(rel.clone(), entry);
                modified = true;
            }
            let mut descendants = BTreeMap::new();
            walk_workspace(path, &rel, &self.rules, &mut descendants);
            for (key, entry) in descendants {
                self.entries.insert(key, entry);
                modified = true;
            }
        }
        if modified {
            self.stats.incremental_updates = self.stats.incremental_updates.saturating_add(1);
        }
    }

    /// Removes one entry and, for a directory, everything beneath it.
    fn remove_entry(&mut self, rel: &str) -> bool {
        let Some(entry) = self.entries.remove(rel) else {
            return false;
        };
        if entry.is_directory {
            let prefix = format!("{rel}/");
            self.entries.retain(|key, _| !key.starts_with(&prefix));
        }
        true
    }

    fn create_entry(&self, rel: String, name: String, is_directory: bool) -> Option<IndexedPath> {
        if self.rules.should_ignore(&rel, &name, is_directory) {
            return None;
        }
        Some(indexed_path(rel, name, is_directory))
    }

    /// The two counters the corpus compares. Nothing in the running client
    /// reads them; they exist so rebuild behavior is measurable.
    #[cfg(test)]
    pub(super) fn stats(&self) -> IndexStats {
        self.stats
    }

    #[cfg(test)]
    fn is_watching(&self) -> bool {
        self.watcher.is_watching()
    }
}

impl Drop for WorkspaceIndex {
    fn drop(&mut self) {
        self.reset();
    }
}

/// The index every completion query is answered from, shared between the
/// completion worker and the synchronous adapter so one workspace root is
/// walked once per process rather than once per keystroke.
#[derive(Debug, Clone)]
pub(crate) struct PathIndex {
    inner: Arc<Mutex<WorkspaceIndex>>,
    /// Held beside the lock rather than inside it, so the render loop can
    /// publish a preference change while the worker is walking a tree.
    watch_enabled: Arc<AtomicBool>,
}

impl Default for PathIndex {
    fn default() -> Self {
        let watch_enabled = Arc::new(AtomicBool::new(false));
        let mut index = WorkspaceIndex::default();
        index.watch_gate = Arc::clone(&watch_enabled);
        Self {
            inner: Arc::new(Mutex::new(index)),
            watch_enabled,
        }
    }
}

impl PathIndex {
    fn locked(&self) -> Result<MutexGuard<'_, WorkspaceIndex>, InputError> {
        self.inner.lock().map_err(|_| {
            InputError::CompletionWorker("completion index lock is poisoned".to_owned())
        })
    }

    pub(crate) fn candidates(
        &self,
        workspace: &Path,
        raw_query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        self.locked()?.candidates(workspace, raw_query)
    }

    /// Publishes `file_watcher_for_autocomplete`. The index reads it on every
    /// query, so a preference change takes effect on the next one without
    /// rebuilding anything.
    pub(crate) fn set_watch_enabled(&self, enabled: bool) {
        self.watch_enabled.store(enabled, Ordering::Relaxed);
    }

    /// How many times this index has walked a tree, which is what proves the
    /// two code paths that answer a mention query share one walk.
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> u64 {
        self.inner
            .lock()
            .map(|index| index.stats().rebuilds)
            .unwrap_or_default()
    }

    /// Takes the watcher diagnostic, which is raised at most once per session.
    /// A query in flight keeps the notice until the next poll rather than
    /// blocking the caller on the walk.
    pub(crate) fn take_notice(&self) -> Option<String> {
        self.inner
            .try_lock()
            .ok()
            .and_then(|mut index| index.notice.take())
    }
}

fn rank_indexed_paths<'a>(
    entries: impl IntoIterator<Item = &'a IndexedPath>,
    raw_query: &str,
    mask_filter: bool,
) -> Vec<CompletionCandidate> {
    let context = path_search_context(raw_query);
    let mut matches = Vec::<(CompletionCandidate, PathMatchRank)>::new();
    for entry in entries.into_iter().take(MAX_INDEXED_ENTRIES) {
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
        if mask_filter && !can_possibly_fuzzy_match(entry, &context) {
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
fn can_possibly_fuzzy_match(entry: &IndexedPath, context: &PathSearchContext<'_>) -> bool {
    context
        .search_pattern_ascii_mask
        .is_none_or(|mask| entry.ascii_mask & mask == mask)
}

/// The index key for `path` under `root`: its relative path in POSIX spelling,
/// or `None` when it names the root itself or escapes it, which is the pair of
/// cases the reference skips.
fn relative_key(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut segments = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

fn indexed_path(rel: String, name: String, is_directory: bool) -> IndexedPath {
    let rel_lower = rel.to_lowercase();
    IndexedPath {
        ascii_mask: build_ascii_mask(&rel_lower),
        rel_lower,
        rel,
        name,
        is_directory,
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

fn walk_workspace(
    directory: &Path,
    prefix: &str,
    rules: &IgnoreRules,
    output: &mut BTreeMap<String, IndexedPath>,
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
        output.insert(rel.clone(), indexed_path(rel.clone(), name, is_directory));
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

#[derive(Default)]
struct IgnoreRules {
    rules: Vec<IgnoreRule>,
    root: Option<PathBuf>,
}

impl std::fmt::Debug for IgnoreRules {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IgnoreRules")
            .field("root", &self.root)
            .field("rules", &self.rules.len())
            .finish()
    }
}

impl IgnoreRules {
    /// Compiles the defaults and the root's `.gitignore` once per root, the way
    /// the reference's `ensure_for_root` does.
    fn ensure_for_root(&mut self, root: &Path) {
        if self.root.as_deref() == Some(root) {
            return;
        }
        *self = Self {
            rules: Self::load(root).rules,
            root: Some(root.to_path_buf()),
        };
    }

    fn reset(&mut self) {
        self.rules.clear();
        self.root = None;
    }

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
        Self {
            rules,
            root: Some(root.to_path_buf()),
        }
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
        pattern_matches(&self.pattern, target)
    }
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

    use std::time::{Duration, Instant};

    /// Waits for `probe` to hold, up to the second the watcher's own bounds
    /// allow: a 200 ms step plus the time the platform takes to report.
    fn eventually(mut probe: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if probe() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        probe()
    }

    fn watching_index() -> PathIndex {
        let index = PathIndex::default();
        index.set_watch_enabled(true);
        index
    }

    #[test]
    fn results_are_bounded_after_the_reference_index_window() {
        let mut entries = (0..MAX_INDEXED_ENTRIES)
            .map(|index| {
                let rel = format!("a{index:05}.txt");
                indexed_path(rel.clone(), rel, false)
            })
            .collect::<Vec<_>>();
        entries.push(indexed_path(
            "zzzz-needle.txt".to_owned(),
            "zzzz-needle.txt".to_owned(),
            false,
        ));
        assert!(rank_indexed_paths(entries.iter(), "needle", true).is_empty());
    }

    #[test]
    fn workspace_index_is_reused_across_queries() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
        let mut index = WorkspaceIndex::default();

        assert_eq!(index.candidates(temporary.path(), "a").unwrap().len(), 1);
        assert_eq!(index.candidates(temporary.path(), "al").unwrap().len(), 1);
        assert_eq!(index.stats().rebuilds, 1);
        assert_eq!(index.stats().incremental_updates, 0);
    }

    /// The first query blocks on the first walk rather than answering from an
    /// index that has not been built, which is the reference's
    /// `_wait_for_rebuild`.
    #[test]
    fn the_first_query_waits_for_the_first_build() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
        let mut index = WorkspaceIndex::default();

        let candidates = index
            .candidates(temporary.path(), "alpha")
            .expect("the first query is answered");

        assert_eq!(index.stats().rebuilds, 1);
        assert_eq!(
            candidates.first().map(|candidate| candidate.label.as_str()),
            Some("@alpha.txt")
        );
    }

    /// A reset drops the built index and the rules compiled for its root, so
    /// the next query rebuilds both. The counters survive, as they do upstream.
    #[test]
    fn a_reset_rebuilds_and_recompiles_the_rules_for_the_root() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
        let mut index = WorkspaceIndex::default();
        index
            .candidates(temporary.path(), "")
            .expect("the first query is answered");
        assert!(index.rules.root.is_some());

        index.reset();
        assert!(index.entries.is_empty(), "the reset index holds nothing");
        assert!(index.rules.root.is_none(), "the rules are uncompiled");

        // A file the reset index has never seen appears, so the answer can only
        // come from a fresh walk under freshly compiled rules.
        fs::write(temporary.path().join("beta.log"), "fixture").expect("path fixture");
        fs::write(temporary.path().join("gamma.txt"), "fixture").expect("path fixture");
        let candidates = index
            .candidates(temporary.path(), "")
            .expect("the query after a reset is answered");

        assert_eq!(index.stats().rebuilds, 2, "the counters survive a reset");
        let labels = candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"@gamma.txt"), "{labels:?}");
        assert!(
            !labels.contains(&"@beta.log"),
            "the recompiled rules still ignore `*.log`: {labels:?}"
        );
    }

    /// US-202: the key is the only thing that decides whether a watcher runs.
    #[test]
    fn the_configured_key_gates_the_watcher() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let index = PathIndex::default();

        index
            .candidates(temporary.path(), "")
            .expect("a query with the key off is answered");
        assert!(
            !index.inner.lock().is_ok_and(|index| index.is_watching()),
            "no watcher exists while the key is off"
        );

        index.set_watch_enabled(true);
        index
            .candidates(temporary.path(), "")
            .expect("a query with the key on is answered");
        assert!(
            index.inner.lock().is_ok_and(|index| index.is_watching()),
            "the key turns a watcher on"
        );

        index.set_watch_enabled(false);
        index
            .candidates(temporary.path(), "")
            .expect("a query after the key went off is answered");
        assert!(
            !index.inner.lock().is_ok_and(|index| index.is_watching()),
            "the watcher is stopped before the query is answered"
        );
    }

    /// US-202: a file written during a session reaches the next query without
    /// a rebuild, which is what the whole watcher exists for.
    #[test]
    fn a_file_written_during_a_session_appears_in_the_next_query() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
        let index = watching_index();
        let candidates = index
            .candidates(temporary.path(), "")
            .expect("the first query is answered");
        assert_eq!(candidates.len(), 1);

        fs::write(temporary.path().join("beta.txt"), "fixture").expect("written during a session");

        assert!(
            eventually(|| index
                .candidates(temporary.path(), "beta")
                .is_ok_and(|candidates| candidates
                    .iter()
                    .any(|candidate| candidate.label == "@beta.txt"))),
            "the written file reaches the index"
        );
        let stats = index
            .inner
            .lock()
            .map(|index| index.stats())
            .expect("the index is readable");
        assert_eq!(stats.rebuilds, 1, "no rebuild answered the change");
        assert!(stats.incremental_updates >= 1, "the change was applied");
    }

    /// US-202: a root change stops the previous root's watcher before the new
    /// one starts, so no batch can reach an index that does not describe it.
    #[test]
    fn a_root_change_moves_the_watcher() {
        let first = tempfile::tempdir().expect("first workspace");
        let second = tempfile::tempdir().expect("second workspace");
        fs::write(second.path().join("only-here.txt"), "fixture").expect("path fixture");
        let index = watching_index();

        index
            .candidates(first.path(), "")
            .expect("the first root is answered");
        let candidates = index
            .candidates(second.path(), "")
            .expect("the second root is answered");

        assert_eq!(
            candidates.first().map(|candidate| candidate.label.as_str()),
            Some("@only-here.txt")
        );
        let (root, rebuilds) = index
            .inner
            .lock()
            .map(|index| (index.root.clone(), index.stats().rebuilds))
            .expect("the index is readable");
        assert_eq!(rebuilds, 2, "each root was walked once");
        assert_eq!(
            root,
            Some(fs::canonicalize(second.path()).expect("resolved root"))
        );
    }

    /// US-202: an unwatchable root is a notice, never a failed query.
    #[test]
    fn an_unwatchable_root_reports_once_and_keeps_answering() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
        let index = watching_index();
        index
            .candidates(temporary.path(), "")
            .expect("the first query is answered");

        // Break the watcher the way an exhausted backend does: the controller
        // is asked to watch a root that is not there.
        if let Ok(mut held) = index.inner.lock() {
            held.watcher.stop();
            held.watch_notice_shown = false;
            let absent = temporary.path().join("absent");
            held.sync_watcher(&absent);
        }

        assert!(
            index
                .take_notice()
                .is_some_and(|notice| notice.starts_with(WATCH_UNAVAILABLE)),
            "the failure is reported"
        );
        assert!(
            index.take_notice().is_none(),
            "and reported once per session"
        );
        assert_eq!(
            index
                .candidates(temporary.path(), "alpha")
                .expect("completion keeps answering")
                .len(),
            1
        );
    }

    /// US-202: a running watcher never outlives the index it feeds, and the
    /// drop path is bounded by the same join the reference's `shutdown` is.
    #[test]
    fn dropping_the_index_stops_the_watcher_within_the_join_bound() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let mut index = WorkspaceIndex::default();
        index.watch_gate.store(true, Ordering::Relaxed);
        index
            .candidates(temporary.path(), "")
            .expect("the first query is answered");
        assert!(index.is_watching(), "the query started a watcher");

        let started = Instant::now();
        drop(index);

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the watcher is joined inside the reference's one second bound"
        );
    }

    /// US-203: the store's own contract, driven through the categories the
    /// watcher delivers.
    #[test]
    fn changes_are_applied_entry_by_entry() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let root = temporary.path();
        fs::create_dir_all(root.join("src/render")).expect("fixture tree");
        fs::write(root.join("src/render/mod.rs"), "fixture").expect("fixture file");
        fs::write(root.join("src/main.rs"), "fixture").expect("fixture file");
        let mut index = WorkspaceIndex::default();
        index
            .candidates(root, "")
            .expect("the fixture root is walkable");
        let root = fs::canonicalize(root).expect("resolved root");

        // A created file is added; a created directory brings its descendants.
        fs::write(root.join("src/added.rs"), "fixture").expect("added file");
        fs::create_dir_all(root.join("pkg/deep")).expect("added directory");
        fs::write(root.join("pkg/deep/leaf.rs"), "fixture").expect("added descendant");
        fs::write(root.join("pkg/ignored.log"), "fixture").expect("ignored descendant");
        index.apply_changes(&[
            (ChangeKind::Added, root.join("src/added.rs")),
            (ChangeKind::Added, root.join("pkg")),
        ]);
        let keys = index.entries.keys().cloned().collect::<Vec<_>>();
        assert!(keys.contains(&"src/added.rs".to_owned()), "{keys:?}");
        assert!(keys.contains(&"pkg/deep/leaf.rs".to_owned()), "{keys:?}");
        assert!(
            !keys.contains(&"pkg/ignored.log".to_owned()),
            "an ignored descendant is never created: {keys:?}"
        );

        // A deleted directory takes everything beneath it by prefix.
        fs::remove_dir_all(root.join("src/render")).expect("removed directory");
        index.apply_changes(&[(ChangeKind::Deleted, root.join("src/render"))]);
        let keys = index.entries.keys().cloned().collect::<Vec<_>>();
        assert!(!keys.contains(&"src/render".to_owned()), "{keys:?}");
        assert!(!keys.contains(&"src/render/mod.rs".to_owned()), "{keys:?}");
        assert!(keys.contains(&"src/main.rs".to_owned()), "{keys:?}");

        // A path outside the root, a path that was never written and a
        // deletion the index never held are all skipped without an update.
        let counted = index.stats().incremental_updates;
        index.apply_changes(&[
            (ChangeKind::Added, root.join("../stranger.txt")),
            (ChangeKind::Added, root.join("src/never-written.rs")),
            (ChangeKind::Deleted, root.join("src/never-written.rs")),
            (ChangeKind::Modified, root.clone()),
        ]);
        assert_eq!(
            index.stats().incremental_updates,
            counted,
            "a batch that changes nothing counts no update"
        );
    }

    /// US-203: past the threshold, one rebuild replaces the batch.
    #[test]
    fn a_batch_past_the_threshold_rebuilds_instead() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let root = temporary.path();
        fs::create_dir(root.join("bulk")).expect("fixture directory");
        let mut index = WorkspaceIndex::default();
        index
            .candidates(root, "")
            .expect("the fixture root is walkable");
        let root = fs::canonicalize(root).expect("resolved root");

        let mut changes = Vec::new();
        for entry in 0..=MASS_CHANGE_THRESHOLD {
            let path = root.join(format!("bulk/file-{entry:03}.txt"));
            fs::write(&path, "fixture").expect("bulk file");
            changes.push((ChangeKind::Added, path));
        }

        index.apply_changes(&changes[..MASS_CHANGE_THRESHOLD]);
        assert_eq!(
            index.stats().rebuilds,
            1,
            "a batch at the threshold applies"
        );
        assert_eq!(index.stats().incremental_updates, 1);

        index.apply_changes(&changes);
        assert_eq!(index.stats().rebuilds, 2, "a batch past it rebuilds");
        assert_eq!(
            index.stats().incremental_updates,
            1,
            "and counts no incremental update"
        );
        assert_eq!(
            index.entries.len(),
            MASS_CHANGE_THRESHOLD + 2,
            "the rebuilt index holds the directory and every file"
        );
    }

    /// US-205: every indexed entry carries the mask of its lowercased relative
    /// path, and the prefilter rejects exactly the entries missing a bit the
    /// query needs, before the matcher is asked anything.
    #[test]
    fn an_indexed_entry_carries_its_mask_and_the_prefilter_reads_it() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir(temporary.path().join("Render")).expect("fixture directory");
        fs::write(temporary.path().join("Render/Table.rs"), "fixture").expect("path fixture");
        let mut index = WorkspaceIndex::default();
        index
            .candidates(temporary.path(), "")
            .expect("the fixture root is walkable");

        let entry = index
            .entries
            .get("Render/Table.rs")
            .expect("the walked entry is held");
        assert_eq!(entry.rel_lower, "render/table.rs");
        assert_eq!(entry.ascii_mask, build_ascii_mask("render/table.rs"));

        let context = path_search_context("table");
        assert!(can_possibly_fuzzy_match(entry, &context));
        let context = path_search_context("tablez");
        assert!(
            !can_possibly_fuzzy_match(entry, &context),
            "a bit the entry lacks rejects it before the matcher"
        );
    }

    /// US-205: a query the mask cannot represent filters nothing.
    #[test]
    fn a_non_ascii_query_disables_the_mask_filter() {
        let entries = [
            indexed_path("café.txt".to_owned(), "café.txt".to_owned(), false),
            indexed_path("cafe.txt".to_owned(), "cafe.txt".to_owned(), false),
        ];

        assert!(query_ascii_mask("café").is_none());
        assert!(query_ascii_mask("cafe").is_some());
        let accented = rank_indexed_paths(entries.iter(), "café", true);
        assert_eq!(
            accented.first().map(|candidate| candidate.label.as_str()),
            Some("@café.txt"),
            "the accented entry is reachable"
        );
        assert_eq!(
            rank_indexed_paths(entries.iter(), "café", true),
            rank_indexed_paths(entries.iter(), "café", false)
        );
    }

    #[test]
    fn default_and_gitignore_rules_match_the_reference_precedence() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(
            temporary.path().join(".gitignore"),
            "ignored/\n*.log\n!keep.log\nreport-[0-9].txt\n[!a]lpha.tmp\n",
        )
        .expect("ignore fixture");
        let mut rules = IgnoreRules::default();
        rules.ensure_for_root(temporary.path());
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
        let candidates = PathIndex::default()
            .candidates(temporary.path(), "loop")
            .expect("cycle-safe scan");
        assert_eq!(
            candidates.first().map(|candidate| candidate.label.as_str()),
            Some("@loop")
        );
    }
}
