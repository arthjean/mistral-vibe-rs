//! The per-program guardrails and the two denylists, run over every command a
//! wrapper exposes in the directory each one runs in.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::command_policy::analyze_command_policy;
use super::extract::analyze_text;
use super::lexer::{WordSplit, split_tokens};
use super::operands::{expand_home, policy_path_to_host, render_policy_path};
use super::repository::{git_repository_identity, git_repository_requires_approval};
use super::{ShellCommandLists, ShellPolicyContext};
use crate::platform::PolicyPath;
use crate::policy::PermissionRequirement;

/// The requirements the per-program guardrails raise, or the refusal a
/// denylist answers with.
///
/// Reference `_resolve_guardrail_permission`: every command a wrapper exposes
/// is checked, the directory each one runs in is tracked through `cd`,
/// `pushd` and `popd`, and a git reader is keyed to every repository it may
/// inspect. Requirements are keyed by the command text, the first occurrence
/// deciding the position and the last the value.
pub(super) fn guardrail_requirements(
    parts: &[String],
    context: &ShellPolicyContext,
    lists: &ShellCommandLists,
    dialect: GuardrailDialect,
) -> Result<Vec<PermissionRequirement>, String> {
    // PowerShell only runs on a Windows host, where the reference keeps a
    // backslash literal.
    let mode = match dialect {
        GuardrailDialect::Windows => WordSplit::LiteralBackslash,
        GuardrailDialect::Legacy | GuardrailDialect::Managed => host_word_split(),
    };
    let mut required: Vec<(String, PermissionRequirement)> = Vec::new();
    let mut cwds = BTreeSet::from([host_directory(&context.working_directory)]);
    let mut cwd_is_unknown = false;
    for part in expand_guardrail_commands(parts, mode) {
        let (denied, standalone) = match dialect {
            GuardrailDialect::Legacy => (lists.denied(&part), lists.denied_standalone(&part)),
            GuardrailDialect::Managed => (
                lists.denied_by_basename(&part),
                lists.denied_standalone(&part),
            ),
            GuardrailDialect::Windows => (
                lists.windows_denied(&part),
                lists.windows_denied_standalone(&part),
            ),
        };
        if let Some(pattern) = denied {
            return Err(format!("`{part}` matches the denylist entry `{pattern}`"));
        }
        if let Some(entry) = standalone {
            return Err(format!("`{entry}` is refused as a standalone command"));
        }
        let tokens = split_tokens(&part, mode);
        cwd_is_unknown = update_guardrail_cwds(&tokens, &mut cwds) || cwd_is_unknown;
        let policy = analyze_command_policy(&tokens);
        let repository = policy.inspect_git_repository
            && (cwd_is_unknown
                || cwds
                    .iter()
                    .any(|cwd| git_repository_requires_approval(&tokens, cwd)));
        if !(policy.requires_approval || repository) {
            continue;
        }
        let pattern = if policy.inspect_git_repository {
            git_repository_pattern(&part, &cwds, cwd_is_unknown)
        } else {
            part.clone()
        };
        let requirement = PermissionRequirement {
            label: part.clone(),
            ..PermissionRequirement::exact_command(&pattern)
        };
        match required.iter_mut().find(|(key, _)| *key == part) {
            Some((_, slot)) => *slot = requirement,
            None => required.push((part, requirement)),
        }
    }
    Ok(required
        .into_iter()
        .map(|(_, requirement)| requirement)
        .collect())
}

/// Which resolver's denylist matching the guardrails apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GuardrailDialect {
    /// The command as written.
    Legacy,
    /// The command as written, or with its program reduced to its basename.
    Managed,
    /// Every PowerShell form of the command: basename, suffix and alias.
    Windows,
}

/// How the guardrails split a command into tokens on this host: the reference
/// keeps a backslash literal on Windows.
pub(super) fn host_word_split() -> WordSplit {
    if cfg!(windows) {
        WordSplit::LiteralBackslash
    } else {
        WordSplit::Posix
    }
}

/// The last component of `program` under the host's separators, Python's
/// `os.path.basename`.
pub(super) fn host_basename(program: &str) -> &str {
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    program.rsplit(separators).next().unwrap_or(program)
}

/// The host directory a policy path names, rendered when it is not one.
fn host_directory(path: &PolicyPath) -> PathBuf {
    policy_path_to_host(path).unwrap_or_else(|| PathBuf::from(render_policy_path(path)))
}

/// Reference `_wrapped_guardrail_commands`: the command an `eval` or an `exec`
/// runs, as far as its text shows it.
fn wrapped_commands(part: &str, mode: WordSplit) -> Vec<String> {
    let tokens = split_tokens(part, mode);
    let Some(first) = tokens.first() else {
        return Vec::new();
    };
    if first == "eval" {
        let evaluated = tokens[1..].join(" ");
        if evaluated.is_empty() {
            return Vec::new();
        }
        return analyze_text(&evaluated).parts;
    }
    if first != "exec" {
        return Vec::new();
    }
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        if token == "--" {
            index += 1;
            break;
        }
        if token == "-a" {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    if index >= tokens.len() {
        return Vec::new();
    }
    vec![tokens[index..].join(" ")]
}

/// Reference `_expand_guardrail_commands`: every part followed, breadth first,
/// by what it wraps, keeping repeated text because each occurrence may run in
/// another directory. The ancestry only stops a wrapper that wraps itself.
pub(super) fn expand_guardrail_commands(parts: &[String], mode: WordSplit) -> Vec<String> {
    let mut expanded = Vec::new();
    let mut pending = parts
        .iter()
        .map(|part| (part.clone(), BTreeSet::<String>::new()))
        .collect::<std::collections::VecDeque<_>>();
    while let Some((part, ancestors)) = pending.pop_front() {
        expanded.push(part.clone());
        if ancestors.contains(&part) {
            continue;
        }
        let mut next = ancestors;
        next.insert(part.clone());
        for wrapped in wrapped_commands(&part, mode) {
            pending.push_back((wrapped, next.clone()));
        }
    }
    expanded
}

/// Reference `_update_guardrail_cwds`: widens `cwds` to every directory a
/// `cd`, `pushd` or `Set-Location` can reach, and answers whether the
/// directory stopped being statically known.
fn update_guardrail_cwds(tokens: &[String], cwds: &mut BTreeSet<PathBuf>) -> bool {
    let Some(first) = tokens.first() else {
        return false;
    };
    let program = first
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if matches!(program.as_str(), "popd" | "pop-location") {
        // A plain pop only returns to a directory an earlier push recorded.
        return tokens.len() != 1;
    }
    if !matches!(
        program.as_str(),
        "cd" | "chdir" | "pushd" | "push-location" | "set-location" | "sl"
    ) {
        return false;
    }
    if matches!(program.as_str(), "pushd" | "push-location") && tokens.len() == 1 {
        return false;
    }
    let target = match tokens {
        [_, target] if !target.starts_with('-') && !target.contains(['*', '?', '[']) => target,
        _ => return true,
    };
    let reached = cwds
        .iter()
        .map(|cwd| resolve_tool_path(target, cwd))
        .collect::<Vec<_>>();
    cwds.extend(reached);
    false
}

/// Reference `resolve_tool_path`: `raw` made absolute against `cwd`, a
/// leading `~` expanded, and the result resolved.
fn resolve_tool_path(raw: &str, cwd: &Path) -> PathBuf {
    let raw = raw.trim();
    if raw.is_empty() {
        return cwd.to_path_buf();
    }
    // `_make_absolute` folds a Git Bash drive path (`/c/work`) first on
    // Windows, where the host would otherwise root it on the current drive.
    let raw = if cfg!(windows) {
        crate::workspace::tool_path::fold_msys_drive(raw)
    } else {
        raw.to_owned()
    };
    let expanded = PathBuf::from(expand_home(&raw));
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    if joined.is_absolute() {
        crate::platform::resolve_lenient(&joined)
    } else {
        joined
    }
}

/// Reference `_git_repository_permission_pattern`: the command keyed to every
/// repository it may read, so approving it in one repository never approves
/// it in another whose configuration runs a helper.
fn git_repository_pattern(command: &str, cwds: &BTreeSet<PathBuf>, cwd_is_unknown: bool) -> String {
    let mut identities = cwds
        .iter()
        .map(|cwd| {
            git_repository_identity(cwd).unwrap_or_else(|| {
                format!(
                    "directory:{}",
                    crate::platform::resolve_lenient(cwd).display()
                )
            })
        })
        .collect::<BTreeSet<_>>();
    if cwd_is_unknown {
        identities.insert("dynamic-directory".to_owned());
    }
    format!(
        "{command} [git repositories: {}]",
        identities.into_iter().collect::<Vec<_>>().join(" | ")
    )
}
