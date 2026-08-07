use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platform::{PathPolicyError, Platform, PolicyPath, parse_policy_path};
use crate::policy::PermissionMode;
use crate::tools::config::ShellCommandConfig;

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellCommandLists {
    pub allowlist: Vec<String>,
    pub denylist: Vec<String>,
    pub denylist_standalone: Vec<String>,
    pub sensitive_patterns: Vec<String>,
}

impl ShellCommandLists {
    /// The four lists a resolved shell configuration carries.
    #[must_use]
    pub fn from_config(config: &ShellCommandConfig) -> Self {
        Self {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellAnalysis {
    pub mode: PermissionMode,
    pub rationale: Vec<String>,
    pub commands: Vec<ShellCommandNode>,
    pub path_operands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Operator(String),
}

/// Resolves the policy for `command` against the built-in analysis and the
/// four configured lists.
///
/// The lists are consulted per extracted segment, in the reference order: a
/// denylist or standalone-denylist match refuses the whole command, a sensitive
/// first word keeps the segment out of any automatic grant, and an allowlist
/// match replaces what the built-in analysis decided about the command text.
/// The path-operand walk still runs either way, so an allowlisted reader
/// pointed outside the workspace roots reaches the operator rather than the
/// filesystem.
pub fn analyze_shell(
    flavor: ShellFlavor,
    command: &str,
    context: &ShellPolicyContext,
    lists: &ShellCommandLists,
) -> ShellAnalysis {
    let tokens = match tokenize(flavor, command) {
        Ok(tokens) => tokens,
        Err(message) => {
            return ShellAnalysis {
                mode: PermissionMode::Ask,
                rationale: vec![message],
                commands: Vec::new(),
                path_operands: Vec::new(),
            };
        }
    };
    let mut rationale = Vec::new();
    let mut mode = PermissionMode::Always;
    if contains_indirection(flavor, command) {
        mode = PermissionMode::Ask;
        rationale.push("indirection or nested shell text requires approval".to_owned());
    }
    if tokens.iter().any(|token| {
        matches!(token, Token::Operator(operator) if matches!(operator.as_str(), ">" | "<"))
    }) {
        mode = mode.min(PermissionMode::Ask);
        rationale.push("shell redirection requires approval".to_owned());
    }
    let segments = split_commands(tokens);
    let mut commands = Vec::new();
    let mut path_operands = Vec::new();
    for segment in segments {
        let words = segment
            .into_iter()
            .filter_map(|token| match token {
                Token::Word(word) => Some(word),
                Token::Operator(_) => None,
            })
            .collect::<Vec<_>>();
        let Some(program) = words.first().cloned() else {
            mode = mode.min(PermissionMode::Ask);
            rationale.push("empty command segment requires approval".to_owned());
            continue;
        };
        let arguments = words[1..].to_vec();
        let normalized = normalize_program(flavor, &program);
        let segment = std::iter::once(normalized.clone())
            .chain(arguments.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let decision = command_mode(flavor, &normalized, &arguments);
        if let Some(pattern) = lists.denied(&segment) {
            return ShellAnalysis {
                mode: PermissionMode::Never,
                rationale: vec![format!(
                    "`{segment}` matches the denylist entry `{pattern}`"
                )],
                commands: Vec::new(),
                path_operands: Vec::new(),
            };
        }
        if let Some(entry) = lists.denied_standalone(&segment) {
            return ShellAnalysis {
                mode: PermissionMode::Never,
                rationale: vec![format!("`{entry}` is refused as a standalone command")],
                commands: Vec::new(),
                path_operands: Vec::new(),
            };
        }
        // Reference `resolve_permission` grants an allowlisted segment only
        // when no guardrail fired for it, which is why `find -exec` stays an
        // approval even though `find` is on the allowlist.
        let decision = match (lists.sensitive(&segment), lists.allowed(&segment)) {
            // A sensitive first word is never granted automatically, whatever
            // the allowlist and the built-in analysis say about it.
            (Some(pattern), _) => (
                PermissionMode::Ask,
                format!("`{segment}` matches the sensitive pattern `{pattern}`"),
            ),
            (None, Some(pattern)) if !decision.guarded => (
                PermissionMode::Always,
                format!("`{segment}` matches the allowlist entry `{pattern}`"),
            ),
            _ => (decision.mode, decision.rationale),
        };
        mode = mode.min(decision.0);
        rationale.push(decision.1);
        let operands = path_operands_for(&normalized, &arguments, lists);
        for operand in operands {
            match normalize_operand(flavor, context, &operand) {
                Ok(path) if inside_any_root(&path, &context.roots) => {
                    match host_path_is_authorized(&path, context) {
                        Some(true) => {
                            mode = mode.min(PermissionMode::Ask);
                            rationale.push(format!(
                                "path `{operand}` requires approval because shell access cannot be bound to the validated filesystem object"
                            ));
                        }
                        Some(false) => {
                            mode = mode.min(PermissionMode::Ask);
                            rationale.push(format!(
                                "path `{operand}` resolves outside workspace roots or cannot be resolved"
                            ));
                        }
                        None => {}
                    }
                    path_operands.push(operand);
                }
                Ok(_) => {
                    mode = mode.min(PermissionMode::Ask);
                    rationale.push(format!("path `{operand}` is outside workspace roots"));
                    path_operands.push(operand);
                }
                Err(_) => {
                    mode = mode.min(PermissionMode::Ask);
                    rationale.push(format!("path `{operand}` is ambiguous"));
                    path_operands.push(operand);
                }
            }
        }
        commands.push(ShellCommandNode { program, arguments });
    }
    ShellAnalysis {
        mode,
        rationale,
        commands,
        path_operands,
    }
}

/// Whether the segment asks `find` to run something, which no allowlist entry
/// covers.
fn has_execution_predicate(program: &str, arguments: &[String]) -> bool {
    program == "find"
        && arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "-exec" | "-execdir" | "-delete"))
}

/// What the built-in analysis decides about one command segment.
///
/// `guarded` marks a decision an allowlist entry may not overturn: the branches
/// that exist because a command can be pointed somewhere it does not normally
/// go. Reference `resolve_permission` keeps the same separation, granting an
/// allowlisted segment only when its own guardrail pass found nothing.
struct CommandDecision {
    mode: PermissionMode,
    rationale: String,
    guarded: bool,
}

fn decided(mode: PermissionMode, rationale: String) -> CommandDecision {
    CommandDecision {
        mode,
        rationale,
        guarded: false,
    }
}

fn guarded(mode: PermissionMode, rationale: String) -> CommandDecision {
    CommandDecision {
        mode,
        rationale,
        guarded: true,
    }
}

fn command_mode(flavor: ShellFlavor, program: &str, arguments: &[String]) -> CommandDecision {
    let destructive = match flavor {
        ShellFlavor::Posix | ShellFlavor::GitBash => {
            ["rm", "rmdir", "dd", "mkfs", "shutdown", "sudo", "eval"].contains(&program)
        }
        ShellFlavor::Cmd => ["del", "erase", "format", "rd", "rmdir"].contains(&program),
        ShellFlavor::PowerShell => [
            "remove-item",
            "clear-content",
            "format-volume",
            "invoke-expression",
            "start-process",
        ]
        .contains(&program),
    };
    if destructive {
        return guarded(
            PermissionMode::Never,
            format!("destructive command `{program}` is denied"),
        );
    }
    if has_execution_predicate(program, arguments) {
        return guarded(
            PermissionMode::Ask,
            "find execution predicates require approval".to_owned(),
        );
    }
    if program == "git" {
        if arguments.iter().any(|argument| {
            matches!(argument.as_str(), "-c" | "--config-env" | "--exec-path")
                || argument.starts_with("-c=")
                || argument.starts_with("--config-env=")
                || argument.starts_with("--exec-path=")
        }) {
            return guarded(
                PermissionMode::Ask,
                "git configuration and executable overrides require approval".to_owned(),
            );
        }
        let subcommand = arguments
            .iter()
            .find(|argument| !argument.starts_with('-'))
            .map(String::as_str)
            .unwrap_or_default();
        // A destructive subcommand is answered before anything that only makes
        // a read broader, so a pathspec separator cannot turn the denial below
        // into an approval prompt.
        if subcommand == "reset" && arguments.iter().any(|argument| argument == "--hard") {
            return guarded(
                PermissionMode::Never,
                "destructive git reset is denied".to_owned(),
            );
        }
        // `--no-index` and a pathspec separator both let git read a path the
        // workspace never named, so neither is covered by an allowlist entry.
        if arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "--" | "--no-index"))
        {
            return guarded(
                PermissionMode::Ask,
                "git reading outside the index requires approval".to_owned(),
            );
        }
        return match subcommand {
            "" | "rev-parse" => decided(PermissionMode::Always, "read-only git command".to_owned()),
            "status"
                if arguments
                    .iter()
                    .filter(|argument| !argument.starts_with('-'))
                    .count()
                    == 1 =>
            {
                decided(PermissionMode::Always, "read-only git status".to_owned())
            }
            "branch"
                if arguments.iter().all(|argument| {
                    argument == "branch"
                        || matches!(
                            argument.as_str(),
                            "--list"
                                | "-l"
                                | "--show-current"
                                | "--contains"
                                | "--no-contains"
                                | "--merged"
                                | "--no-merged"
                                | "--points-at"
                                | "--format"
                                | "--sort"
                                | "--color"
                                | "--no-color"
                        )
                        || argument.starts_with("--format=")
                        || argument.starts_with("--sort=")
                        || argument.starts_with("--color=")
                }) =>
            {
                decided(
                    PermissionMode::Always,
                    "read-only git branch query".to_owned(),
                )
            }
            _ => decided(
                PermissionMode::Ask,
                format!("git subcommand `{subcommand}` requires approval"),
            ),
        };
    }
    if program == "rg"
        && arguments
            .iter()
            .any(|argument| argument == "--pre" || argument.starts_with("--pre="))
    {
        return guarded(
            PermissionMode::Ask,
            "ripgrep preprocessor execution requires approval".to_owned(),
        );
    }
    let read_only = match flavor {
        ShellFlavor::Posix | ShellFlavor::GitBash => [
            "pwd", "ls", "cat", "head", "tail", "rg", "grep", "find", "wc", "which",
        ]
        .contains(&program),
        ShellFlavor::Cmd => ["cd", "dir", "type", "findstr", "where"].contains(&program),
        ShellFlavor::PowerShell => [
            "get-content",
            "gc",
            "get-childitem",
            "gci",
            "get-location",
            "select-string",
        ]
        .contains(&program),
    };
    if read_only {
        decided(
            PermissionMode::Always,
            format!("reader `{program}` is allowlisted"),
        )
    } else {
        decided(
            PermissionMode::Ask,
            format!("unknown or mutating command `{program}` requires approval"),
        )
    }
}

fn tokenize(flavor: ShellFlavor, command: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && matches!(flavor, ShellFlavor::Posix | ShellFlavor::GitBash) {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            continue;
        }
        if character.is_whitespace() {
            push_word(&mut tokens, &mut current);
            continue;
        }
        if matches!(character, ';' | '|' | '&') {
            push_word(&mut tokens, &mut current);
            let mut operator = character.to_string();
            if chars.peek() == Some(&character) {
                operator.push(character);
                chars.next();
            }
            tokens.push(Token::Operator(operator));
            continue;
        }
        if matches!(character, '>' | '<') {
            push_word(&mut tokens, &mut current);
            tokens.push(Token::Operator(character.to_string()));
            continue;
        }
        current.push(character);
    }
    if quote.is_some() || escaped {
        return Err("unparseable shell quoting requires approval".to_owned());
    }
    push_word(&mut tokens, &mut current);
    if tokens.is_empty() {
        return Err("empty shell command requires approval".to_owned());
    }
    Ok(tokens)
}

fn push_word(tokens: &mut Vec<Token>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(Token::Word(std::mem::take(current)));
    }
}

fn split_commands(tokens: Vec<Token>) -> Vec<Vec<Token>> {
    let mut segments = vec![Vec::new()];
    for token in tokens {
        match &token {
            Token::Operator(operator) if matches!(operator.as_str(), ";" | "&&" | "||" | "|") => {
                segments.push(Vec::new());
            }
            Token::Operator(operator) if matches!(operator.as_str(), ">" | "<") => {
                if let Some(segment) = segments.last_mut() {
                    segment.push(Token::Word("__redirection__".to_owned()));
                }
            }
            _ => {
                if let Some(segment) = segments.last_mut() {
                    segment.push(token);
                }
            }
        }
    }
    segments
}

fn contains_indirection(flavor: ShellFlavor, command: &str) -> bool {
    command.contains("$(")
        || command.contains('`')
        || command.contains("__redirection__")
        || match flavor {
            ShellFlavor::PowerShell => {
                command.contains("$env:")
                    || command.trim_start().starts_with("& ")
                    || command.contains("iex ")
            }
            ShellFlavor::Cmd => {
                command.contains('%') || command.to_ascii_lowercase().contains("call ")
            }
            ShellFlavor::Posix | ShellFlavor::GitBash => {
                command.contains("${") || command.contains("eval ")
            }
        }
}

fn normalize_program(flavor: ShellFlavor, program: &str) -> String {
    let file = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe");
    match flavor {
        ShellFlavor::Cmd | ShellFlavor::PowerShell => file.to_ascii_lowercase(),
        ShellFlavor::Posix | ShellFlavor::GitBash => file.to_owned(),
    }
}

/// The operands of `program` that name a path, or nothing when it takes none.
///
/// `lists` widens the inspected set with everything the operator allowlisted:
/// reference `_PATH_COMMANDS` is documented as a superset of the read-only
/// allowlist for exactly this reason, since a command that can be auto-allowed
/// must have its paths checked first or `wc -l /etc/passwd` reads outside the
/// workspace without ever asking.
fn path_operands_for(
    program: &str,
    arguments: &[String],
    lists: &ShellCommandLists,
) -> Vec<String> {
    let takes_paths = [
        "ls",
        "cat",
        "head",
        "tail",
        "rg",
        "grep",
        "find",
        "dir",
        "type",
        "findstr",
        "get-content",
        "gc",
        "get-childitem",
        "gci",
        "select-string",
    ];
    let allowlisted = lists
        .allowlist
        .iter()
        .any(|pattern| pattern.split_whitespace().next() == Some(program));
    if !takes_paths.contains(&program) && !allowlisted {
        return Vec::new();
    }
    arguments
        .iter()
        .filter(|argument| !argument.starts_with('-') && *argument != "__redirection__")
        .filter(|argument| {
            !(matches!(program, "rg" | "grep" | "findstr" | "select-string")
                && arguments.first() == Some(*argument))
        })
        .cloned()
        .collect()
}

fn normalize_operand(
    flavor: ShellFlavor,
    context: &ShellPolicyContext,
    operand: &str,
) -> Result<PolicyPath, PathPolicyError> {
    let platform = match flavor {
        ShellFlavor::Posix => Platform::Posix,
        ShellFlavor::GitBash => Platform::GitBash,
        ShellFlavor::Cmd | ShellFlavor::PowerShell => Platform::Windows,
    };
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
        Err(error) => Err(error),
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
        ShellPolicyContext {
            platform: Platform::Posix,
            working_directory: parse_policy_path(Platform::Posix, "/work/project")
                .expect("working directory"),
            roots: vec![
                parse_policy_path(Platform::Posix, "/work/project").expect("workspace root"),
            ],
        }
    }

    #[test]
    fn posix_nested_commands_and_find_exec_are_conservative() {
        let safe = analyze_shell(
            ShellFlavor::Posix,
            "pwd && rg needle src",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(safe.mode, PermissionMode::Always);
        assert_eq!(safe.commands.len(), 2);
        let find = analyze_shell(
            ShellFlavor::Posix,
            "find . -exec sh -c 'echo x' \\;",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(find.mode, PermissionMode::Ask);
        let nested = analyze_shell(
            ShellFlavor::Posix,
            "cat \"$(credential-helper)\"",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(nested.mode, PermissionMode::Ask);
    }

    #[test]
    fn explicit_deny_wins_over_reader_segments() {
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "cat README.md && rm secret",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(analysis.mode, PermissionMode::Never);
    }

    #[test]
    fn executable_reader_options_and_git_no_index_require_approval() {
        let ripgrep = analyze_shell(
            ShellFlavor::Posix,
            "rg --pre malicious-helper needle .",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(ripgrep.mode, PermissionMode::Ask);

        let git = analyze_shell(
            ShellFlavor::Posix,
            "git diff --no-index /etc/passwd /dev/null",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(git.mode, PermissionMode::Ask);
    }

    /// A pathspec separator broadens a read; it never softens the denial a
    /// destructive subcommand already earned.
    #[test]
    fn a_pathspec_separator_does_not_reopen_a_destructive_git_reset() {
        for command in ["git reset --hard", "git reset --hard -- src"] {
            let analysis = analyze_shell(
                ShellFlavor::Posix,
                command,
                &posix_context(),
                &posix_lists(),
            );
            assert_eq!(
                analysis.mode,
                PermissionMode::Never,
                "`{command}`: {:?}",
                analysis.rationale
            );
        }
    }

    /// US-103: the four configured lists decide a segment before the built-in
    /// analysis does, which is what closes the commands the reference refuses
    /// and this port used to run.
    #[test]
    fn the_configured_lists_refuse_what_the_reference_refuses() {
        for command in ["vim notes.txt", "nano", "tmux", "gdb ./binary", "passwd"] {
            let analysis = analyze_shell(
                ShellFlavor::Posix,
                command,
                &posix_context(),
                &posix_lists(),
            );
            assert_eq!(
                analysis.mode,
                PermissionMode::Never,
                "`{command}` is on the reference denylist: {:?}",
                analysis.rationale
            );
        }
        // The standalone denylist refuses the bare interpreter and nothing else.
        let bare = analyze_shell(
            ShellFlavor::Posix,
            "python3",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(bare.mode, PermissionMode::Never);
        let scripted = analyze_shell(
            ShellFlavor::Posix,
            "python3 script.py",
            &posix_context(),
            &posix_lists(),
        );
        assert_ne!(
            scripted.mode,
            PermissionMode::Never,
            "the same interpreter with an argument is not a standalone command"
        );
    }

    /// A sensitive first word never rides an allowlist entry, and it is asked
    /// about even where the analysis would have allowed it.
    #[test]
    fn a_sensitive_first_word_is_always_asked_about() {
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "sudo ls",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(
            analysis
                .rationale
                .iter()
                .any(|line| line.contains("sensitive pattern `sudo`")),
            "{:?}",
            analysis.rationale
        );
    }

    /// The allowlist grant never outruns the operand walk: an allowlisted
    /// reader pointed outside the roots still reaches the operator.
    #[test]
    fn an_allowlisted_reader_pointed_outside_the_roots_still_asks() {
        let inside = analyze_shell(
            ShellFlavor::Posix,
            "wc -l /work/project/notes.txt",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(
            inside.mode,
            PermissionMode::Always,
            "`wc` is on the reference read-only allowlist: {:?}",
            inside.rationale
        );
        let outside = analyze_shell(
            ShellFlavor::Posix,
            "wc -l /etc/passwd",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(outside.mode, PermissionMode::Ask);
    }

    /// An operator who empties the allowlist gets an approval per command
    /// rather than an unusable tool.
    #[test]
    fn an_emptied_allowlist_asks_rather_than_refusing() {
        let lists = ShellCommandLists::default();
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "vim notes.txt",
            &posix_context(),
            &lists,
        );
        assert_ne!(
            analysis.mode,
            PermissionMode::Never,
            "an emptied denylist stops refusing what it used to refuse"
        );
        let reader = analyze_shell(ShellFlavor::Posix, "sudo ls", &posix_context(), &lists);
        assert_eq!(
            reader.mode,
            PermissionMode::Never,
            "the built-in analysis still guards sudo"
        );
    }

    #[test]
    fn paths_outside_roots_require_approval() {
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "cat /etc/passwd",
            &posix_context(),
            &posix_lists(),
        );
        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(
            analysis
                .rationale
                .iter()
                .any(|line| line.contains("outside"))
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
        let root_text = root.path().to_string_lossy();
        let context = ShellPolicyContext {
            platform: Platform::Posix,
            working_directory: parse_policy_path(Platform::Posix, &root_text).expect("cwd"),
            roots: vec![parse_policy_path(Platform::Posix, &root_text).expect("root")],
        };

        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "cat outside/secret",
            &context,
            &posix_lists(),
        );

        assert_eq!(analysis.mode, PermissionMode::Ask);
        assert!(
            analysis
                .rationale
                .iter()
                .any(|line| line.contains("resolves outside"))
        );
    }

    #[test]
    fn windows_shells_handle_aliases_drives_unc_and_ambiguity() {
        let context = ShellPolicyContext {
            platform: Platform::Windows,
            working_directory: parse_policy_path(Platform::Windows, r"C:\work\project")
                .expect("cwd"),
            roots: vec![parse_policy_path(Platform::Windows, r"C:\work\project").expect("root")],
        };
        let safe = analyze_shell(
            ShellFlavor::PowerShell,
            r"gc C:\work\project\README.md",
            &context,
            &posix_lists(),
        );
        assert_eq!(safe.mode, PermissionMode::Always);
        let unc = analyze_shell(
            ShellFlavor::Cmd,
            r"type \\server\share\secret.txt",
            &context,
            &posix_lists(),
        );
        assert_eq!(unc.mode, PermissionMode::Ask);
        let provider = analyze_shell(
            ShellFlavor::PowerShell,
            r"gc Env:\SECRET",
            &context,
            &posix_lists(),
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
