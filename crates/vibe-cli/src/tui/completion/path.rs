use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use vibe_core::sync::lock;

use super::{CompletionCandidate, CompletionKind};
use crate::tui::input::InputError;

mod git;
mod ignore;
mod listing;
mod rank;
mod watch;

use git::list_git_entries;
use ignore::{IgnoreRules, walk_workspace};
use listing::{collect_filesystem_matches, list_current_directory};
#[cfg(test)]
use rank::rank_indexed_paths;
use rank::{
    PathSearchContext, build_ascii_mask, path_search_context, query_ascii_mask, rank_with_context,
};
use watch::{ChangeKind, WatchController};

/// Reference `DEFAULT_TARGET_MATCHES`: how many candidates one answer keeps.
/// Every indexed entry is ranked, as the reference's uncapped
/// `max_entries_to_process` does since v2.25.3.
const MAX_PATH_MATCHES: usize = 100;

/// Past this many changes in one delivered batch, the reference replaces the
/// incremental path with one full rebuild.
const MASS_CHANGE_THRESHOLD: usize = 200;

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

/// The workspace file index: the reference's `FileIndexer`, its `FileIndexStore`
/// and their shared ignore rules, held together because nothing here needs the
/// upstream split across three objects.
///
/// Inside a git work tree the index is what `git ls-files` lists, as reference
/// `FileIndexStore._list_git_entries` does since v2.25.3; elsewhere it is the
/// ignore-rule walk. A watched change only marks a git-backed index dirty, and
/// the next query lists the tree again.
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
    /// Whether the entries came from `git ls-files` rather than the walk.
    git_backed: bool,
    /// Set by a watched change to a git-backed index: the next query rebuilds.
    dirty: bool,
    watcher: WatchController,
    /// Read on every query, the way the reference reads its
    /// `should_enable_watcher` getter inside `get_index`.
    watch_gate: Arc<AtomicBool>,
    /// A watcher failure waiting to be surfaced, taken once per session.
    notice: Option<String>,
    watch_notice_shown: bool,
}

impl WorkspaceIndex {
    /// Reference `PathCompleter._collect_matches` for the text after `@`: a
    /// bare `@` lists the working directory, a `..` path lists the directory
    /// it names outside the root, and anything else is ranked against the
    /// index, with a trailing slash that names no indexed directory retried
    /// as a fuzzy pattern.
    pub(super) fn collect(
        &mut self,
        workspace: &Path,
        raw_query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        let partial = raw_query.replace('\\', "/");
        if partial.is_empty() {
            return Ok(list_current_directory(workspace));
        }
        let root = vibe_core::platform::canonicalize(workspace)
            .map_err(|error| InputError::Workspace(error.to_string()))?;
        if let Some(matches) = collect_filesystem_matches(&root, &partial) {
            return Ok(matches);
        }
        self.ensure(&root);
        let context = path_search_context(&partial);
        let mut matches = rank_with_context(self.entries.values(), &context, true);
        if matches.is_empty() && partial.ends_with('/') {
            let prefix = partial.trim_end_matches('/');
            let prefix_is_real_directory = self
                .entries
                .values()
                .any(|entry| entry.is_directory && entry.rel == prefix);
            if !prefix.is_empty() && !prefix_is_real_directory {
                let fallback = PathSearchContext {
                    suffix: "",
                    search_pattern: &partial,
                    path_prefix: "",
                    immediate_only: false,
                    search_pattern_ascii_mask: query_ascii_mask(&partial),
                };
                matches = rank_with_context(self.entries.values(), &fallback, true);
            }
        }
        Ok(matches)
    }

    /// Reference `FileIndexer.get_index`: a new root or a dirty index is built
    /// again, and the watcher follows the configured key. Delivered batches are
    /// applied first, which is when the reference's watch thread would already
    /// have applied them.
    fn ensure(&mut self, root: &Path) {
        if self.root.as_deref() != Some(root) {
            // The previous root's watcher stops before the new root is built,
            // so no batch can arrive against an index it does not describe.
            self.watcher.stop();
        }
        self.drain_changes();
        if self.root.as_deref() != Some(root) || self.dirty {
            self.rebuild(root);
        }
        self.sync_watcher(root);
    }

    /// Lists `root` from scratch: through `git ls-files` inside a work tree,
    /// through the ignore-rule walk everywhere else.
    fn rebuild(&mut self, root: &Path) {
        if let Some(entries) = list_git_entries(root) {
            self.entries = entries;
            self.git_backed = true;
        } else {
            self.rules.ensure_for_root(root);
            let mut entries = BTreeMap::new();
            walk_workspace(root, "", &self.rules, &mut entries);
            self.entries = entries;
            self.git_backed = false;
        }
        self.root = Some(root.to_path_buf());
        self.dirty = false;
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
        self.git_backed = false;
        self.dirty = false;
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
            self.handle_batch(&root, &changes);
        }
    }

    /// Reference `FileIndexer._handle_watch_changes`: an empty batch is
    /// dropped, and a batch left over from a watcher this index has replaced
    /// names a root the store no longer holds, which is the stale-root guard.
    fn handle_batch(&mut self, root: &Path, changes: &[(ChangeKind, PathBuf)]) {
        if changes.is_empty() || self.root.as_deref() != Some(root) {
            return;
        }
        self.apply_changes(changes);
    }

    /// Applies one delivered batch: a git-backed index is only marked dirty,
    /// a walked one is updated in place, or rebuilt when the batch is larger
    /// than the reference's threshold.
    fn apply_changes(&mut self, changes: &[(ChangeKind, PathBuf)]) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.git_backed {
            self.dirty = true;
            return;
        }
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

    /// Builds or refreshes the index for `workspace` and ranks `raw_query`
    /// against it without the `_collect_matches` routing, which is what the
    /// index tests below exercise.
    #[cfg(test)]
    fn candidates(
        &mut self,
        workspace: &Path,
        raw_query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        let root = vibe_core::platform::canonicalize(workspace)
            .map_err(|error| InputError::Workspace(error.to_string()))?;
        self.ensure(&root);
        Ok(rank_indexed_paths(self.entries.values(), raw_query, true))
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
    /// What the composer's path completer answers for the text after `@`,
    /// reference `PathCompleter._collect_matches`.
    pub(crate) fn completer_candidates(
        &self,
        workspace: &Path,
        raw_query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        lock(&self.inner).collect(workspace, raw_query)
    }

    /// Publishes `file_watcher_for_autocomplete`. The index reads it on every
    /// query, so a preference change takes effect on the next one without
    /// rebuilding anything.
    pub(crate) fn set_watch_enabled(&self, enabled: bool) {
        self.watch_enabled.store(enabled, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn candidates(
        &self,
        workspace: &Path,
        raw_query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        lock(&self.inner).candidates(workspace, raw_query)
    }

    /// How many times this index has walked a tree, which is what proves the
    /// two code paths that answer a mention query share one walk.
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> u64 {
        lock(&self.inner).stats().rebuilds
    }

    /// Takes the watcher diagnostic, which is raised at most once per session.
    /// A query in flight keeps the notice until the next poll rather than
    /// blocking the caller on the walk.
    pub(crate) fn take_notice(&self) -> Option<String> {
        let mut index = match self.inner.try_lock() {
            Ok(index) => index,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        index.notice.take()
    }
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
#[path = "path/path_tests.rs"]
mod path_tests;
