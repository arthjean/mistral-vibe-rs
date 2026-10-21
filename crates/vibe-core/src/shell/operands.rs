//! The operands a call reaches outside the workspace roots, positioned the way
//! the reference resolves them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::command_policy::path_candidates;
use super::guardrails::host_word_split;
use super::lexer::{WordSplit, split_tokens};
use super::{ShellCommandLists, ShellFlavor, ShellPolicyContext, ShellResolver};
use crate::platform::{PathPolicyError, Platform, PolicyPath, parse_policy_path};
use crate::policy::path_grant_pattern_matches;
use crate::scratchpad::is_scratchpad_path;

/// The commands whose path operands are inspected.
///
/// Reference `_PATH_COMMANDS`, the union of `_MUTATING_PATH_COMMANDS` and the
/// POSIX read-only commands. The reference documents it as a superset of the
/// read-only allowlist for a reason this port keeps: a command that can be
/// auto-allowed must have its operands checked first, or `grep secret
/// /etc/passwd` reads outside the workspace without ever asking.
const MUTATING_PATH_COMMANDS: [&str; 8] =
    ["cd", "chmod", "chown", "cp", "mkdir", "mv", "rm", "touch"];

/// Whether `program` has its path operands inspected under `flavor`.
///
/// The set is the reference union: the eight mutating commands and every
/// read-only command of the branch, which is what makes it a superset of the
/// read-only allowlist by construction.
pub(super) fn inspects_paths(program: &str) -> bool {
    MUTATING_PATH_COMMANDS.contains(&program)
        || crate::tools::config::shell_read_only_commands(true).contains(&program)
}

/// The paths a call reaches outside every root, sorted and deduplicated, and
/// the operands they came from.
///
/// Reference `_collect_outside_paths`: only a path-inspecting command's
/// operands are considered, a flag and a `chmod` mode are skipped, a token that
/// does not look like a path is skipped, the scratchpad is skipped, and every
/// operand left outside is named by its resolved path, so two files in one
/// directory are two approvals.
pub(super) fn collect_outside_paths(
    flavor: ShellFlavor,
    segments: &[String],
    context: &ShellPolicyContext,
) -> (Vec<String>, Vec<String>) {
    let mut paths = BTreeSet::new();
    let mut operands = Vec::new();
    // The managed resolver positions the call's own directory too.
    if context.resolver == ShellResolver::Managed {
        if let Some(cwd) = &context.unpositioned_cwd {
            paths.insert(cwd.clone());
        } else if let Some(path) = escaping_directory(context, &context.working_directory) {
            paths.insert(path);
        }
    }
    // Reference `_split_command_tokens`: the legacy resolver keeps a backslash
    // literal on a Windows host, because a path like `C:\Users\me` would
    // otherwise lose its separators to the POSIX escape rule; the managed one
    // always escapes.
    let mode = match context.resolver {
        ShellResolver::Legacy => host_word_split(),
        ShellResolver::Managed => WordSplit::Posix,
    };
    for segment in segments {
        let tokens = split_tokens(segment, mode);
        let Some(program) = tokens.first() else {
            continue;
        };
        for token in path_candidates(&tokens, inspects_paths(program)) {
            if !looks_like_path(flavor, &token) {
                continue;
            }
            if let Some(path) = escaping_path(flavor, context, &token) {
                paths.insert(path);
            }
            operands.push(token);
        }
    }
    (paths.into_iter().collect(), operands)
}

/// Drops every outside path a shell allowlist entry already grants.
///
/// Reference `resolve_permission` of every shell family since v2.25.8: a
/// permanent approval writes the encoded grant into the tool's allowlist, and
/// an operator may write an absolute path glob there, so both are read back
/// before the call is asked about; a command prefix never is.
pub(super) fn retain_ungranted(outside: &mut Vec<String>, lists: &ShellCommandLists) {
    outside.retain(|path| {
        !lists
            .allowlist
            .iter()
            .any(|pattern| path_grant_pattern_matches(path, pattern))
    });
}

/// Reference `_collect_outside_paths`: only a token shaped like a path is
/// resolved, so a `grep` pattern and a `chmod` mode never become directories.
///
/// Git Bash also reads a token opening with a drive (`C:`, `C:notes.txt`) as a
/// path on that drive's current directory, which the reference skips and the
/// policy cannot position, so it is asked about here rather than granted.
pub(super) fn looks_like_path(flavor: ShellFlavor, token: &str) -> bool {
    token.starts_with('/')
        || token.starts_with('~')
        || token.starts_with('.')
        || token.contains('/')
        || token.contains('\\')
        || (flavor == ShellFlavor::GitBash && has_drive_prefix(token))
}

/// Whether `token` opens with a drive letter and its colon.
fn has_drive_prefix(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':')
}

/// The path grammar an operand of `flavor` is written in.
pub(super) fn operand_platform(flavor: ShellFlavor) -> Platform {
    match flavor {
        ShellFlavor::Posix => Platform::Posix,
        ShellFlavor::GitBash => Platform::GitBash,
        ShellFlavor::Cmd | ShellFlavor::PowerShell => Platform::Windows,
    }
}

/// The resolved path `token` reaches outside every root, or [`None`] when it
/// stays inside.
///
/// The operand is parsed under the interpreter's own path grammar and then
/// positioned on the host's, which is what lets a Git Bash `/c/work/notes.txt`
/// land on the Windows workspace root it names.
pub(super) fn escaping_path(
    flavor: ShellFlavor,
    context: &ShellPolicyContext,
    token: &str,
) -> Option<String> {
    let platform = operand_platform(flavor);
    let expanded = expand_home(token);
    let Ok(path) = normalize_operand(platform, context, &expanded) else {
        // A path the policy cannot position is treated as outside rather than
        // as inside, which is the direction the reference resolves an
        // unresolvable operand in, and is named as written.
        return Some(expanded);
    };
    let host = policy_path_to_host(&path);
    if let Some(host) = host.as_deref()
        && is_scratchpad_path(host, context.scratchpad.as_deref())
    {
        return None;
    }
    // A path that exists is positioned on the filesystem too, so a symlink
    // pointing out of the workspace is caught. One that does not exist yet is
    // positioned lexically: `cp x <workdir>/copy.txt` names a file the call is
    // about to create, and refusing it for not existing would ask about every
    // write into the workspace.
    let lexically_inside = inside_any_root(&path, &context.roots);
    let inside = match host.as_deref() {
        Some(host) if host.exists() => {
            lexically_inside && host_path_is_authorized(&path, context) != Some(false)
        }
        Some(_) | None => lexically_inside,
    };
    (!inside).then(|| outside_name(&path))
}

/// A managed call's own directory when it leaves the workspace, which the
/// reference collects as the resolved directory itself.
pub(super) fn escaping_directory(
    context: &ShellPolicyContext,
    directory: &PolicyPath,
) -> Option<String> {
    let host = policy_path_to_host(directory);
    if let Some(host) = host.as_deref()
        && is_scratchpad_path(host, context.scratchpad.as_deref())
    {
        return None;
    }
    let inside = inside_any_root(directory, &context.roots)
        && match host.as_deref() {
            Some(host) if host.exists() => {
                host_path_is_authorized(directory, context) != Some(false)
            }
            Some(_) | None => true,
        };
    (!inside).then(|| outside_name(directory))
}

/// The text an outside requirement names `path` by.
///
/// Reference `resolve_tool_path`, a non-strict `Path.resolve()`: a path on the
/// host is resolved through every symlink its existing prefix crosses, so an
/// approval names where the call actually reaches rather than the link it went
/// through, and without the verbatim prefix Windows canonicalization adds. A
/// path in a grammar the host cannot open stays as the policy rendered it, as
/// `str(PureWindowsPath(...))` does on a POSIX host.
fn outside_name(path: &PolicyPath) -> String {
    match policy_path_to_host(path) {
        Some(host) => crate::platform::strip_verbatim_prefix(
            &crate::platform::resolve_lenient(&host).to_string_lossy(),
        ),
        None => render_policy_path(path),
    }
}

/// The separator `platform` writes a path with.
fn separator_for(platform: Platform) -> char {
    if platform == Platform::Windows {
        '\\'
    } else {
        '/'
    }
}

/// The text a requirement names `path` by, in the separator its platform uses.
pub(super) fn render_policy_path(path: &PolicyPath) -> String {
    let separator = separator_for(path.platform);
    let mut rendered = path.root.clone();
    for component in &path.components {
        if !rendered.is_empty() && !rendered.ends_with(separator) {
            rendered.push(separator);
        }
        rendered.push_str(component);
    }
    if rendered.is_empty() {
        separator.to_string()
    } else {
        rendered
    }
}

/// `token` with a leading `~` replaced by the operator's home directory.
///
/// Reference `_collect_outside_paths` calls `Path.expanduser` before resolving,
/// without which `cat ~/.ssh/id_rsa` would position under a literal `~`
/// directory inside the workspace and never raise a requirement. On Windows
/// `expanduser` also reads `~\`, and joins the rest under the Windows home.
pub(super) fn expand_home(token: &str) -> String {
    crate::platform::expand_home(Path::new(token))
        .to_string_lossy()
        .into_owned()
}

pub(super) fn normalize_operand(
    platform: Platform,
    context: &ShellPolicyContext,
    operand: &str,
) -> Result<PolicyPath, PathPolicyError> {
    match parse_policy_path(platform, operand) {
        Ok(parsed) => Ok(position(context, parsed)),
        Err(PathPolicyError::ParentTraversal) => fold_traversal(platform, context, operand),
        Err(error) => Err(error),
    }
}

/// A parsed operand placed on the policy: an absolute one carried onto the
/// host's grammar, a relative one joined onto the working directory.
fn position(context: &ShellPolicyContext, mut parsed: PolicyPath) -> PolicyPath {
    if parsed.root.is_empty() {
        let mut combined = context.working_directory.clone();
        combined.components.extend(parsed.components);
        return combined;
    }
    parsed.platform = context.platform;
    parsed
}

/// `operand` positioned on the working directory with its `..` components
/// folded away.
///
/// Reference `_collect_outside_paths` calls `Path.resolve()`, which folds a
/// traversal instead of refusing it, so `cat sub/../notes.txt` is measured where
/// it actually reads. [`parse_policy_path`] refuses one, because the file tools
/// it also serves must never let a `..` cross a root; the folding therefore
/// happens here, on a shell operand only. Without it a traversal is treated as
/// unresolvable, which asks about a path that never left the workspace.
fn fold_traversal(
    platform: Platform,
    context: &ShellPolicyContext,
    operand: &str,
) -> Result<PolicyPath, PathPolicyError> {
    let separators = path_separators(platform);
    // The head is everything before the first ascent, which the same grammar the
    // rest of the policy uses reads the root from. Every separator is one ASCII
    // byte, so the running offset addresses the operand directly.
    let mut offset = 0_usize;
    let mut head_end = operand.len();
    for part in operand.split(separators) {
        if part == ".." {
            head_end = offset;
            break;
        }
        offset += part.len() + 1;
    }
    let (head, rest) = operand.split_at(head_end);
    let mut folded = if head.is_empty() {
        context.working_directory.clone()
    } else {
        position(context, parse_policy_path(platform, head)?)
    };
    for part in rest.split(separators) {
        match part {
            "" | "." => {}
            ".." => {
                // At a root an ascent is the root itself, which is what a
                // filesystem answers. A relative operand that has no root left
                // to stand on cannot be positioned at all.
                if folded.components.pop().is_none() && folded.root.is_empty() {
                    return Err(PathPolicyError::ParentTraversal);
                }
            }
            component => folded.components.push(component.to_owned()),
        }
    }
    Ok(folded)
}

/// The separators an operand of `platform` is written with.
///
/// Git Bash accepts both, which is why only POSIX reads a backslash as an
/// ordinary character.
fn path_separators(platform: Platform) -> &'static [char] {
    if platform == Platform::Posix {
        &['/']
    } else {
        &['/', '\\']
    }
}

fn inside_any_root(path: &PolicyPath, roots: &[PolicyPath]) -> bool {
    roots.iter().any(|root| {
        root.root.eq_ignore_ascii_case(&path.root)
            && path.components.len() >= root.components.len()
            && path
                .components
                .iter()
                .zip(&root.components)
                .all(|(left, right)| {
                    if path.platform == Platform::Windows {
                        left.eq_ignore_ascii_case(right)
                    } else {
                        left == right
                    }
                })
    })
}

fn host_path_is_authorized(path: &PolicyPath, context: &ShellPolicyContext) -> Option<bool> {
    let working_directory = policy_path_to_host(&context.working_directory)?;
    crate::platform::canonicalize(&working_directory).ok()?;
    let candidate = policy_path_to_host(path)?;
    let canonical = match crate::platform::canonicalize(candidate) {
        Ok(canonical) => canonical,
        Err(_) => return Some(false),
    };
    Some(context.roots.iter().any(|root| {
        policy_path_to_host(root)
            .and_then(|root| crate::platform::canonicalize(root).ok())
            .is_some_and(|root| canonical.starts_with(root))
    }))
}

pub(super) fn policy_path_to_host(path: &PolicyPath) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if path.platform != Platform::Posix {
            return None;
        }
        let mut result = PathBuf::from(&path.root);
        result.extend(&path.components);
        Some(result)
    }
    #[cfg(windows)]
    {
        if path.platform == Platform::Posix {
            return None;
        }
        let mut result = PathBuf::from(&path.root);
        result.extend(&path.components);
        Some(result)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}
