//! How an approval names the paths it covers.
//!
//! Reference `vibe/permissions.py` since v2.25.8. A path outside every root is
//! granted by an encoded pattern, `vibe-path:<scope>:<normalized path>`, rather
//! than by a glob: the path is written literally, so a `*` or a `[` in a file
//! name never widens the grant, and the scope says whether the grant stops at
//! that path or reaches everything under it. The same matcher still reads the
//! path globs an operator writes into a tool's `allowlist` and that earlier
//! approvals stored, without letting a `*` cross a separator.
//!
//! Paths are compared as Python compares them on a POSIX host: a path carrying
//! a drive, a leading `//` or a backslash reads as a Windows path, normalized by
//! `ntpath.normpath` and folded by `ntpath.normcase`; every other path is
//! normalized by `posixpath.normpath` and keeps its case.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{PermissionRequirement, PermissionScope};
use crate::matching::pattern_matches;

/// The word every encoded grant starts with.
const PATH_GRANT_PREFIX: &str = "vibe-path";

/// How far an approval of a path reaches.
///
/// Reference `PathGrantScope`. The wire value is also the middle field of the
/// encoded grant, so the spelling is load-bearing twice over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathGrantScope {
    /// The path itself and nothing else.
    Exact,
    /// The path and every path under it.
    DirectoryRecursive,
}

impl PathGrantScope {
    /// Every scope, in the order the reference declares them.
    pub const ALL: [Self; 2] = [Self::Exact, Self::DirectoryRecursive];

    /// The wire value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::DirectoryRecursive => "directory_recursive",
        }
    }

    /// The scope a wire value names, matched exactly as a `StrEnum` lookup is.
    fn from_label(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.label() == value)
    }
}

/// The encoded grant for `path` under `scope`.
///
/// Reference `path_grant_pattern`: the path is normalized first, so two
/// spellings of one path encode alike.
#[must_use]
pub fn path_grant_pattern(path: &str, scope: PathGrantScope) -> String {
    format!(
        "{PATH_GRANT_PREFIX}:{}:{}",
        scope.label(),
        normalized_path(path)
    )
}

/// Whether `pattern` covers `path`.
///
/// Reference `path_pattern_matches`, which both the file-tool allowlist and an
/// outside-directory approval are read with. An encoded grant compares
/// normalized paths, a recursive one stopping at a separator so `/srv/app`
/// never covers `/srv/application`. Any other pattern is a path glob: an
/// absolute one is matched component by component and anchored at the root, as
/// `PurePath.match` anchors it, and a relative one against the whole path, as
/// `fnmatch` reads it.
#[must_use]
pub fn path_pattern_matches(path: &str, pattern: &str) -> bool {
    let normalized = normalized_path(path);
    if let Some((scope, granted)) = parse_path_grant_pattern(pattern) {
        return match scope {
            PathGrantScope::Exact => normalized == granted,
            PathGrantScope::DirectoryRecursive => {
                let separator = if is_windows_path(&granted) { '\\' } else { '/' };
                normalized == granted
                    || normalized.starts_with(&format!(
                        "{}{separator}",
                        granted.trim_end_matches(separator)
                    ))
            }
        };
    }
    let windows = is_windows_path(path) || is_windows_path(pattern);
    if windows {
        if is_windows_absolute(pattern) {
            return windows_components_match(pattern, &normalized);
        }
        return pattern_matches(&windows_normcase(pattern), &normalized);
    }
    if pattern.starts_with('/') {
        return super::pure_path_match(pattern, &normalized);
    }
    pattern_matches(pattern, &normalized)
}

/// Whether a shell allowlist entry `pattern` grants `path`.
///
/// Reference `path_grant_pattern_matches`: a shell allowlist also holds command
/// prefixes and wildcards (`cat`, `npm *`, `*`), which must never clear a path
/// outside the workdir because `fnmatch` would accept them. Only an encoded
/// grant or an absolute path glob is read as a path grant, and then exactly as
/// [`path_pattern_matches`] reads it.
#[must_use]
pub fn path_grant_pattern_matches(path: &str, pattern: &str) -> bool {
    (parse_path_grant_pattern(pattern).is_some() || is_legacy_path_grant_pattern(pattern))
        && path_pattern_matches(path, pattern)
}

/// Reference `_is_legacy_path_grant_pattern`: an absolute path glob such as
/// `/tmp/*` or `C:\tmp\*`, read under the grammar the pattern itself is
/// written in.
fn is_legacy_path_grant_pattern(pattern: &str) -> bool {
    if !pattern.contains(['*', '?', '[']) {
        return false;
    }
    if is_windows_path(pattern) {
        is_windows_absolute(pattern)
    } else {
        pattern.starts_with('/')
    }
}

/// The root a recursive grant of `path` would reach, when there is one.
///
/// Reference `shell_path_scope_root`: only a directory the session can list and
/// enter offers a recursive grant, and it is the directory itself, so a file
/// never offers one.
#[must_use]
pub fn path_scope_root(path: &Path) -> Option<String> {
    (path.is_dir() && traversable(path)).then(|| path.display().to_string())
}

/// Python `os.access(path, os.R_OK | os.X_OK)`.
#[cfg(unix)]
fn traversable(path: &Path) -> bool {
    use nix::unistd::{AccessFlags, access};
    access(path, AccessFlags::R_OK | AccessFlags::X_OK).is_ok()
}

/// Python `os.access` on Windows checks only the read-only attribute, which
/// neither flag reads, so an existing directory is always traversable.
#[cfg(not(unix))]
fn traversable(_path: &Path) -> bool {
    true
}

/// The scopes an approval of `requirements` offers its operator.
///
/// Reference `available_path_scopes` (`vibe/app_server/_approval_permissions.py`):
/// nothing when no requirement reaches outside the workdir, and otherwise a
/// single scope. The recursive one is offered only when every outside target
/// is itself the root a recursive grant would reach, which is a directory the
/// session can enter; one file among them leaves the exact scope alone.
#[must_use]
pub fn available_path_scopes(requirements: &[PermissionRequirement]) -> Vec<PathGrantScope> {
    let mut outside = requirements
        .iter()
        .filter(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
        .peekable();
    if outside.peek().is_none() {
        return Vec::new();
    }
    let recursive = outside.all(|requirement| {
        requirement.path_scope_root.as_deref().is_some_and(|root| {
            path_pattern_matches(
                root,
                &path_grant_pattern(&requirement.invocation_pattern, PathGrantScope::Exact),
            )
        })
    });
    vec![if recursive {
        PathGrantScope::DirectoryRecursive
    } else {
        PathGrantScope::Exact
    }]
}

/// Why an operator's path scope cannot be applied to an approval.
///
/// The two refusals of reference `approval_grant_permissions`, raised once the
/// callback is answered, which fails the turn rather than the answer. The
/// sentences are this port's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathScopeRefusal {
    /// A scope was chosen for an approval that names no outside path.
    #[error("no path outside the workdir was asked about, so no path scope can be chosen")]
    WithoutPaths,
    /// A scope was chosen that the approval did not offer.
    #[error("the approval did not offer the `{}` path scope", .0.label())]
    NotOffered(PathGrantScope),
}

/// The scope an approval of `requirements` is granted under, given what the
/// operator chose.
///
/// Reference `approval_grant_permissions`: no choice means the exact scope,
/// and a choice has to be one [`available_path_scopes`] offered. An approval
/// that offered none is granted as asked, which the exact scope leaves
/// untouched since it names no outside path.
///
/// # Errors
///
/// The [`PathScopeRefusal`] the reference raises for that choice.
pub fn approval_path_scope(
    requirements: &[PermissionRequirement],
    selected: Option<PathGrantScope>,
) -> Result<PathGrantScope, PathScopeRefusal> {
    let offered = available_path_scopes(requirements);
    match selected {
        Some(_) if offered.is_empty() => Err(PathScopeRefusal::WithoutPaths),
        None => Ok(PathGrantScope::Exact),
        Some(scope) if offered.contains(&scope) => Ok(scope),
        Some(scope) => Err(PathScopeRefusal::NotOffered(scope)),
    }
}

/// `requirements` with every outside path granted under `scope`.
///
/// Reference `scope_required_permissions`: only an outside-directory
/// requirement changes, and only its session pattern, which becomes the exact
/// grant of its target or the recursive grant of its root, the target itself
/// standing in for a root it lacks.
#[must_use]
pub fn scope_required_permissions(
    requirements: &[PermissionRequirement],
    scope: PathGrantScope,
) -> Vec<PermissionRequirement> {
    requirements
        .iter()
        .map(|requirement| {
            if requirement.scope != PermissionScope::OutsideDirectory {
                return requirement.clone();
            }
            let root = match scope {
                PathGrantScope::Exact => &requirement.invocation_pattern,
                PathGrantScope::DirectoryRecursive => requirement
                    .path_scope_root
                    .as_ref()
                    .unwrap_or(&requirement.invocation_pattern),
            };
            PermissionRequirement {
                session_pattern: path_grant_pattern(root, scope),
                ..requirement.clone()
            }
        })
        .collect()
}

/// What an approval option granting `scope` names, for an option that lasts
/// the session or one that is kept.
///
/// Reference `path_scope_label` (`vibe/utils/tool_presentation.py`), which
/// both the terminal and the editor compose their option names from: one
/// target or several, and for the exact scope a session option that stresses
/// it reaches nothing more. The words are this port's own.
#[must_use]
pub fn path_scope_target(
    requirements: &[PermissionRequirement],
    scope: PathGrantScope,
    for_session: bool,
) -> &'static str {
    let several = requirements
        .iter()
        .filter(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
        .nth(1)
        .is_some();
    match (scope, several, for_session) {
        (PathGrantScope::DirectoryRecursive, true, _) => "these folders and their contents",
        (PathGrantScope::DirectoryRecursive, false, _) => "this folder and its contents",
        (PathGrantScope::Exact, true, true) => "only these paths",
        (PathGrantScope::Exact, false, true) => "only this file",
        (PathGrantScope::Exact, true, false) => "these paths",
        (PathGrantScope::Exact, false, false) => "this file",
    }
}

/// The scope and the normalized path an encoded grant names, or [`None`] for
/// anything else, an unknown scope included.
fn parse_path_grant_pattern(pattern: &str) -> Option<(PathGrantScope, String)> {
    let (prefix, remainder) = pattern.split_once(':')?;
    if prefix != PATH_GRANT_PREFIX {
        return None;
    }
    let (scope, path) = remainder.split_once(':')?;
    Some((PathGrantScope::from_label(scope)?, normalized_path(path)))
}

/// Reference `_normalized_path`.
fn normalized_path(path: &str) -> String {
    if is_windows_path(path) {
        windows_normcase(&windows_normpath(path))
    } else {
        posix_normpath(path)
    }
}

/// Reference `_is_windows_path`: `PureWindowsPath(path).drive` is not empty, or
/// the path carries a backslash. A drive is a second character `:` or a
/// leading pair of separators, which is why `//server` reads as Windows.
fn is_windows_path(path: &str) -> bool {
    path.contains('\\')
        || path.chars().nth(1) == Some(':')
        || path.replace('\\', "/").starts_with("//")
}

/// `PureWindowsPath(pattern).is_absolute()`: a drive followed by a root, or a
/// UNC drive, which always carries one.
fn is_windows_absolute(pattern: &str) -> bool {
    let pattern = pattern.replace('/', "\\");
    let (drive, root, _) = windows_splitroot(&pattern);
    !drive.is_empty() && (!root.is_empty() || drive.starts_with('\\'))
}

/// `PureWindowsPath(path).match(pattern)` for an absolute pattern: every
/// component, the drive included, matched in order and without case.
fn windows_components_match(pattern: &str, path: &str) -> bool {
    let components = |value: &str| {
        windows_normcase(value)
            .split('\\')
            .filter(|component| !component.is_empty() && *component != ".")
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    };
    let expected = components(pattern);
    let found = components(path);
    expected.len() == found.len()
        && expected
            .iter()
            .zip(&found)
            .all(|(pattern, component)| pattern_matches(pattern, component))
}

/// Python `ntpath.normcase` on a POSIX host.
fn windows_normcase(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

/// Python `ntpath.splitroot` over a path whose separators are backslashes.
fn windows_splitroot(path: &str) -> (&str, &str, &str) {
    if let Some(rest) = path.strip_prefix('\\') {
        if !rest.starts_with('\\') {
            return ("", "\\", rest);
        }
        // A UNC or device drive: `\\server\share`, or `\\?\UNC\server\share`.
        let start = if path
            .get(..8)
            .is_some_and(|head| head.eq_ignore_ascii_case("\\\\?\\UNC\\"))
        {
            8
        } else {
            2
        };
        let Some(index) = path.get(start..).and_then(|tail| tail.find('\\')) else {
            return (path, "", "");
        };
        let index = start.saturating_add(index);
        let Some(second) = path
            .get(index.saturating_add(1)..)
            .and_then(|tail| tail.find('\\'))
        else {
            return (path, "", "");
        };
        let second = index.saturating_add(1).saturating_add(second);
        return (
            path.get(..second).unwrap_or(path),
            path.get(second..second.saturating_add(1)).unwrap_or(""),
            path.get(second.saturating_add(1)..).unwrap_or(""),
        );
    }
    let mut characters = path.char_indices();
    if let (Some(_), Some((colon, ':'))) = (characters.next(), characters.next()) {
        let drive_end = colon.saturating_add(1);
        let drive = path.get(..drive_end).unwrap_or("");
        let rest = path.get(drive_end..).unwrap_or("");
        return match rest.strip_prefix('\\') {
            Some(tail) => (drive, "\\", tail),
            None => (drive, "", rest),
        };
    }
    ("", "", path)
}

/// Python `ntpath.normpath`.
fn windows_normpath(path: &str) -> String {
    let path = path.replace('/', "\\");
    let (drive, root, tail) = windows_splitroot(&path);
    let mut components: Vec<&str> = Vec::new();
    for component in tail.split('\\') {
        match component {
            "" | "." => {}
            ".." => match components.last() {
                Some(&last) if last != ".." => {
                    components.pop();
                }
                None if !root.is_empty() => {}
                _ => components.push(component),
            },
            _ => components.push(component),
        }
    }
    let prefix = format!("{drive}{root}");
    if prefix.is_empty() && components.is_empty() {
        return ".".to_owned();
    }
    format!("{prefix}{}", components.join("\\"))
}

/// Python `posixpath.normpath`.
fn posix_normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." if initial.is_empty() && components.last().is_none_or(|last| *last == "..") => {
                components.push(component);
            }
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    let normalized = format!("{initial}{}", components.join("/"));
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}
