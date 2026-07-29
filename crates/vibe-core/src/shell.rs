use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platform::{PathPolicyError, Platform, PolicyPath, parse_policy_path};
use crate::policy::PermissionMode;

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

pub fn analyze_shell(
    flavor: ShellFlavor,
    command: &str,
    context: &ShellPolicyContext,
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
        let decision = command_mode(flavor, &normalized, &arguments);
        mode = mode.min(decision.0);
        rationale.push(decision.1);
        let operands = path_operands_for(&normalized, &arguments);
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

fn command_mode(
    flavor: ShellFlavor,
    program: &str,
    arguments: &[String],
) -> (PermissionMode, String) {
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
        return (
            PermissionMode::Never,
            format!("destructive command `{program}` is denied"),
        );
    }
    if program == "find"
        && arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "-exec" | "-execdir" | "-delete"))
    {
        return (
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
            return (
                PermissionMode::Ask,
                "git configuration and executable overrides require approval".to_owned(),
            );
        }
        let subcommand = arguments
            .iter()
            .find(|argument| !argument.starts_with('-'))
            .map(String::as_str)
            .unwrap_or_default();
        return match subcommand {
            "" | "rev-parse"
                if !arguments
                    .iter()
                    .any(|argument| matches!(argument.as_str(), "--" | "--no-index")) =>
            {
                (PermissionMode::Always, "read-only git command".to_owned())
            }
            "status"
                if arguments
                    .iter()
                    .filter(|argument| !argument.starts_with('-'))
                    .count()
                    == 1
                    && !arguments
                        .iter()
                        .any(|argument| matches!(argument.as_str(), "--" | "--no-index")) =>
            {
                (PermissionMode::Always, "read-only git status".to_owned())
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
                (
                    PermissionMode::Always,
                    "read-only git branch query".to_owned(),
                )
            }
            "reset" if arguments.iter().any(|argument| argument == "--hard") => (
                PermissionMode::Never,
                "destructive git reset is denied".to_owned(),
            ),
            _ => (
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
        return (
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
        (
            PermissionMode::Always,
            format!("reader `{program}` is allowlisted"),
        )
    } else {
        (
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

fn path_operands_for(program: &str, arguments: &[String]) -> Vec<String> {
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
    if !takes_paths.contains(&program) {
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
        let safe = analyze_shell(ShellFlavor::Posix, "pwd && rg needle src", &posix_context());
        assert_eq!(safe.mode, PermissionMode::Always);
        assert_eq!(safe.commands.len(), 2);
        let find = analyze_shell(
            ShellFlavor::Posix,
            "find . -exec sh -c 'echo x' \\;",
            &posix_context(),
        );
        assert_eq!(find.mode, PermissionMode::Ask);
        let nested = analyze_shell(
            ShellFlavor::Posix,
            "cat \"$(credential-helper)\"",
            &posix_context(),
        );
        assert_eq!(nested.mode, PermissionMode::Ask);
    }

    #[test]
    fn explicit_deny_wins_over_reader_segments() {
        let analysis = analyze_shell(
            ShellFlavor::Posix,
            "cat README.md && rm secret",
            &posix_context(),
        );
        assert_eq!(analysis.mode, PermissionMode::Never);
    }

    #[test]
    fn executable_reader_options_and_git_no_index_require_approval() {
        let ripgrep = analyze_shell(
            ShellFlavor::Posix,
            "rg --pre malicious-helper needle .",
            &posix_context(),
        );
        assert_eq!(ripgrep.mode, PermissionMode::Ask);

        let git = analyze_shell(
            ShellFlavor::Posix,
            "git diff --no-index /etc/passwd /dev/null",
            &posix_context(),
        );
        assert_eq!(git.mode, PermissionMode::Ask);
    }

    #[test]
    fn paths_outside_roots_require_approval() {
        let analysis = analyze_shell(ShellFlavor::Posix, "cat /etc/passwd", &posix_context());
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

        let analysis = analyze_shell(ShellFlavor::Posix, "cat outside/secret", &context);

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
        );
        assert_eq!(safe.mode, PermissionMode::Always);
        let unc = analyze_shell(
            ShellFlavor::Cmd,
            r"type \\server\share\secret.txt",
            &context,
        );
        assert_eq!(unc.mode, PermissionMode::Ask);
        let provider = analyze_shell(ShellFlavor::PowerShell, r"gc Env:\SECRET", &context);
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
