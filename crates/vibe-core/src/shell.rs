use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::{PathPolicyError, Platform, PolicyPath, parse_policy_path};
use crate::policy::{PermissionContext, PermissionMode, PermissionRequirement, PermissionScope};
use crate::scratchpad::is_scratchpad_path;
use crate::tools::config::ShellCommandConfig;

mod command_policy;
mod extract;
mod lexer;
mod repository;
mod windows;

#[cfg(test)]
mod shell_parity_tests;

use command_policy::{analyze_command_policy, has_option_guardrails, path_candidates};
pub use extract::{REDIRECT_MARKER, extract_commands};
use extract::{TextAnalysis, analyze_text};
use lexer::{WordSplit, split_tokens, whitespace_words};
pub(crate) use repository::resolve_path;
use repository::{git_repository_identity, git_repository_requires_approval};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellFlavor {
    Posix,
    GitBash,
    Cmd,
    PowerShell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellConfig {
    pub flavor: ShellFlavor,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
}

impl ShellConfig {
    #[must_use]
    pub fn default_for(platform: Platform) -> Self {
        match platform {
            Platform::Posix => Self {
                flavor: ShellFlavor::Posix,
                executable: PathBuf::from("/bin/sh"),
                arguments: vec!["-lc".to_owned()],
            },
            Platform::GitBash => Self {
                flavor: ShellFlavor::GitBash,
                executable: PathBuf::from("bash.exe"),
                arguments: vec!["-lc".to_owned()],
            },
            Platform::Windows => Self {
                flavor: ShellFlavor::PowerShell,
                executable: PathBuf::from("powershell.exe"),
                arguments: vec![
                    "-NoLogo".to_owned(),
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                ],
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPolicyContext {
    pub platform: Platform,
    pub working_directory: PolicyPath,
    pub roots: Vec<PolicyPath>,
    /// The session's own scratchpad, whose paths never raise a requirement.
    ///
    /// Reference `_collect_outside_dirs` consults `is_scratchpad_path` before
    /// it collects a directory, so the runtime's own capability is not
    /// something the operator is asked about. [`None`] for a session whose
    /// scratchpad could not be opened.
    pub scratchpad: Option<PathBuf>,
    /// Which of the reference's two POSIX resolvers answers.
    pub resolver: ShellResolver,
    /// What the call's overrides require beside its command, appended after
    /// everything the command earned.
    ///
    /// Reference `_build_context_permissions`: a custom shell and a custom
    /// environment each carry one, and either keeps an allowlisted command from
    /// being granted without a prompt.
    pub context_requirements: Vec<PermissionRequirement>,
    /// The call's environment overrides, which a PowerShell path may expand.
    pub environment: Vec<(String, String)>,
}

/// The reference resolver a shell tool answers through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellResolver {
    /// Reference `Bash.resolve_permission` (`vibe/core/tools/builtins/bash.py`),
    /// the non-managed `bash` tool: operands resolve against the session
    /// directory, the denylist matches the command as written, and a host
    /// whose shell is not POSIX defers to the configured permission.
    #[default]
    Legacy,
    /// Reference `_resolve_posix_shell_permission`
    /// (`vibe/core/tools/builtins/experimental_bash.py`), behind managed `bash`
    /// and both `git_bash` variants: operands resolve against the call's `cwd`,
    /// which is itself an outside directory when it leaves the workspace, the
    /// denylist also matches the program's basename, and the context
    /// requirements follow what the command earned.
    Managed,
}

impl ShellPolicyContext {
    /// A context whose working directory is its only root and which names no
    /// scratchpad.
    #[must_use]
    pub fn new(platform: Platform, working_directory: PolicyPath) -> Self {
        Self {
            platform,
            roots: vec![working_directory.clone()],
            working_directory,
            scratchpad: None,
            resolver: ShellResolver::Legacy,
            context_requirements: Vec::new(),
            environment: Vec::new(),
        }
    }

    /// The same context with the call's environment overrides.
    #[must_use]
    pub fn with_environment(mut self, environment: Vec<(String, String)>) -> Self {
        self.environment = environment;
        self
    }

    /// The same context answered by the managed resolver, from the call's
    /// `cwd` and with `context_requirements` appended.
    ///
    /// Reference `resolve_tool_path(cwd, self.cwd)`: a relative `cwd` resolves
    /// against the session directory. The roots stay what they were, so the
    /// `cwd` is positioned against the workspace rather than against itself. A
    /// `cwd` the policy cannot position is asked about as written.
    #[must_use]
    pub fn managed(
        mut self,
        flavor: ShellFlavor,
        cwd: Option<&str>,
        mut context_requirements: Vec<PermissionRequirement>,
    ) -> Self {
        if let Some(raw) = cwd.map(str::trim).filter(|raw| !raw.is_empty()) {
            let expanded = expand_home(raw);
            match normalize_operand(operand_platform(flavor), &self, &expanded) {
                Ok(directory) => self.working_directory = directory,
                Err(_) => context_requirements.insert(
                    0,
                    PermissionRequirement::outside_directory(&join_glob(
                        &expanded,
                        separator_for(operand_platform(flavor)),
                    )),
                ),
            }
        }
        self.resolver = ShellResolver::Managed;
        self.context_requirements = context_requirements;
        self
    }

    /// The same context whose file tools may always reach `scratchpad`.
    #[must_use]
    pub fn with_scratchpad(mut self, scratchpad: Option<PathBuf>) -> Self {
        self.scratchpad = scratchpad;
        self
    }

    /// The same context whose operands may also reach `roots`.
    ///
    /// Reference `_collect_outside_dirs` positions an operand against
    /// `Workspace.authorized_roots`, which holds every `--add-dir` root beside
    /// the working directory.
    #[must_use]
    pub fn with_roots(mut self, roots: impl IntoIterator<Item = PolicyPath>) -> Self {
        for root in roots {
            if !self.roots.contains(&root) {
                self.roots.push(root);
            }
        }
        self
    }
}

/// The four lists a shell tool resolves from its configuration.
///
/// Reference `BashTool` matches each extracted segment against them in this
/// order: `denylist` and `denylist_standalone` refuse outright,
/// `sensitive_patterns` keeps a segment out of the automatic grant, and
/// `allowlist` grants it. The grant is conditional on the segment's path
/// operands staying inside the working directory, which is why the lists are
/// resolved here, next to the operand walk, rather than in the permission
/// store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommandLists {
    /// The configured `tools.<name>.permission` the lists are consulted under.
    ///
    /// Reference `_is_unconditionally_allowed` grants every non-sensitive
    /// command when it is `ALWAYS`, so the operator's own setting decides
    /// before the allowlist does.
    pub permission: PermissionMode,
    pub allowlist: Vec<String>,
    pub denylist: Vec<String>,
    pub denylist_standalone: Vec<String>,
    pub sensitive_patterns: Vec<String>,
}

impl Default for ShellCommandLists {
    /// Four empty lists under the permission the reference declares for every
    /// shell family, so an operator who empties the lists is asked rather than
    /// granted.
    fn default() -> Self {
        Self {
            permission: PermissionMode::Ask,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            denylist_standalone: Vec::new(),
            sensitive_patterns: Vec::new(),
        }
    }
}

impl ShellCommandLists {
    /// The four lists a resolved shell configuration carries.
    #[must_use]
    pub fn from_config(config: &ShellCommandConfig) -> Self {
        Self {
            permission: config.shared.permission,
            allowlist: config.shared.allowlist.clone(),
            denylist: config.shared.denylist.clone(),
            denylist_standalone: config.denylist_standalone.clone(),
            sensitive_patterns: config.shared.sensitive_patterns.clone(),
        }
    }

    /// Reference `_matches_pattern`: a segment matches a pattern when it is the
    /// pattern, or when the pattern is its first words.
    fn matches(pattern: &str, segment: &str) -> bool {
        segment == pattern || segment.starts_with(&format!("{pattern} "))
    }

    fn denied(&self, segment: &str) -> Option<&str> {
        self.denylist
            .iter()
            .find(|pattern| Self::matches(pattern, segment))
            .map(String::as_str)
    }

    /// Reference `_matches_command_or_basename`, the managed resolver's
    /// denylist: the segment as written, or with its program reduced to its
    /// basename, so `/usr/bin/vim` is refused as `vim` is.
    fn denied_by_basename(&self, segment: &str) -> Option<&str> {
        let words = whitespace_words(segment);
        let normalized = words.split_first().map(|(program, rest)| {
            std::iter::once(host_basename(program))
                .chain(rest.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        });
        self.denylist
            .iter()
            .find(|pattern| {
                Self::matches(pattern, segment)
                    || normalized
                        .as_deref()
                        .is_some_and(|normalized| Self::matches(pattern, normalized))
            })
            .map(String::as_str)
    }

    /// Reference `_is_standalone_denylisted`: only a single-word segment is
    /// refused, by its whole text or by its basename, so `python3 script.py`
    /// runs where a bare `python3` does not.
    fn denied_standalone(&self, segment: &str) -> Option<&str> {
        let mut words = segment.split_whitespace();
        let first = words.next()?;
        if words.next().is_some() {
            return None;
        }
        let basename = host_basename(first);
        self.denylist_standalone
            .iter()
            .find(|entry| entry.as_str() == first || entry.as_str() == basename)
            .map(String::as_str)
    }

    /// Reference `_is_sensitive`: the first word of the segment, matched
    /// exactly.
    pub fn sensitive(&self, segment: &str) -> Option<&str> {
        let first = segment.split_whitespace().next()?;
        self.sensitive_patterns
            .iter()
            .find(|entry| entry.as_str() == first)
            .map(String::as_str)
    }

    fn allowed(&self, segment: &str) -> Option<&str> {
        self.allowlist
            .iter()
            .find(|pattern| Self::matches(pattern, segment))
            .map(String::as_str)
    }
}

/// What a custom shell and a custom environment require beside the command.
///
/// Reference `_build_context_permissions` and
/// `_build_git_bash_context_permissions`: the shell override carries itself
/// verbatim as both patterns, so approving one interpreter never approves
/// another, and the environment override is widened for the session because
/// the names change per call. Neither is literal, so a stored grant still
/// reads them as globs.
#[must_use]
pub fn override_requirements(
    shell: Option<&str>,
    environment: &[String],
) -> Vec<PermissionRequirement> {
    let mut requirements = Vec::new();
    if let Some(shell) = shell.filter(|shell| !shell.is_empty()) {
        let pattern = format!("shell override: {shell}");
        requirements.push(PermissionRequirement {
            scope: PermissionScope::CommandPattern,
            invocation_pattern: pattern.clone(),
            session_pattern: pattern,
            label: format!("custom shell ({shell})"),
            literal: false,
        });
    }
    if !environment.is_empty() {
        let mut names = environment.to_vec();
        names.sort();
        let names = names.join(", ");
        requirements.push(PermissionRequirement {
            scope: PermissionScope::CommandPattern,
            invocation_pattern: format!("env override: {names}"),
            session_pattern: "env override *".to_owned(),
            label: format!("custom environment ({names})"),
            literal: false,
        });
    }
    requirements
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellCommandNode {
    pub program: String,
    pub arguments: Vec<String>,
}

/// What resolving a command's policy answers.
///
/// Reference `BashTool.resolve_permission` returns a `PermissionContext`
/// carrying a permission and the requirements the operator is asked about. This
/// carries the same two, plus the segments and operands the decision was made
/// from, which is what lets a caller name the offending part in a refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellAnalysis {
    pub mode: PermissionMode,
    pub rationale: Vec<String>,
    pub commands: Vec<ShellCommandNode>,
    pub path_operands: Vec<String>,
    /// What an `Ask` decision asks for, already deduplicated.
    ///
    /// Empty for an `Always` or a `Never` decision, and empty for an `Ask` that
    /// only falls back to the configured permission, which is the reference
    /// answering `None`.
    #[serde(default)]
    pub requirements: Vec<PermissionRequirement>,
}

/// The `find` predicates that make a walk run a program or write a file.
///
/// Reference `_find_policy` in
/// `vibe/core/tools/builtins/_shell_command_policy.py`: the side-effecting
/// actions plus the `-files0-from` option. No allowlist entry covers a segment
/// carrying one, because `find` on the allowlist grants a walk and not what the
/// walk runs, deletes, or writes.
const FIND_EXECUTION_PREDICATES: [&str; 10] = [
    "-delete",
    "-exec",
    "-execdir",
    "-files0-from",
    "-fls",
    "-fprint",
    "-fprint0",
    "-fprintf",
    "-ok",
    "-okdir",
];

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
fn inspects_paths(program: &str) -> bool {
    MUTATING_PATH_COMMANDS.contains(&program)
        || crate::tools::config::shell_read_only_commands(true).contains(&program)
}

/// Resolves the policy for `command` against the four configured lists.
///
/// The two POSIX flavors answer through the reference resolver `context`
/// names, in its order: the grammar reads the text, the guardrails and the
/// denylists run over every command a wrapper exposes, the operands that leave
/// the workspace are collected, an unconditionally allowed command runs, and
/// everything else becomes the requirements the operator answers. A `Cmd`
/// host is not a POSIX shell, where the reference `bash` tool resolves nothing
/// and the configured permission applies.
///
/// Nothing is denied outside the two denylists. A command this port cannot
/// classify reaches an approval prompt, which is the direction the guard is
/// meant to fail in.
pub fn analyze_shell(
    flavor: ShellFlavor,
    command: &str,
    context: &ShellPolicyContext,
    lists: &ShellCommandLists,
) -> ShellAnalysis {
    match flavor {
        ShellFlavor::Posix | ShellFlavor::GitBash => analyze_posix(flavor, command, context, lists),
        ShellFlavor::Cmd => deferred(
            lists,
            vec!["a Windows command shell resolves no policy of its own".to_owned()],
            Vec::new(),
            Vec::new(),
        ),
        ShellFlavor::PowerShell => windows::analyze_powershell(command, context, lists),
    }
}

/// The analysis that resolves nothing, which the reference answers with
/// `None`: the configured permission applies and nothing is asked about.
fn deferred(
    lists: &ShellCommandLists,
    rationale: Vec<String>,
    commands: Vec<ShellCommandNode>,
    path_operands: Vec<String>,
) -> ShellAnalysis {
    ShellAnalysis {
        mode: lists.permission,
        rationale,
        commands,
        path_operands,
        requirements: Vec::new(),
    }
}

/// Reference `Bash.resolve_permission` and `_resolve_posix_shell_permission`.
fn analyze_posix(
    flavor: ShellFlavor,
    command: &str,
    context: &ShellPolicyContext,
    lists: &ShellCommandLists,
) -> ShellAnalysis {
    let managed = context.resolver == ShellResolver::Managed;
    let text = analyze_text(command);
    let parts = &text.parts;
    if parts.is_empty() && !text.requires_approval() {
        return deferred(
            lists,
            vec!["no command was extracted; the configured permission applies".to_owned()],
            Vec::new(),
            Vec::new(),
        );
    }
    let commands = parts
        .iter()
        .map(|part| command_node(part))
        .collect::<Vec<_>>();

    // 1. The guardrails, which also carry the only two refusals.
    let dialect = if managed {
        GuardrailDialect::Managed
    } else {
        GuardrailDialect::Legacy
    };
    let guardrails = match guardrail_requirements(parts, context, lists, dialect) {
        Ok(guardrails) => guardrails,
        Err(reason) => return refusal(reason),
    };

    // 2. The operands that leave the workspace.
    let (outside, operands) = collect_outside_directories(flavor, parts, context);

    // 3. The grant, when nothing withheld it.
    let mut rationale = text
        .reasons
        .iter()
        .map(|reason| format!("the text holds {reason}"))
        .collect::<Vec<_>>();
    let sensitive = parts.iter().any(|part| lists.sensitive(part).is_some());
    let unconditional = !sensitive
        && !(managed && !context.context_requirements.is_empty())
        && (lists.permission == PermissionMode::Always
            || (parts.iter().all(|part| lists.allowed(part).is_some()) && outside.is_empty()));
    if unconditional && guardrails.is_empty() && !text.requires_approval() {
        rationale.push("every command is allowed and stays inside the workspace".to_owned());
        return ShellAnalysis {
            mode: PermissionMode::Always,
            rationale,
            commands,
            path_operands: operands,
            requirements: Vec::new(),
        };
    }

    // 4. What is left is what the operator answers. Once the words stop
    // describing what runs, no part is offered; where they still do, the
    // approval owed for unreadable syntax sits on the commands that carried it,
    // allowlisted ones included.
    let (scoped, include_allowlisted) = if text.invalidates_scope {
        (&[][..], false)
    } else {
        (parts.as_slice(), text.requires_approval())
    };
    let mut requirements = command_requirements(
        scoped,
        &outside,
        include_allowlisted,
        |part| lists.sensitive(part),
        |part| lists.allowed(part).is_some(),
        &mut rationale,
    );
    for guardrail in &guardrails {
        rationale.push(format!(
            "`{}` carries an option or a repository setting that needs approval",
            guardrail.label
        ));
    }
    requirements.extend(guardrails);
    if needs_exact_command_scope(&text, &requirements) {
        rationale.push("only the text as written can be approved".to_owned());
        requirements.push(PermissionRequirement {
            label: text.approval_label(),
            ..PermissionRequirement::exact_command(command)
        });
    }
    if managed {
        requirements.extend(context.context_requirements.iter().cloned());
    }
    if requirements.is_empty() {
        return deferred(lists, rationale, commands, operands);
    }
    ShellAnalysis {
        mode: PermissionMode::Ask,
        rationale,
        commands,
        path_operands: operands,
        requirements,
    }
}

/// The programs whose session input can drive a pager, which can run a shell
/// command from its own prompt.
const PAGER_SESSION_COMMANDS: [&str; 3] = ["git", "less", "more"];

/// Whether the session running `command` may be a pager, so input written to
/// it needs approval.
///
/// Reference `BashStdin.resolve_permission`: every command the text runs,
/// wrappers expanded, is reduced to its lowercased basename without an `.exe`
/// suffix. A Git Bash session keeps backslashes literal when it splits.
#[must_use]
pub fn session_runs_pager(flavor: ShellFlavor, command: &str) -> bool {
    let mode = match flavor {
        ShellFlavor::Posix => WordSplit::Posix,
        ShellFlavor::GitBash => WordSplit::LiteralBackslash,
        ShellFlavor::Cmd | ShellFlavor::PowerShell => {
            return windows::runs_pager(command, &PAGER_SESSION_COMMANDS);
        }
    };
    let parts = expand_guardrail_commands(&analyze_text(command).parts, host_word_split());
    parts.iter().any(|part| {
        split_tokens(part, mode).first().is_some_and(|program| {
            let name = host_basename(program).to_lowercase();
            let name = name.strip_suffix(".exe").unwrap_or(&name);
            PAGER_SESSION_COMMANDS.contains(&name)
        })
    })
}

/// What input to a session needs, given the command the session runs, or
/// `None` for a session the family does not know.
///
/// Reference `BashStdin.resolve_permission`: a session that may be a pager, or
/// an unknown one, is asked about under a pattern naming the session; anything
/// else falls to the configured permission.
#[must_use]
pub fn pager_input_permission(
    flavor: ShellFlavor,
    session_id: &str,
    command: Option<&str>,
) -> PermissionContext {
    if command.is_some_and(|command| !session_runs_pager(flavor, command)) {
        return PermissionContext::deferred();
    }
    let label = format!("input to pager session {session_id}");
    PermissionContext::asking(vec![PermissionRequirement {
        scope: PermissionScope::CommandPattern,
        invocation_pattern: label.clone(),
        session_pattern: label.clone(),
        label,
        literal: false,
    }])
}

/// Reference `needs_exact_command_scope`: the text as written is the only
/// scope left when the words stopped describing what runs, or when syntax
/// needs approval and no command pattern came out of the parts.
fn needs_exact_command_scope(text: &TextAnalysis, requirements: &[PermissionRequirement]) -> bool {
    text.invalidates_scope
        || (text.requires_approval()
            && !requirements
                .iter()
                .any(|requirement| requirement.scope == PermissionScope::CommandPattern))
}

/// Reference `_build_required_permissions`: one requirement per session
/// pattern the parts earn, then one per directory the call leaves the
/// workspace for.
fn command_requirements<'lists>(
    parts: &[String],
    outside: &[String],
    include_allowlisted: bool,
    sensitive_pattern: impl Fn(&str) -> Option<&'lists str>,
    allowed: impl Fn(&str) -> bool,
    rationale: &mut Vec<String>,
) -> Vec<PermissionRequirement> {
    let mut requirements = Vec::new();
    let mut seen_session = BTreeSet::new();
    for part in parts {
        let tokens = whitespace_words(part);
        if tokens.is_empty() {
            continue;
        }
        let sensitive = sensitive_pattern(part);
        if sensitive.is_none() && !include_allowlisted && allowed(part) {
            continue;
        }
        // A sensitive part carries itself as its own session pattern, so
        // approving `sudo apt update` never approves `sudo rm`.
        if let Some(pattern) = sensitive {
            rationale.push(format!(
                "`{part}` matches the sensitive pattern `{pattern}`"
            ));
            requirements.push(PermissionRequirement::exact_command(part));
            continue;
        }
        let requirement = command_session_requirement(part, &tokens);
        if seen_session.insert(requirement.session_pattern.clone()) {
            requirements.push(requirement);
        }
    }
    for glob in outside {
        rationale.push(format!("`{glob}` is outside the workspace roots"));
        requirements.push(PermissionRequirement::outside_directory(glob));
    }
    requirements
}

/// Reference `command_session_pattern`: a guardrailed program keeps its own
/// text, read literally, because a trailing `*` would cover the option its
/// guardrail asks about; any other takes its arity pattern.
fn command_session_requirement(part: &str, tokens: &[String]) -> PermissionRequirement {
    if has_option_guardrails(tokens) {
        let literal = tokens.join(" ");
        return PermissionRequirement {
            invocation_pattern: part.to_owned(),
            ..PermissionRequirement::exact_command(&literal)
        };
    }
    PermissionRequirement::command(part)
}

/// The requirements the per-program guardrails raise, or the refusal a
/// denylist answers with.
///
/// Reference `_resolve_guardrail_permission`: every command a wrapper exposes
/// is checked, the directory each one runs in is tracked through `cd`,
/// `pushd` and `popd`, and a git reader is keyed to every repository it may
/// inspect. Requirements are keyed by the command text, the first occurrence
/// deciding the position and the last the value.
fn guardrail_requirements(
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
enum GuardrailDialect {
    /// The command as written.
    Legacy,
    /// The command as written, or with its program reduced to its basename.
    Managed,
    /// Every PowerShell form of the command: basename, suffix and alias.
    Windows,
}

/// How the guardrails split a command into tokens on this host: the reference
/// keeps a backslash literal on Windows.
fn host_word_split() -> WordSplit {
    if cfg!(windows) {
        WordSplit::LiteralBackslash
    } else {
        WordSplit::Posix
    }
}

/// The last component of `program` under the host's separators, Python's
/// `os.path.basename`.
fn host_basename(program: &str) -> &str {
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
fn expand_guardrail_commands(parts: &[String], mode: WordSplit) -> Vec<String> {
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
    let expanded = PathBuf::from(expand_home(raw));
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    if joined.is_absolute() {
        resolve_path(&joined)
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
            git_repository_identity(cwd)
                .unwrap_or_else(|| format!("directory:{}", resolve_path(cwd).display()))
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

fn refusal(reason: String) -> ShellAnalysis {
    ShellAnalysis {
        mode: PermissionMode::Never,
        rationale: vec![reason],
        commands: Vec::new(),
        path_operands: Vec::new(),
        requirements: Vec::new(),
    }
}

fn command_node(segment: &str) -> ShellCommandNode {
    let mut words = segment.split(' ').filter(|word| !word.is_empty());
    let program = words.next().unwrap_or_default().to_owned();
    ShellCommandNode {
        program,
        arguments: words.map(ToOwned::to_owned).collect(),
    }
}

/// The directories a call reaches outside every root, as the globs a
/// requirement names them by, and the operands they came from.
///
/// Reference `_collect_outside_dirs`: only a path-inspecting command's operands
/// are considered, a flag and a `chmod` mode are skipped, a token that does not
/// look like a path is skipped, the scratchpad is skipped, and a file
/// contributes its parent directory while a directory contributes itself. The
/// globs are sorted and deduplicated, so two operands in one directory are one
/// approval.
fn collect_outside_directories(
    flavor: ShellFlavor,
    segments: &[String],
    context: &ShellPolicyContext,
) -> (Vec<String>, Vec<String>) {
    let mut globs = BTreeSet::new();
    let mut operands = Vec::new();
    // The managed resolver positions the call's own directory too, and names
    // it by itself rather than by its parent.
    if context.resolver == ShellResolver::Managed
        && let Some(glob) = escaping_directory_glob(context, &context.working_directory)
    {
        globs.insert(glob);
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
            if !looks_like_path(&token) {
                continue;
            }
            if let Some(glob) = escaping_glob(flavor, context, &token) {
                globs.insert(glob);
            }
            operands.push(token);
        }
    }
    (globs.into_iter().collect(), operands)
}

/// Reference `_collect_outside_dirs`: only a token shaped like a path is
/// resolved, so a `grep` pattern and a `chmod` mode never become directories.
fn looks_like_path(token: &str) -> bool {
    token.starts_with('/')
        || token.starts_with('~')
        || token.starts_with('.')
        || token.contains('/')
        || token.contains('\\')
}

/// The glob naming where `token` reaches, or [`None`] when it stays inside.
///
/// The operand is parsed under the interpreter's own path grammar and then
/// positioned on the host's, which is what lets a Git Bash `/c/work/notes.txt`
/// land on the Windows workspace root it names.
/// The path grammar an operand of `flavor` is written in.
fn operand_platform(flavor: ShellFlavor) -> Platform {
    match flavor {
        ShellFlavor::Posix => Platform::Posix,
        ShellFlavor::GitBash => Platform::GitBash,
        ShellFlavor::Cmd | ShellFlavor::PowerShell => Platform::Windows,
    }
}

fn escaping_glob(flavor: ShellFlavor, context: &ShellPolicyContext, token: &str) -> Option<String> {
    let platform = operand_platform(flavor);
    let expanded = expand_home(token);
    let Ok(path) = normalize_operand(platform, context, &expanded) else {
        // A path the policy cannot position is treated as outside rather than
        // as inside, which is the direction the reference resolves an
        // unresolvable operand in.
        return Some(join_glob(&expanded, separator_for(platform)));
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
    if inside {
        return None;
    }
    // A directory names itself; a file names the directory holding it, which is
    // what makes one approval cover a sibling read.
    let directory = match host.as_deref() {
        Some(host) if host.is_dir() => path.clone(),
        Some(_) | None => parent_of(&path),
    };
    Some(join_glob(
        &render_policy_path(&directory),
        separator_for(directory.platform),
    ))
}

/// The glob naming a managed call's own directory when it leaves the
/// workspace, which the reference collects as the directory itself.
fn escaping_directory_glob(context: &ShellPolicyContext, directory: &PolicyPath) -> Option<String> {
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
    (!inside).then(|| {
        join_glob(
            &render_policy_path(directory),
            separator_for(directory.platform),
        )
    })
}

/// The separator `platform` writes a path with.
fn separator_for(platform: Platform) -> char {
    if platform == Platform::Windows {
        '\\'
    } else {
        '/'
    }
}

/// `directory` joined with `*`, which is how the reference names the class of
/// paths one approval covers.
fn join_glob(directory: &str, separator: char) -> String {
    if directory.ends_with(separator) {
        format!("{directory}*")
    } else {
        format!("{directory}{separator}*")
    }
}

/// `path` without its last component, or `path` when it has none.
fn parent_of(path: &PolicyPath) -> PolicyPath {
    let mut parent = path.clone();
    parent.components.pop();
    parent
}

/// The text a requirement names `path` by, in the separator its platform uses.
fn render_policy_path(path: &PolicyPath) -> String {
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
/// Reference `_collect_outside_dirs` calls `Path.expanduser` before resolving,
/// without which `cat ~/.ssh/id_rsa` would position under a literal `~`
/// directory inside the workspace and never raise a requirement.
fn expand_home(token: &str) -> String {
    if token != "~" && !token.starts_with("~/") {
        return token.to_owned();
    }
    let Some(home) = crate::config::user_home_directory() else {
        return token.to_owned();
    };
    let home = home.to_string_lossy().into_owned();
    match token.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None => home,
    }
}

fn normalize_operand(
    platform: Platform,
    context: &ShellPolicyContext,
    operand: &str,
) -> Result<PolicyPath, PathPolicyError> {
    match parse_policy_path(platform, operand) {
        Ok(mut absolute) if !absolute.root.is_empty() => {
            absolute.platform = context.platform;
            Ok(absolute)
        }
        Ok(relative) => {
            let mut combined = context.working_directory.clone();
            combined.components.extend(relative.components);
            Ok(combined)
        }
        Err(PathPolicyError::ParentTraversal) => fold_traversal(platform, context, operand),
        Err(error) => Err(error),
    }
}

/// `operand` positioned on the working directory with its `..` components
/// folded away.
///
/// Reference `_collect_outside_dirs` calls `Path.resolve()`, which folds a
/// traversal instead of refusing it, so `cat sub/../notes.txt` is measured where
/// it actually reads. [`parse_policy_path`] refuses one, because the file tools
/// it also serves must never let a `..` cross a root; the folding therefore
/// happens here, on a shell operand only. Without it a traversal is treated as
/// unresolvable, which both asks about a path that never left the workspace and
/// names it by a glob built on the file rather than on the directory holding it.
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
        match parse_policy_path(platform, head) {
            Ok(mut absolute) if !absolute.root.is_empty() => {
                absolute.platform = context.platform;
                absolute
            }
            Ok(relative) => {
                let mut combined = context.working_directory.clone();
                combined.components.extend(relative.components);
                combined
            }
            Err(error) => return Err(error),
        }
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
    std::fs::canonicalize(&working_directory).ok()?;
    let candidate = policy_path_to_host(path)?;
    let canonical = match std::fs::canonicalize(candidate) {
        Ok(canonical) => canonical,
        Err(_) => return Some(false),
    };
    Some(context.roots.iter().any(|root| {
        policy_path_to_host(root)
            .and_then(|root| std::fs::canonicalize(root).ok())
            .is_some_and(|root| canonical.starts_with(root))
    }))
}

fn policy_path_to_host(path: &PolicyPath) -> Option<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PermissionScope;

    /// The lists a POSIX host resolves for `bash`, which is what every session
    /// analyzes with.
    fn posix_lists() -> ShellCommandLists {
        ShellCommandLists::from_config(
            &crate::tools::config::ToolConfigResolver::new()
                .with_posix_shell(true)
                .view("bash"),
        )
    }

    fn posix_context() -> ShellPolicyContext {
        ShellPolicyContext::new(
            Platform::Posix,
            parse_policy_path(Platform::Posix, "/work/project").expect("working directory"),
        )
    }

    fn analyze(command: &str) -> ShellAnalysis {
        analyze_shell(
            ShellFlavor::Posix,
            command,
            &posix_context(),
            &posix_lists(),
        )
    }

    /// The session patterns an analysis asks about, in order.
    fn session_patterns(analysis: &ShellAnalysis) -> Vec<String> {
        analysis
            .requirements
            .iter()
            .map(|requirement| requirement.session_pattern.clone())
            .collect()
    }

    // ----------------------------------------------------------------------
    // US-109: the grammar decides what runs
    // ----------------------------------------------------------------------

    /// US-109: a heredoc is a redirect, so the command under it is not the bare
    /// standalone interpreter the denylist refuses. Its body is what runs, so
    /// only the text as written can be approved.
    #[test]
    fn a_heredoc_is_not_a_bare_standalone_interpreter() {
        let command = "python3 <<'EOF'\nprint(1)\nEOF";
        let analysis = analyze(command);
        assert_ne!(
            analysis.mode,
            PermissionMode::Never,
            "a heredoc body is not a standalone `python3`: {:?}",
            analysis.rationale
        );
        assert_eq!(session_patterns(&analysis), vec![command.to_owned()]);
        assert!(analysis.requirements[0].literal);
    }

    /// US-109: each segment of a chain is analyzed on its own, so approving the
    /// reader never approves what follows it.
    #[test]
    fn a_chain_is_analyzed_one_segment_at_a_time() {
        let analysis = analyze("cat README.md && rm -rf build");
        assert_eq!(analysis.commands.len(), 2);
        // `cat` is allowlisted and drops out; only `rm` is asked about.
        assert_eq!(session_patterns(&analysis), vec!["rm *".to_owned()]);
    }

    /// US-109: text the grammar finds no command in defers to the configured
    /// permission rather than being granted.
    #[test]
    fn text_without_a_command_falls_back_to_the_configured_permission() {
        let analysis = analyze("");
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(analysis.requirements.is_empty());
        assert!(analysis.commands.is_empty());
    }

    // ----------------------------------------------------------------------
    // US-110: the four lists, and nothing else, decide
    // ----------------------------------------------------------------------

    /// US-110: the denylist refuses, naming both the segment and the pattern.
    #[test]
    fn the_denylist_refuses_and_names_what_matched() {
        for (command, pattern) in [
            ("vim notes.txt", "vim"),
            ("nano", "nano"),
            ("tmux", "tmux"),
            ("gdb ./binary", "gdb"),
            ("passwd", "passwd"),
            ("bash -i", "bash -i"),
        ] {
            let analysis = analyze(command);
            assert_eq!(
                analysis.mode,
                PermissionMode::Never,
                "`{command}` is on the reference denylist: {:?}",
                analysis.rationale
            );
            let reason = analysis.rationale.join("; ");
            assert!(
                reason.contains(pattern),
                "`{command}` refused without naming `{pattern}`: {reason}"
            );
        }
    }

    /// US-110: the standalone denylist refuses the bare interpreter by its whole
    /// text or its basename, and refuses nothing that carries an argument.
    #[test]
    fn the_standalone_denylist_refuses_only_a_single_word() {
        for command in ["python3", "/usr/bin/python3", "su", "sh"] {
            assert_eq!(
                analyze(command).mode,
                PermissionMode::Never,
                "`{command}` is a bare standalone interpreter"
            );
        }
        for command in ["python3 script.py", "sh build.sh"] {
            assert_ne!(
                analyze(command).mode,
                PermissionMode::Never,
                "`{command}` carries an argument, so the standalone rule misses it"
            );
        }
    }

    /// US-110: the five commands this port used to refuse outright now reach an
    /// approval prompt, because no denylist entry matches any of them.
    ///
    /// One named case per loosening, which is what makes the change visible in
    /// the suite rather than buried in a list diff.
    #[test]
    fn rm_now_reaches_an_approval_prompt() {
        let analysis = analyze("rm -rf build/");
        assert_eq!(
            analysis.mode,
            PermissionMode::Ask,
            "{:?}",
            analysis.rationale
        );
        assert_eq!(session_patterns(&analysis), vec!["rm *".to_owned()]);
    }

    #[test]
    fn dd_now_reaches_an_approval_prompt() {
        let analysis = analyze("dd if=/dev/zero of=disk.img");
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(session_patterns(&analysis).contains(&"dd *".to_owned()));
    }

    #[test]
    fn mkfs_now_reaches_an_approval_prompt() {
        let analysis = analyze("mkfs.ext4 /dev/sdb1");
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(session_patterns(&analysis).contains(&"mkfs.ext4 *".to_owned()));
    }

    #[test]
    fn shutdown_now_reaches_an_approval_prompt() {
        let analysis = analyze("shutdown -h now");
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert_eq!(session_patterns(&analysis), vec!["shutdown *".to_owned()]);
    }

    #[test]
    fn eval_now_reaches_an_approval_prompt() {
        let analysis = analyze("eval echo hi");
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(session_patterns(&analysis).contains(&"eval *".to_owned()));
    }

    /// The sixth loosening this epic carries: `git reset --hard` is on no
    /// denylist, so it is asked about rather than refused.
    #[test]
    fn a_destructive_git_reset_now_reaches_an_approval_prompt() {
        for command in ["git reset --hard", "git reset --hard -- src"] {
            let analysis = analyze(command);
            assert_eq!(
                analysis.mode,
                PermissionMode::Ask,
                "`{command}`: {:?}",
                analysis.rationale
            );
            assert!(
                !analysis.requirements.is_empty(),
                "`{command}` must leave something to approve"
            );
        }
    }

    /// US-110: a sensitive first word always produces a requirement, is never
    /// covered by an allowlist match, and never widens past itself.
    #[test]
    fn a_sensitive_first_word_is_always_asked_about() {
        let analysis = analyze("sudo ls");
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert_eq!(session_patterns(&analysis), vec!["sudo ls".to_owned()]);
        assert!(
            analysis
                .rationale
                .iter()
                .any(|line| line.contains("sensitive pattern `sudo`")),
            "{:?}",
            analysis.rationale
        );

        // Even at permission `always`, which grants everything else outright.
        let mut lists = posix_lists();
        lists.permission = PermissionMode::Always;
        let granted = analyze_shell(ShellFlavor::Posix, "ls -la", &posix_context(), &lists);
        assert_eq!(granted.mode, PermissionMode::Always);
        let sensitive = analyze_shell(ShellFlavor::Posix, "sudo ls", &posix_context(), &lists);
        assert_eq!(sensitive.mode, PermissionMode::Ask);
    }

    /// US-110: `find` running a program is asked about under the whole segment,
    /// even though `find` is allowlisted. `{}` is syntax that needs approval,
    /// so the part is asked about beside the guardrail, and neither list
    /// deduplicates against the other, as upstream.
    #[test]
    fn find_running_a_program_is_asked_about_once_per_segment() {
        for predicate in ["-exec", "-execdir", "-ok", "-okdir"] {
            let analysis = analyze(&format!("find . {predicate} rm {{}} ;"));
            assert_eq!(
                analysis.mode,
                PermissionMode::Ask,
                "`find {predicate}` must ask: {:?}",
                analysis.rationale
            );
            let requirement = analysis
                .requirements
                .first()
                .expect("a find execution raises a requirement");
            assert_eq!(requirement.scope, PermissionScope::CommandPattern);
            assert!(
                requirement.invocation_pattern.contains(predicate),
                "the requirement carries the whole segment: {requirement:?}"
            );
            // The grant may not widen: the segment is its own session pattern.
            assert_eq!(requirement.invocation_pattern, requirement.session_pattern);
        }
        // Deleting or writing a file is gated the same way.
        for command in ["find . -delete", "find . -fprint out.txt"] {
            let analysis = analyze(command);
            assert_eq!(analysis.mode, PermissionMode::Ask, "`{command}` must ask");
            assert_eq!(
                analysis.requirements,
                vec![PermissionRequirement::exact_command(command)]
            );
        }
        // A plain walk stays allowlisted.
        assert_eq!(analyze("find . -name '*.rs'").mode, PermissionMode::Always);
        // The same segment twice is one approval per list.
        let repeated = analyze("find . -exec rm {} \\; && find . -exec rm {} \\;");
        assert_eq!(
            repeated.requirements.len(),
            2,
            "{:?}",
            repeated.requirements
        );
    }

    /// Text the grammar cannot parse, or that a line continuation rewrites
    /// before the shell reads it, is never granted: the extracted segments no
    /// longer describe what runs, so the text as written is the only scope.
    #[test]
    fn syntax_the_segments_do_not_describe_is_asked_about_as_written() {
        for command in ["cat 'unterminated", "cat file.txt \\\nsecret"] {
            let analysis = analyze(command);
            assert_eq!(analysis.mode, PermissionMode::Ask, "`{command}` must ask");
            let [requirement] = analysis.requirements.as_slice() else {
                panic!("one whole-command requirement: {:?}", analysis.requirements);
            };
            assert_eq!(requirement.invocation_pattern, command);
            assert_eq!(requirement.session_pattern, command);
        }
    }

    /// US-110: an operator who empties the allowlist is asked per segment rather
    /// than losing the tool.
    #[test]
    fn an_emptied_allowlist_asks_per_segment() {
        let lists = ShellCommandLists::default();
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "ls -la && cat README.md",
            &posix_context(),
            &lists,
        );
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert_eq!(
            session_patterns(&analysis),
            vec!["ls *".to_owned(), "cat *".to_owned()]
        );
        // An emptied denylist stops refusing what it used to refuse, which is
        // the operator's decision rather than a hardcoded one.
        assert_ne!(
            analyze_shell(
                ShellFlavor::Posix,
                "vim notes.txt",
                &posix_context(),
                &lists
            )
            .mode,
            PermissionMode::Never
        );
    }

    /// US-110: two segments reducing to the same session pattern are one
    /// approval, not two.
    #[test]
    fn segments_sharing_a_session_pattern_are_asked_once() {
        let analysis = analyze("cargo build && cargo build --release");
        assert_eq!(
            session_patterns(&analysis),
            vec!["cargo build *".to_owned()]
        );
        let distinct = analyze("cargo build && cargo test");
        assert_eq!(
            session_patterns(&distinct),
            vec!["cargo build *".to_owned(), "cargo test *".to_owned()]
        );
    }

    // ----------------------------------------------------------------------
    // US-111: the operands that leave the workspace
    // ----------------------------------------------------------------------

    /// US-111: an allowlisted reader pointed outside the roots asks, under an
    /// `outside_directory` requirement naming the parent directory.
    #[test]
    fn an_allowlisted_reader_pointed_outside_the_roots_still_asks() {
        let inside = analyze("wc -l /work/project/notes.txt");
        assert_eq!(
            inside.mode,
            PermissionMode::Always,
            "`wc` is on the reference read-only allowlist: {:?}",
            inside.rationale
        );

        let outside = analyze("grep secret /etc/passwd");
        assert_eq!(outside.mode, PermissionMode::Ask);
        let requirement = outside
            .requirements
            .iter()
            .find(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
            .expect("an escaping operand raises an outside-directory requirement");
        assert_eq!(requirement.invocation_pattern, "/etc/*");
        assert_eq!(requirement.session_pattern, "/etc/*");
        assert_eq!(requirement.label, "outside workdir (/etc/*)");
    }

    /// US-111: a flag and a `chmod` mode are not paths.
    #[test]
    fn flags_and_chmod_modes_are_not_resolved_as_paths() {
        let flagged = analyze("ls --color /work/project");
        assert_eq!(
            flagged.mode,
            PermissionMode::Always,
            "{:?}",
            flagged.rationale
        );
        let mode = analyze("chmod +x /work/project/script.sh");
        assert!(
            mode.requirements
                .iter()
                .all(|requirement| requirement.scope != PermissionScope::OutsideDirectory),
            "`+x` is a mode, not a path: {:?}",
            mode.requirements
        );
    }

    /// US-111: identical directories are emitted once, whichever operand
    /// reached them.
    #[test]
    fn one_directory_is_one_approval() {
        let analysis = analyze("cat /etc/passwd /etc/group && cat /etc/hosts");
        let outside = analysis
            .requirements
            .iter()
            .filter(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
            .collect::<Vec<_>>();
        assert_eq!(outside.len(), 1, "{outside:?}");
        assert_eq!(outside[0].invocation_pattern, "/etc/*");
    }

    /// US-111: a command that inspects no path never resolves its arguments,
    /// which is why `echo` naming a file outside is not an approval.
    #[test]
    fn a_command_outside_the_path_set_has_no_operands_resolved() {
        let analysis = analyze("echo /etc/passwd");
        assert_eq!(
            analysis.mode,
            PermissionMode::Always,
            "{:?}",
            analysis.rationale
        );
        assert!(analysis.path_operands.is_empty());
    }

    /// US-111: the inspected set stays a superset of the read-only allowlist, so
    /// no auto-allowed reader escapes the operand walk.
    #[test]
    fn every_read_only_allowlist_command_has_its_operands_inspected() {
        let uninspected = crate::tools::config::shell_read_only_commands(true)
            .iter()
            .filter(|program| !inspects_paths(program))
            .collect::<Vec<_>>();
        assert!(
            uninspected.is_empty(),
            "read-only commands whose operands are never inspected: {uninspected:?}"
        );
        // The reference set is the union with the eight mutating commands, and
        // nothing else: a command only the shared allowlist names is not
        // inspected, which is what keeps `echo /etc/passwd` an allowed echo.
        assert!(!inspects_paths("echo"));
        assert!(inspects_paths("rm"));
    }

    /// US-111: a path inside the scratchpad raises nothing, because it is the
    /// runtime's own capability rather than the operator's workspace.
    #[test]
    fn a_scratchpad_operand_raises_no_requirement() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scratchpad = tempfile::tempdir().expect("scratchpad");
        std::fs::write(scratchpad.path().join("note.txt"), "note").expect("note");
        let root = workspace.path().to_string_lossy().into_owned();
        let context = ShellPolicyContext::new(
            Platform::Posix,
            parse_policy_path(Platform::Posix, &root).expect("root"),
        )
        .with_scratchpad(Some(scratchpad.path().to_path_buf()));

        let note = scratchpad.path().join("note.txt");
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            &format!("cat {}", note.display()),
            &context,
            &posix_lists(),
        );
        assert_eq!(
            analysis.mode,
            PermissionMode::Always,
            "the scratchpad is granted before any list: {:?}",
            analysis.rationale
        );
    }

    /// US-111: a `~` operand is expanded before it is positioned, so a home-relative
    /// read is measured where it actually reads.
    #[test]
    fn a_home_relative_operand_is_expanded_before_it_is_positioned() {
        let Some(home) = crate::config::user_home_directory() else {
            eprintln!("skipping: the environment names no home directory");
            return;
        };
        let analysis = analyze("cat ~/.ssh/id_rsa");
        assert_eq!(
            analysis.mode,
            PermissionMode::Ask,
            "{:?}",
            analysis.rationale
        );
        let expected = format!(
            "{}/.ssh/*",
            home.display().to_string().trim_end_matches('/')
        );
        assert!(
            analysis
                .requirements
                .iter()
                .any(|requirement| requirement.invocation_pattern == expected),
            "expected `{expected}` among {:?}",
            analysis.requirements
        );
    }

    /// US-111: an operand that ascends is folded before it is positioned, so it
    /// is measured where it actually reads and named by the directory holding
    /// it rather than by itself.
    #[test]
    fn an_ascending_operand_is_folded_before_it_is_positioned() {
        let outside = analyze("cat ../elsewhere/secret.txt");
        assert_eq!(outside.mode, PermissionMode::Ask, "{:?}", outside.rationale);
        let requirement = outside
            .requirements
            .iter()
            .find(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
            .expect("an ascending operand leaves the workspace");
        assert_eq!(requirement.invocation_pattern, "/work/elsewhere/*");
        assert_eq!(requirement.session_pattern, "/work/elsewhere/*");

        // An ascent that lands back inside raises nothing, which is what keeps a
        // relative read of a sibling directory from asking.
        let inside = analyze("cat sub/../notes.txt");
        assert_eq!(
            inside.mode,
            PermissionMode::Always,
            "`sub/../notes.txt` never left the workspace: {:?}",
            inside.rationale
        );

        // An ascent past the root stops at it rather than failing to position.
        let root = analyze("cat ../../../../etc/passwd");
        assert!(
            root.requirements
                .iter()
                .any(|requirement| requirement.invocation_pattern == "/etc/*"),
            "{:?}",
            root.requirements
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_reader_operand_outside_root_requires_approval() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "secret").expect("outside file");
        symlink(outside.path(), root.path().join("outside")).expect("symlink");
        let root_text = root.path().to_string_lossy().into_owned();
        let context = ShellPolicyContext::new(
            Platform::Posix,
            parse_policy_path(Platform::Posix, &root_text).expect("root"),
        );

        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "cat outside/secret",
            &context,
            &posix_lists(),
        );

        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(
            analysis
                .requirements
                .iter()
                .any(|requirement| requirement.scope == PermissionScope::OutsideDirectory),
            "a symlink out of the workspace is still out of it: {:?}",
            analysis.requirements
        );
    }

    // ----------------------------------------------------------------------
    // The guards this port keeps beyond the reference set
    // ----------------------------------------------------------------------

    /// Each of them withholds the automatic grant and nothing more: the segment
    /// is asked about under its own pattern, never refused.
    #[test]
    fn the_extra_guards_ask_rather_than_refuse() {
        for command in [
            "cat $(credential-helper)",
            "cat README.md > /etc/motd",
            "git diff --no-index /etc/passwd /dev/null",
            "git -c core.pager=sh log",
            "rg --pre malicious-helper needle .",
        ] {
            let analysis = analyze(command);
            assert_eq!(
                analysis.mode,
                PermissionMode::Ask,
                "`{command}`: {:?}",
                analysis.rationale
            );
            assert!(
                !analysis.requirements.is_empty(),
                "`{command}` must leave something to approve"
            );
        }
    }

    #[test]
    fn windows_shells_handle_aliases_drives_unc_and_ambiguity() {
        let windows_lists = ShellCommandLists::from_config(
            &crate::tools::config::ToolConfigResolver::new()
                .with_posix_shell(false)
                .view("powershell"),
        );
        let context = ShellPolicyContext::new(
            Platform::Windows,
            parse_policy_path(Platform::Windows, r"C:\work\project").expect("cwd"),
        );
        let safe = analyze_shell(
            ShellFlavor::PowerShell,
            r"type C:\work\project\README.md",
            &context,
            &windows_lists,
        );
        assert_eq!(safe.mode, PermissionMode::Always, "{:?}", safe.rationale);
        // A `cmd.exe` host is not a POSIX shell, where the reference `bash`
        // tool resolves nothing and the configured permission applies.
        let deferred = analyze_shell(
            ShellFlavor::Cmd,
            r"type C:\work\project\README.md",
            &context,
            &windows_lists,
        );
        assert_eq!(deferred.mode, windows_lists.permission);
        assert!(deferred.requirements.is_empty());
        let unc = analyze_shell(
            ShellFlavor::PowerShell,
            r"type \\server\share\secret.txt",
            &context,
            &windows_lists,
        );
        assert_eq!(unc.mode, PermissionMode::Ask);
        let provider = analyze_shell(
            ShellFlavor::PowerShell,
            r"type Env:\SECRET",
            &context,
            &windows_lists,
        );
        assert_eq!(provider.mode, PermissionMode::Ask);
    }

    #[test]
    fn platform_default_preserves_executable_arguments_and_flavor() {
        assert_eq!(
            ShellConfig::default_for(Platform::Posix),
            ShellConfig {
                flavor: ShellFlavor::Posix,
                executable: PathBuf::from("/bin/sh"),
                arguments: vec!["-lc".to_owned()],
            }
        );
        assert_eq!(
            ShellConfig::default_for(Platform::Windows).flavor,
            ShellFlavor::PowerShell
        );
    }
}
