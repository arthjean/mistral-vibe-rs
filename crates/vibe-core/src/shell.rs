use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platform::{PathPolicyError, Platform, PolicyPath, parse_policy_path};
use crate::policy::{PermissionMode, PermissionRequirement};
use crate::scratchpad::is_scratchpad_path;
use crate::tools::config::ShellCommandConfig;

mod extract;

#[cfg(test)]
mod shell_parity_tests;

pub use extract::{REDIRECT_MARKER, extract_commands};

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
        }
    }

    /// The same context whose file tools may always reach `scratchpad`.
    #[must_use]
    pub fn with_scratchpad(mut self, scratchpad: Option<PathBuf>) -> Self {
        self.scratchpad = scratchpad;
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

    /// Reference `_is_standalone_denylisted`: only a single-word segment is
    /// refused, by its whole text or by its basename, so `python3 script.py`
    /// runs where a bare `python3` does not.
    fn denied_standalone(&self, segment: &str) -> Option<&str> {
        let mut words = segment.split_whitespace();
        let first = words.next()?;
        if words.next().is_some() {
            return None;
        }
        let basename = first.rsplit(['/', '\\']).next().unwrap_or(first);
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

/// The `find` predicates that make a read an execution.
///
/// Reference `_FIND_EXECUTION_PREDICATES`. No allowlist entry covers a segment
/// carrying one, because `find` on the allowlist grants a walk and not the
/// program the walk runs.
const FIND_EXECUTION_PREDICATES: [&str; 4] = ["-exec", "-execdir", "-ok", "-okdir"];

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
fn inspects_paths(flavor: ShellFlavor, program: &str) -> bool {
    MUTATING_PATH_COMMANDS.contains(&program)
        || crate::tools::config::shell_read_only_commands(matches!(
            flavor,
            ShellFlavor::Posix | ShellFlavor::GitBash
        ))
        .contains(&program)
}

/// Resolves the policy for `command` against the four configured lists.
///
/// Reference `BashTool.resolve_permission`, in its order: the grammar extracts
/// the segments, a denylist or standalone-denylist match refuses the whole
/// command, the operands that leave the workspace are collected, an
/// unconditionally allowed command runs, and everything else becomes the
/// requirements the operator answers.
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
    let segments = segments_of(flavor, command);
    if segments.is_empty() {
        // Reference `resolve_permission` returns `None` here, which defers to
        // the configured permission rather than deciding.
        return ShellAnalysis {
            mode: lists.permission,
            rationale: vec![
                "no command was extracted; the configured permission applies".to_owned(),
            ],
            commands: Vec::new(),
            path_operands: Vec::new(),
            requirements: Vec::new(),
        };
    }

    // 1. The two denylists, which are the only thing that refuses outright.
    for segment in &segments {
        if let Some(pattern) = lists.denied(segment) {
            return refusal(format!(
                "`{segment}` matches the denylist entry `{pattern}`"
            ));
        }
        if let Some(entry) = lists.denied_standalone(segment) {
            return refusal(format!("`{entry}` is refused as a standalone command"));
        }
    }

    // 2. The guardrails an allowlist entry may not overturn.
    let mut rationale = Vec::new();
    let mut guardrails = Vec::new();
    let mut seen_guardrail = BTreeSet::new();
    for segment in &segments {
        if !has_find_execution_predicate(segment) {
            continue;
        }
        if !seen_guardrail.insert(segment.clone()) {
            continue;
        }
        rationale.push(format!("`{segment}` asks `find` to run a program"));
        guardrails.push(PermissionRequirement::exact_command(segment));
    }
    // The guards this port keeps beyond the reference set, each of which only
    // withholds an automatic grant. A segment they name is asked about under
    // its own pattern, so the operator still has something to approve.
    let withheld = withheld_segments(flavor, command, &segments, &mut rationale);
    let guarded = !guardrails.is_empty() || !withheld.is_empty();

    // 3. The operands that leave the workspace.
    let (outside, operands) = collect_outside_directories(flavor, &segments, context);

    // 4. The grant, when nothing withheld it.
    let sensitive = segments
        .iter()
        .any(|segment| lists.sensitive(segment).is_some());
    let allowed_outright = !sensitive
        && (lists.permission == PermissionMode::Always
            || (segments
                .iter()
                .all(|segment| lists.allowed(segment).is_some())
                && outside.is_empty()));
    let commands = segments
        .iter()
        .map(|segment| command_node(segment))
        .collect::<Vec<_>>();
    if allowed_outright && !guarded {
        rationale.push("every segment is allowed and stays inside the workspace".to_owned());
        return ShellAnalysis {
            mode: PermissionMode::Always,
            rationale,
            commands,
            path_operands: operands,
            requirements: Vec::new(),
        };
    }

    // 5. What is left is what the operator answers.
    let mut requirements = Vec::new();
    let mut seen_session = BTreeSet::new();
    for segment in &segments {
        let sensitive = lists.sensitive(segment);
        if sensitive.is_none() && lists.allowed(segment).is_some() && !withheld.contains(segment) {
            continue;
        }
        // A sensitive segment carries itself as its own session pattern, so
        // approving `sudo apt update` never approves `sudo rm`.
        let requirement = if let Some(pattern) = sensitive {
            rationale.push(format!(
                "`{segment}` matches the sensitive pattern `{pattern}`"
            ));
            PermissionRequirement::exact_command(segment)
        } else {
            PermissionRequirement::command(segment)
        };
        if seen_session.insert(requirement.session_pattern.clone()) {
            requirements.push(requirement);
        }
    }
    for glob in &outside {
        rationale.push(format!("`{glob}` is outside the workspace roots"));
        requirements.push(PermissionRequirement::outside_directory(glob));
    }
    requirements.extend(guardrails);

    if requirements.is_empty() {
        // Reference `resolve_permission` returns `None` when it composed no
        // requirement, which defers to the configured permission.
        return ShellAnalysis {
            mode: lists.permission,
            rationale,
            commands,
            path_operands: operands,
            requirements: Vec::new(),
        };
    }
    ShellAnalysis {
        mode: PermissionMode::Ask,
        rationale,
        commands,
        path_operands: operands,
        requirements,
    }
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

/// The segments `command` runs.
///
/// The bash grammar answers for the two POSIX flavors, which is what the
/// reference parses every `bash` and `git_bash` call with. The two Windows
/// interpreters are not bash, so their segments still come from the word split
/// this port has always used for them; proving Windows execution equivalence is
/// an explicit non-goal of the PRD this implements.
fn segments_of(flavor: ShellFlavor, command: &str) -> Vec<String> {
    match flavor {
        ShellFlavor::Posix | ShellFlavor::GitBash => extract_commands(command),
        ShellFlavor::Cmd | ShellFlavor::PowerShell => command
            .split(['\n', ';', '|', '&'])
            .map(|segment| {
                segment
                    .split_whitespace()
                    .map(|word| word.trim_matches(['"', '\'']))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|segment| !segment.is_empty())
            .map(|segment| normalize_windows_segment(&segment))
            .collect(),
    }
}

/// A Windows segment under the lowercased basename its lists are written in.
fn normalize_windows_segment(segment: &str) -> String {
    let mut words = segment.split(' ');
    let Some(program) = words.next() else {
        return segment.to_owned();
    };
    let file = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    std::iter::once(file.as_str())
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_node(segment: &str) -> ShellCommandNode {
    let mut words = segment.split(' ').filter(|word| !word.is_empty());
    let program = words.next().unwrap_or_default().to_owned();
    ShellCommandNode {
        program,
        arguments: words.map(ToOwned::to_owned).collect(),
    }
}

/// Reference `_has_find_execution_predicate`: a `find` segment naming one of
/// the four predicates anywhere in its text.
fn has_find_execution_predicate(segment: &str) -> bool {
    if !ShellCommandLists::matches("find", segment) {
        return false;
    }
    FIND_EXECUTION_PREDICATES
        .iter()
        .any(|predicate| segment.contains(predicate))
}

/// The segments this port withholds an automatic grant from beyond the
/// reference set.
///
/// Each entry only costs an approval prompt: none of them refuses, so the guard
/// still fails toward asking. They exist because the command text alone does not
/// say what a call reads: a substitution runs another program, a redirect writes
/// somewhere the segment never names, and three git options read a path the
/// index never held. `crates/vibe-core/src/shell/shell_parity_tests.rs` records
/// every one of them against the reference verdict, so the set cannot grow
/// silently.
fn withheld_segments(
    flavor: ShellFlavor,
    command: &str,
    segments: &[String],
    rationale: &mut Vec<String>,
) -> BTreeSet<String> {
    let mut withheld = BTreeSet::new();
    if contains_indirection(flavor, command) {
        rationale.push("indirection or nested shell text requires approval".to_owned());
        withheld.extend(segments.iter().cloned());
    }
    for segment in segments {
        if segment
            .split(' ')
            .next_back()
            .is_some_and(|word| word == REDIRECT_MARKER)
        {
            rationale.push(format!(
                "`{segment}` redirects, which the text does not name"
            ));
            withheld.insert(segment.clone());
            continue;
        }
        if let Some(reason) = git_or_ripgrep_guard(segment) {
            rationale.push(reason);
            withheld.insert(segment.clone());
        }
    }
    withheld
}

/// Why a `git` or `rg` segment does not ride its allowlist entry.
fn git_or_ripgrep_guard(segment: &str) -> Option<String> {
    let mut words = segment.split(' ').filter(|word| !word.is_empty());
    let program = words.next()?;
    let arguments = words.collect::<Vec<_>>();
    if program == "git" {
        if arguments.iter().any(|argument| {
            matches!(*argument, "-c" | "--config-env" | "--exec-path")
                || argument.starts_with("-c=")
                || argument.starts_with("--config-env=")
                || argument.starts_with("--exec-path=")
        }) {
            return Some("git configuration and executable overrides require approval".to_owned());
        }
        if arguments
            .iter()
            .any(|argument| matches!(*argument, "--" | "--no-index"))
        {
            return Some("git reading outside the index requires approval".to_owned());
        }
        if arguments.first() == Some(&"reset") {
            return Some("git reset requires approval".to_owned());
        }
        return None;
    }
    if program == "rg"
        && arguments
            .iter()
            .any(|argument| *argument == "--pre" || argument.starts_with("--pre="))
    {
        return Some("ripgrep preprocessor execution requires approval".to_owned());
    }
    None
}

fn contains_indirection(flavor: ShellFlavor, command: &str) -> bool {
    command.contains("$(")
        || command.contains('`')
        || match flavor {
            ShellFlavor::PowerShell => {
                command.contains("$env:")
                    || command.trim_start().starts_with("& ")
                    || command.contains("iex ")
            }
            ShellFlavor::Cmd => {
                command.contains('%') || command.to_ascii_lowercase().contains("call ")
            }
            ShellFlavor::Posix | ShellFlavor::GitBash => command.contains("${"),
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
    for segment in segments {
        let mut words = split_operand_tokens(flavor, segment).into_iter();
        let Some(program) = words.next() else {
            continue;
        };
        if !inspects_paths(flavor, &program) {
            continue;
        }
        for token in words {
            if token.starts_with('-') {
                continue;
            }
            // A `chmod` mode is not a path, and `+x` would otherwise resolve as
            // a relative one.
            if program == "chmod" && token.starts_with('+') {
                continue;
            }
            if !looks_like_path(&token) {
                continue;
            }
            let Some(glob) = escaping_glob(flavor, context, &token) else {
                operands.push(token);
                continue;
            };
            globs.insert(glob);
            operands.push(token);
        }
    }
    (globs.into_iter().collect(), operands)
}

/// The tokens of a segment, as the interpreter's own quoting reads them.
///
/// Reference `_split_command_tokens` uses POSIX `shlex` and falls back to a
/// whitespace split when the quoting does not close. On Windows it keeps the
/// backslash literal, because a path like `C:\Users\me` would otherwise lose
/// its separators to the POSIX escape rule.
fn split_operand_tokens(flavor: ShellFlavor, segment: &str) -> Vec<String> {
    match flavor {
        ShellFlavor::Posix | ShellFlavor::GitBash => shlex::split(segment)
            .unwrap_or_else(|| segment.split_whitespace().map(ToOwned::to_owned).collect()),
        ShellFlavor::Cmd | ShellFlavor::PowerShell => segment
            .split_whitespace()
            .map(|word| word.trim_matches(['"', '\'']).to_owned())
            .collect(),
    }
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
fn escaping_glob(flavor: ShellFlavor, context: &ShellPolicyContext, token: &str) -> Option<String> {
    let platform = match flavor {
        ShellFlavor::Posix => Platform::Posix,
        ShellFlavor::GitBash => Platform::GitBash,
        ShellFlavor::Cmd | ShellFlavor::PowerShell => Platform::Windows,
    };
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
    /// standalone interpreter the denylist refuses.
    #[test]
    fn a_heredoc_is_not_a_bare_standalone_interpreter() {
        let analysis = analyze("python3 <<'EOF'\nprint(1)\nEOF");
        assert_ne!(
            analysis.mode,
            PermissionMode::Never,
            "a heredoc body is not a standalone `python3`: {:?}",
            analysis.rationale
        );
        assert_eq!(session_patterns(&analysis), vec!["python3 *".to_owned()]);
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
    /// once per distinct segment, even though `find` is allowlisted.
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
        // A plain walk stays allowlisted.
        assert_eq!(analyze("find . -name '*.rs'").mode, PermissionMode::Always);
        // The same segment twice is one approval.
        let repeated = analyze("find . -exec rm {} ; && find . -exec rm {} ;");
        assert_eq!(
            repeated.requirements.len(),
            1,
            "{:?}",
            repeated.requirements
        );
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
        for posix in [true, false] {
            let flavor = if posix {
                ShellFlavor::Posix
            } else {
                ShellFlavor::PowerShell
            };
            let uninspected = crate::tools::config::shell_read_only_commands(posix)
                .iter()
                .filter(|program| !inspects_paths(flavor, program))
                .collect::<Vec<_>>();
            assert!(
                uninspected.is_empty(),
                "read-only commands whose operands are never inspected: {uninspected:?}"
            );
        }
        // The reference set is the union with the eight mutating commands, and
        // nothing else: a command only the shared allowlist names is not
        // inspected, which is what keeps `echo /etc/passwd` an allowed echo.
        assert!(!inspects_paths(ShellFlavor::Posix, "echo"));
        assert!(inspects_paths(ShellFlavor::Posix, "rm"));
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
            ShellFlavor::Cmd,
            r"type C:\work\project\README.md",
            &context,
            &windows_lists,
        );
        assert_eq!(safe.mode, PermissionMode::Always, "{:?}", safe.rationale);
        let unc = analyze_shell(
            ShellFlavor::Cmd,
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
