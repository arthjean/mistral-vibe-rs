//! Whether a git reader's own repository can make it run a helper.
//!
//! Reference `git_repository_identity` and `git_repository_requires_approval`
//! in `vibe/core/tools/builtins/_shell_command_policy.py`. `git status` is
//! allowlisted because it reads, but a repository can point `core.pager`,
//! `core.fsmonitor`, a diff driver or a filter at any program, and the
//! allowlist was never meant to grant that program. The configuration is read
//! from the files the repository owns, without invoking git, which is the only
//! way to ask the question before anything runs.

use std::fs;
use std::path::{Path, PathBuf};

use super::command_policy::analyze_command_policy;

/// A value git reads as off.
const FALSE_VALUES: [&str; 5] = ["", "0", "false", "no", "off"];

/// The repository-owned configuration files governing `cwd`: the shared
/// `config` and the worktree's `config.worktree`, in that order.
fn git_config_paths(cwd: &Path) -> Vec<PathBuf> {
    for directory in cwd.ancestors() {
        let marker = directory.join(".git");
        if marker.is_dir() {
            return vec![marker.join("config"), marker.join("config.worktree")];
        }
        // A bare repository is its own git directory.
        if directory.join("HEAD").is_file()
            && directory.join("config").is_file()
            && directory.join("objects").is_dir()
            && directory.join("refs").is_dir()
        {
            return vec![directory.join("config"), directory.join("config.worktree")];
        }
        if !marker.is_file() {
            continue;
        }
        // A linked worktree or a submodule: `.git` is a file naming the git
        // directory, whose `commondir` names the shared one.
        let Ok(contents) = fs::read_to_string(&marker) else {
            return Vec::new();
        };
        let Some((prefix, value)) = contents.trim().split_once(':') else {
            return Vec::new();
        };
        if !prefix.eq_ignore_ascii_case("gitdir") {
            return Vec::new();
        }
        let mut git_dir = expand_home(value.trim());
        if !git_dir.is_absolute() {
            git_dir = directory.join(git_dir);
        }
        let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
            Ok(common) => {
                let common = PathBuf::from(common.trim());
                if common.is_absolute() {
                    common
                } else {
                    git_dir.join(common)
                }
            }
            Err(_) => git_dir.clone(),
        };
        return vec![common_dir.join("config"), git_dir.join("config.worktree")];
    }
    Vec::new()
}

fn expand_home(value: &str) -> PathBuf {
    if (value == "~" || value.starts_with("~/"))
        && let Some(home) = crate::config::user_home_directory()
    {
        return match value.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => home,
        };
    }
    PathBuf::from(value)
}

/// A stable name for the repository governing `cwd`, or [`None`] outside one.
///
/// The worktree's own git directory, resolved, so a symlinked checkout path
/// shares its approvals while two linked worktrees, whose `config.worktree`
/// may run different helpers, keep theirs apart.
pub(crate) fn git_repository_identity(cwd: &Path) -> Option<String> {
    let paths = git_config_paths(cwd);
    let git_dir = paths.last()?.parent()?.to_path_buf();
    let resolved = resolve_path(&git_dir);
    Some(resolved.to_string_lossy().into_owned())
}

/// One `(section, key, value)` entry, the section and key folded to
/// lowercase and a bare key read as `true`.
type ConfigEntry = (String, String, String);

fn git_config_entries(cwd: &Path) -> Vec<ConfigEntry> {
    let mut entries = Vec::new();
    for path in git_config_paths(cwd) {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let mut section = String::new();
        for raw in split_lines(&text) {
            let line = raw.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if let Some(header) = line.strip_prefix('[')
                && let Some(end) = header.find(']')
            {
                section = header[..end]
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                continue;
            }
            if let Some((key, value)) = config_entry(line) {
                entries.push((section.clone(), key.to_lowercase(), value));
            }
        }
    }
    entries
}

/// A `key` or `key = value` line, the value trimmed and unquoted.
fn config_entry(line: &str) -> Option<(String, String)> {
    let mut characters = line.char_indices();
    let (_, first) = characters.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let key_end = characters
        .find(|(_, character)| !(character.is_ascii_alphanumeric() || *character == '-'))
        .map_or(line.len(), |(index, _)| index);
    let key = &line[..key_end];
    let rest = line[key_end..].trim_start();
    if rest.is_empty() {
        return Some((key.to_owned(), "true".to_owned()));
    }
    // `key =` with nothing after it reads as `true` too, as the reference's
    // `value or "true"` does.
    let value = match rest.strip_prefix('=')?.trim() {
        "" => "true",
        value => value,
    };
    Some((key.to_owned(), value.trim_matches('"').to_owned()))
}

/// Python's `str.splitlines`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let breaks = matches!(
            character,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !breaks {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + character.len_utf8();
        if character == '\r'
            && let Some((next_index, '\n')) = characters.peek().copied()
        {
            characters.next();
            end = next_index + 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn is_active(value: &str) -> bool {
    !FALSE_VALUES.contains(&value.to_lowercase().as_str())
}

/// Whether an otherwise granted git reader could run repository-owned code
/// from `cwd`.
pub(crate) fn git_repository_requires_approval(tokens: &[String], cwd: &Path) -> bool {
    let policy = analyze_command_policy(tokens);
    let Some(subcommand) = tokens.get(1).filter(|_| policy.inspect_git_repository) else {
        return false;
    };
    let subcommand = subcommand.to_lowercase();
    let entries = git_config_entries(cwd);
    let active = |section: &str, keys: &[&str]| {
        entries.iter().any(|(entry_section, key, value)| {
            entry_section == section && keys.contains(&key.as_str()) && is_active(value)
        })
    };
    // An include can hide any of the settings below.
    if entries
        .iter()
        .any(|(section, _, _)| section == "include" || section == "includeif")
        || active("core", &["pager"])
        || active("pager", &[subcommand.as_str()])
    {
        return true;
    }
    if (subcommand == "diff" || subcommand == "status")
        && (active("core", &["fsmonitor"]) || active("filter", &["clean", "process"]))
    {
        return true;
    }
    if ["diff", "log", "status"].contains(&subcommand.as_str())
        && active("diff", &["command", "external", "textconv"])
    {
        return true;
    }
    // A remerge can run a merge driver, and a signature check a verifier.
    subcommand == "log" && (active("merge", &["driver"]) || active("gpg", &["program"]))
}

/// `path` made absolute with its symlinks resolved as far as it exists, the
/// rest folded lexically: Python's non-strict `Path.resolve`.
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            other => {
                resolved.push(other.as_os_str());
                if let Ok(canonical) = fs::canonicalize(&resolved) {
                    resolved = canonical;
                }
            }
        }
    }
    resolved
}

#[cfg(test)]
mod repository_tests;
