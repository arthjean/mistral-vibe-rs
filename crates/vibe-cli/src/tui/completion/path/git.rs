//! The index of a git work tree: what `git ls-files` lists.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use vibe_core::worktree::trusted_git_executable;

use super::{IndexedPath, indexed_path};

/// Reference `FileIndexStore._list_git_entries`: what `git ls-files` lists
/// under `root`, tracked and untracked but not ignored, with every parent
/// directory added, or [`None`] when `root` is in no work tree or git cannot
/// run. A listed path that no longer exists is skipped. The repository's
/// `core.fsmonitor` is cleared for the call, which changes no answer and keeps
/// a checkout from running a command of its choosing.
pub(super) fn list_git_entries(root: &Path) -> Option<BTreeMap<String, IndexedPath>> {
    let git = trusted_git_executable(root)?;
    let output = Command::new(git)
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.fsmonitor=",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut entries = BTreeMap::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        let Ok(rel) = std::str::from_utf8(raw) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let path = root.join(rel);
        if !path.exists() {
            continue;
        }
        add_git_entry(&mut entries, rel, &path);
    }
    Some(entries)
}

/// Adds one listed path and every directory above it, as reference
/// `_add_git_entry` does: a parent already present keeps its entry.
fn add_git_entry(entries: &mut BTreeMap<String, IndexedPath>, rel: &str, path: &Path) {
    let segments = rel
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>();
    let Some((name, parents)) = segments.split_last() else {
        return;
    };
    for depth in 1..=parents.len() {
        let parent = segments[..depth].join("/");
        entries
            .entry(parent.clone())
            .or_insert_with(|| indexed_path(parent, segments[depth - 1].to_owned(), true));
    }
    let normalized = segments.join("/");
    entries.insert(
        normalized.clone(),
        indexed_path(normalized, (*name).to_owned(), path.is_dir()),
    );
}
