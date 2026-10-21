//! The two answers read off the disk rather than the index: a bare `@` lists
//! the working directory, and a partial that starts with `..` lists the
//! directory it names outside the root.

use std::fs;
use std::path::Path;

use super::super::CompletionCandidate;
use super::{MAX_PATH_MATCHES, mention_candidate};

/// Reference `PathCompleter._list_current_directory`
/// (`vibe/cli/autocompletion/completers.py`): a bare `@` lists the working
/// directory as it is on disk, without the index or its ignore rules. Hidden
/// entries are skipped, names sort case-insensitively (ties keep the directory
/// order), directories carry a trailing slash, and the list stops at the
/// completer's target count.
pub(super) fn list_current_directory(workspace: &Path) -> Vec<CompletionCandidate> {
    let Ok(entries) = fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect::<Vec<_>>();
    names.sort_by_cached_key(|name| name.to_lowercase());
    names
        .into_iter()
        .take(MAX_PATH_MATCHES)
        .map(|name| {
            let is_directory = workspace.join(&name).is_dir();
            mention_candidate(name, is_directory)
        })
        .collect()
}

/// Reference `PathCompleter._collect_filesystem_matches`: a partial whose first
/// segment is `..` and whose directory resolves outside `root` lists that
/// directory's children, or [`None`] when the query belongs to the index.
pub(super) fn collect_filesystem_matches(
    root: &Path,
    partial: &str,
) -> Option<Vec<CompletionCandidate>> {
    let (dir_portion, suffix) = split_outside_root_dir(partial)?;
    if dir_portion.is_empty() {
        return None;
    }
    let target = vibe_core::platform::resolve_lenient(&root.join(&dir_portion));
    if target.starts_with(root) {
        return None;
    }
    if target.is_dir() {
        return Some(list_outside_root_children(&target, &dir_portion, &suffix));
    }
    let parent = target.parent()?;
    if parent == root || !parent.is_dir() {
        return Some(Vec::new());
    }
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(list_outside_root_children(
        parent,
        &posix_parent(&dir_portion),
        &name,
    ))
}

/// Reference `PathCompleter._split_outside_root_dir`: the directory part and the
/// name prefix of a partial that starts with `..`.
fn split_outside_root_dir(partial: &str) -> Option<(String, String)> {
    if partial.is_empty() || partial.split('/').next() != Some("..") {
        return None;
    }
    if partial.ends_with('/') {
        return Some((partial.trim_end_matches('/').to_owned(), String::new()));
    }
    let Some(slash) = partial.rfind('/') else {
        return Some((partial.to_owned(), String::new()));
    };
    let (directory, suffix) = (&partial[..slash], &partial[slash + 1..]);
    if suffix == ".." {
        return Some((format!("{directory}/{suffix}"), String::new()));
    }
    Some((directory.to_owned(), suffix.to_owned()))
}

/// Reference `PathCompleter._list_outside_root_children`: the visible children
/// of `directory` whose name starts with `suffix` regardless of case, sorted
/// case-insensitively, labeled under the directory part as typed.
fn list_outside_root_children(
    directory: &Path,
    dir_portion: &str,
    suffix: &str,
) -> Vec<CompletionCandidate> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let suffix_lower = suffix.to_lowercase();
    let mut matched = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') || suffix.starts_with('.'))
        .filter(|name| suffix_lower.is_empty() || name.to_lowercase().starts_with(&suffix_lower))
        .collect::<Vec<_>>();
    matched.sort_by_cached_key(|name| name.to_lowercase());
    matched
        .into_iter()
        .take(MAX_PATH_MATCHES)
        .map(|name| {
            let is_directory = directory.join(&name).is_dir();
            mention_candidate(format!("{dir_portion}/{name}"), is_directory)
        })
        .collect()
}

/// `Path(value).parent.as_posix()` for a relative POSIX path: empty and `.`
/// segments are dropped, and a single segment's parent is `.`.
fn posix_parent(value: &str) -> String {
    let segments = value
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>();
    match segments.split_last() {
        Some((_, parents)) if !parents.is_empty() => parents.join("/"),
        _ => ".".to_owned(),
    }
}
