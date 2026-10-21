use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::{Platform, PolicyPath};
use crate::policy::{PermissionContext, PermissionMode, PermissionRequirement, PermissionScope};
use crate::tools::config::ShellCommandConfig;

mod command_policy;
mod extract;
mod guardrails;
mod lexer;
mod operands;
mod repository;
mod windows;

#[cfg(test)]
mod shell_parity_tests;
#[cfg(test)]
mod shell_tests;

use command_policy::has_option_guardrails;
pub use extract::{REDIRECT_MARKER, extract_commands};
use extract::{TextAnalysis, analyze_text};
use guardrails::{
    GuardrailDialect, expand_guardrail_commands, guardrail_requirements, host_basename,
    host_word_split,
};
use lexer::{WordSplit, split_tokens, whitespace_words};
use operands::{
    collect_outside_paths, expand_home, normalize_operand, operand_platform, retain_ungranted,
};

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
    /// Reference `_collect_outside_paths` consults `is_scratchpad_path` before
    /// it collects a path, so the runtime's own capability is not
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
    /// A managed call's `cwd` as written, when the policy could not position
    /// it, which is then asked about as an outside path.
    pub unpositioned_cwd: Option<String>,
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
            unpositioned_cwd: None,
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
        context_requirements: Vec<PermissionRequirement>,
    ) -> Self {
        if let Some(raw) = cwd.map(str::trim).filter(|raw| !raw.is_empty()) {
            let expanded = expand_home(raw);
            match normalize_operand(operand_platform(flavor), &self, &expanded) {
                Ok(directory) => self.working_directory = directory,
                Err(_) => self.unpositioned_cwd = Some(expanded),
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
    /// Reference `_collect_outside_paths` positions an operand against
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
            path_scope_root: None,
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
            path_scope_root: None,
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

    // 2. The operands that leave the workspace, less those the allowlist
    // already grants by path.
    let (mut outside, operands) = collect_outside_paths(flavor, parts, context);
    retain_ungranted(&mut outside, lists);

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
        path_scope_root: None,
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
/// pattern the parts earn, then one per path the call reaches outside the
/// workspace, in sorted order.
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
    for path in outside {
        rationale.push(format!("`{path}` is outside the workspace roots"));
        requirements.push(PermissionRequirement::outside_path(Path::new(path)));
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
