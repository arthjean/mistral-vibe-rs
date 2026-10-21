//! The list rules a tool resolves on its own, before the store positions a
//! call: the file-tool chain, the `task` tool's subagent lists, and the
//! `PurePath.match` both the sensitive patterns and the encoded path grants
//! are read under.

use std::path::Path;

use super::store::canonicalize_for_policy;
use super::{PermissionContext, PermissionMode, PermissionRequirement, path_pattern_matches};
use crate::matching::{fnmatch, pattern_matches};
use crate::scratchpad::is_scratchpad_path;
use crate::tools::config::SharedToolConfig;

/// Resolves what a file tool may do with `path`, before the trust roots.
///
/// Reference `resolve_file_tool_permission`, in its order: the scratchpad is
/// the runtime's own capability and is granted first; the denylist refuses
/// under `fnmatch` and the allowlist grants under [`path_pattern_matches`],
/// which reads the encoded grants a permanent approval writes there, both
/// against the resolved absolute path and both before anything else is read; a sensitive match then raises a `file_pattern`
/// requirement even where the configured permission is `always`. The
/// working-directory boundary is the one step this port answers elsewhere: the
/// roots live in the store, so the path travels on the context and
/// [`PermissionStore`](super::PermissionStore) positions it.
#[must_use]
pub fn resolve_file_tool_permission(
    path: &Path,
    tool: &str,
    settings: &SharedToolConfig,
    scratchpad: Option<&Path>,
) -> PermissionContext {
    if is_scratchpad_path(path, scratchpad) {
        return PermissionContext::settled(PermissionMode::Always)
            .because(format!("`{tool}` may always reach its own scratchpad"));
    }
    // The lists are matched against the resolved absolute path, which is what
    // makes `**/.env` mean the same thing here and upstream. A path that cannot
    // be resolved is matched as written rather than skipped: refusing to look is
    // not a reason to grant.
    let resolved = canonicalize_for_policy(path).unwrap_or_else(|_| path.to_path_buf());
    let subject = resolved.display().to_string();
    if let Some(pattern) = matched_pattern(&settings.denylist, &subject) {
        return PermissionContext::settled(PermissionMode::Never).because(format!(
            "`{subject}` matches the `{tool}` denylist entry `{pattern}`"
        ));
    }
    if let Some(pattern) = settings
        .allowlist
        .iter()
        .find(|pattern| path_pattern_matches(&subject, pattern))
    {
        return PermissionContext::settled(PermissionMode::Always).because(format!(
            "`{subject}` matches the `{tool}` allowlist entry `{pattern}`"
        ));
    }
    let mut context = PermissionContext::deferred().over_paths(vec![path.to_path_buf()]);
    if let Some(pattern) = matched_path_pattern(&settings.sensitive_patterns, &subject) {
        context.permission = Some(PermissionMode::Ask);
        context.reason = Some(format!(
            "`{subject}` matches the `{tool}` sensitive pattern `{pattern}`"
        ));
        context
            .requirements
            .push(PermissionRequirement::sensitive_file(&subject, tool));
    }
    context
}

/// Resolves what the `task` tool may delegate to `agent`.
///
/// Reference `TaskTool.resolve_permission`, in its order: the denylist refuses
/// first, so a name matching both lists is refused, the allowlist then grants,
/// and a name in neither list settles nothing and leaves the configured
/// permission to decide. Both lists are matched by the same glob rules the file
/// lists use rather than by equality, so `review-*` names a family.
#[must_use]
pub fn resolve_task_tool_permission(agent: &str, settings: &SharedToolConfig) -> PermissionContext {
    if let Some(pattern) = matched_pattern(&settings.denylist, agent) {
        return PermissionContext::settled(PermissionMode::Never).because(format!(
            "subagent `{agent}` matches the `task` denylist entry `{pattern}`"
        ));
    }
    if let Some(pattern) = matched_pattern(&settings.allowlist, agent) {
        return PermissionContext::settled(PermissionMode::Always).because(format!(
            "subagent `{agent}` matches the `task` allowlist entry `{pattern}`"
        ));
    }
    PermissionContext::deferred()
}

/// The first entry of `patterns` matching `subject` under `fnmatch`, in the
/// order the operator wrote them.
fn matched_pattern<'a>(patterns: &'a [String], subject: &str) -> Option<&'a String> {
    patterns.iter().find(|pattern| fnmatch(pattern, subject))
}

/// The first entry of `patterns` naming `subject` as a path, in the order the
/// operator wrote them.
///
/// Reference `matches_sensitive_pattern` (`vibe/core/tools/utils.py`) lowers
/// both sides before `PurePath.match`, so `.ENV` is as sensitive as `.env`.
/// `PurePath` is the host flavor, so on Windows `\` separates as `/` does.
pub(super) fn matched_path_pattern<'a>(
    patterns: &'a [String],
    subject: &str,
) -> Option<&'a String> {
    let flavor = PathFlavor::host();
    let subject = subject.to_lowercase();
    patterns
        .iter()
        .find(|pattern| flavor.pure_path_match(&pattern.to_lowercase(), &subject))
}

/// Whether a resolved absolute path is one a sensitive pattern names.
///
/// `grep` drops the matches it finds in such a file, which is how the
/// reference keeps a search from reading what `read_file` would ask about.
#[must_use]
pub fn matches_sensitive_pattern(patterns: &[String], resolved: &Path) -> bool {
    matched_path_pattern(patterns, &resolved.display().to_string()).is_some()
}

/// The `PurePath` class a match reads its paths under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathFlavor {
    Posix,
    Windows,
}

impl PathFlavor {
    /// `PurePath` itself, which the reference instantiates on the host flavor.
    fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    /// Whether `pattern` names `subject` under `PurePath.match`.
    ///
    /// Reference `resolve_file_tool_permission` runs `sensitive_patterns`
    /// through it, and [`path_pattern_matches`] runs an absolute POSIX
    /// allowlist glob through it, while the denylist stays on `fnmatch`. This
    /// matcher compares component by component and anchors on the right:
    /// `.env` names a file called `.env` at any depth, `/etc/*` names only a
    /// direct child of `/etc`, and a `*` never crosses a separator the way
    /// `fnmatch` lets it.
    ///
    /// Under the Windows flavor `\` and `/` both separate, the drive is the
    /// first component, a pattern carrying a drive is anchored like one
    /// carrying a root, and case never matters, so `C:\PROJ\.ENV` is named
    /// by `**/.env` and a pattern on `D:` never names a path on `C:`.
    ///
    /// A pattern naming no component matches nothing. Upstream raises
    /// `ValueError` there; failing to read a pattern is a reason to raise no
    /// requirement from it, never a reason to drop the ones the other patterns
    /// raise.
    fn pure_path_match(self, pattern: &str, subject: &str) -> bool {
        let (pattern, subject) = match self {
            Self::Posix => (pattern.to_owned(), subject.to_owned()),
            Self::Windows => {
                let fold = |value: &str| value.replace('\\', "/").to_lowercase();
                (fold(pattern), fold(subject))
            }
        };
        let expected = self.components(&pattern);
        let Some(width) = (!expected.is_empty()).then_some(expected.len()) else {
            return false;
        };
        let found = self.components(&subject);
        if self.is_anchored(&pattern) {
            // An anchored pattern compares against the whole path, root
            // included.
            return found.len() == width && components_match(&expected, &found);
        }
        let Some(tail) = found
            .len()
            .checked_sub(width)
            .and_then(|start| found.get(start..))
        else {
            return false;
        };
        components_match(&expected, tail)
    }

    /// Whether a pattern carries a drive or a root, which `PurePath.match`
    /// matches from the start rather than from the right.
    fn is_anchored(self, pattern: &str) -> bool {
        pattern.starts_with('/') || (self == Self::Windows && pattern.chars().nth(1) == Some(':'))
    }

    /// The components `PurePath` compares by, keeping the root as an empty
    /// leading component so an absolute path and an absolute pattern line up.
    ///
    /// Only `/` separates: the Windows flavor has turned every `\` into one
    /// already, and its drive (`c:`) is the leading component a root
    /// follows, as `str(PureWindowsPath)` spells it. A UNC path keeps both
    /// leading separators, as an empty component each.
    fn components(self, value: &str) -> Vec<&str> {
        let mut components = Vec::new();
        if self == Self::Windows && value.starts_with("//") {
            components.extend(["", ""]);
        } else if value.starts_with('/') {
            components.push("");
        }
        components.extend(
            value
                .split('/')
                .filter(|component| !component.is_empty() && *component != "."),
        );
        components
    }
}

/// `PurePosixPath.match`, which [`path_pattern_matches`] reads an absolute
/// POSIX glob with whatever the host.
pub(super) fn pure_path_match(pattern: &str, subject: &str) -> bool {
    PathFlavor::Posix.pure_path_match(pattern, subject)
}

/// Whether every pattern component names the component facing it.
fn components_match(expected: &[&str], found: &[&str]) -> bool {
    expected
        .iter()
        .zip(found)
        .all(|(pattern, component)| component_matches(pattern, component))
}

/// Whether one pattern component names one path component.
fn component_matches(pattern: &str, component: &str) -> bool {
    match pattern {
        // `**` stands for any single component, the root included, which is
        // what lets the shipped `**/.env` name a file sitting at the root.
        "**" => true,
        // A lone `*` needs something to name, so it never stands in for the
        // root the way `**` does.
        "*" => !component.is_empty(),
        _ => pattern_matches(pattern, component),
    }
}
