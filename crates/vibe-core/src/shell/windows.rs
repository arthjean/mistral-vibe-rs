//! The PowerShell policy: how a Windows command splits, what its lists match
//! it under, and which paths it reaches.
//!
//! Reference `vibe/core/tools/builtins/windows_shell.py`. PowerShell is not
//! bash, so the reference reads it with a grammar of its own: commands split on
//! `;`, `|`, `&`, `&&`, `||` and newlines outside quotes and groups, a
//! subexpression or a script block contributes the commands inside it, and a
//! backtick escapes. A command is matched under every name it can run by: as
//! written, by its basename, without its executable suffix, and through the
//! PowerShell alias it spells, so `rm` on a list covers `Remove-Item` and
//! `C:\tools\rm.exe` does not.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use super::lexer::{WordSplit, split_tokens};
use super::{
    GuardrailDialect, ShellAnalysis, ShellCommandLists, ShellFlavor, ShellPolicyContext,
    command_node, command_requirements, deferred, escaping_directory_glob, escaping_glob,
    guardrail_requirements, refusal,
};
use crate::policy::{PermissionMode, PermissionRequirement, PermissionScope};

/// The executable suffixes a Windows command name drops.
const EXECUTABLE_SUFFIXES: [&str; 5] = [".exe", ".cmd", ".bat", ".com", ".ps1"];

/// PowerShell's built-in aliases the lists are also matched through.
const POWERSHELL_ALIASES: [(&str, &str); 30] = [
    ("ac", "add-content"),
    ("cat", "get-content"),
    ("cd", "set-location"),
    ("chdir", "set-location"),
    ("copy", "copy-item"),
    ("cp", "copy-item"),
    ("cpi", "copy-item"),
    ("del", "remove-item"),
    ("dir", "get-childitem"),
    ("erase", "remove-item"),
    ("gc", "get-content"),
    ("gci", "get-childitem"),
    ("ls", "get-childitem"),
    ("md", "new-item"),
    ("mi", "move-item"),
    ("mkdir", "new-item"),
    ("move", "move-item"),
    ("mv", "move-item"),
    ("ni", "new-item"),
    ("rd", "remove-item"),
    ("ren", "rename-item"),
    ("rename", "rename-item"),
    ("ri", "remove-item"),
    ("rm", "remove-item"),
    ("rmdir", "remove-item"),
    ("rni", "rename-item"),
    ("sc", "set-content"),
    ("sl", "set-location"),
    ("sls", "select-string"),
    ("type", "get-content"),
];

/// Commands whose arguments are text rather than paths.
const NON_PATH_COMMANDS: [&str; 3] = ["echo", "write-host", "write-output"];

/// Commands that take `/x` options, so such a token is not a root path.
const SLASH_OPTION_COMMANDS: [&str; 8] = [
    "findstr", "more", "robocopy", "tree", "ver", "where", "whoami", "xcopy",
];

static VARIABLE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\$(?:\{(?P<braced_name>[^}]+)\}|(?:(?P<scope>[A-Za-z_][\w]*):)?(?P<name>[A-Za-z_][\w]*))",
    )
    .ok()
});

static PROVIDER_PATH: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^(?P<provider>[A-Za-z][\w.-]*)::(?P<path>.*)$").ok());

static ATTACHED_VALUE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^-[^:=\s]+[:=](.+)$").ok());

// --------------------------------------------------------------------------
// Splitting
// --------------------------------------------------------------------------

fn flush(parts: &mut Vec<String>, buffer: &mut String) {
    let part = buffer.trim();
    if !part.is_empty() {
        parts.push(part.to_owned());
    }
    buffer.clear();
}

fn starts_with_at(characters: &[char], index: usize, text: &str) -> bool {
    (index..)
        .zip(text.chars())
        .all(|(position, expected)| characters.get(position) == Some(&expected))
}

/// Where the group opened at `opening_index` closes, quotes, escapes and
/// nested subexpressions skipped.
fn group_end(
    characters: &[char],
    opening_index: usize,
    opening: char,
    closing: char,
) -> Option<usize> {
    let mut depth = 1_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut index = opening_index + 1;
    while index < characters.len() {
        let character = characters[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if character == '`' {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            index += 1;
            continue;
        }
        if quote == Some('\'') {
            index += 1;
            continue;
        }
        if quote == Some('"') {
            if starts_with_at(characters, index, "$(") {
                index = group_end(characters, index + 1, '(', ')')? + 1;
                continue;
            }
            index += 1;
            continue;
        }
        if character == opening {
            depth += 1;
        } else if character == closing {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

/// The commands a PowerShell text runs: its top-level parts, then those of
/// every subexpression and group it holds.
pub(crate) fn split_command_parts(command: &str) -> Vec<String> {
    let characters = command.chars().collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut nested = Vec::new();
    let mut buffer = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if escaped {
            buffer.push(character);
            escaped = false;
            index += 1;
            continue;
        }
        if character == '`'
            || (character == '^' && matches!(characters.get(index + 1), Some('&' | '|' | ';')))
        {
            buffer.push(character);
            escaped = true;
            index += 1;
            continue;
        }
        if quote != Some('\'')
            && starts_with_at(&characters, index, "$(")
            && let Some(end) = group_end(&characters, index + 1, '(', ')')
        {
            buffer.extend(&characters[index..=end]);
            nested.push(characters[index + 2..end].iter().collect::<String>());
            index = end + 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            buffer.push(character);
            index += 1;
            continue;
        }
        if quote.is_none() {
            // A script block or a parenthesized expression is kept whole in
            // its part and read again for the commands inside it. `${` and
            // `@{` are a variable and a hashtable, not a block.
            if matches!(character, '{' | '(')
                && !(character == '{' && index > 0 && matches!(characters[index - 1], '$' | '@'))
            {
                let closing = if character == '{' { '}' } else { ')' };
                if let Some(end) = group_end(&characters, index, character, closing) {
                    buffer.extend(&characters[index..=end]);
                    nested.push(characters[index + 1..end].iter().collect::<String>());
                    index = end + 1;
                    continue;
                }
            }
            if starts_with_at(&characters, index, "&&") || starts_with_at(&characters, index, "||")
            {
                flush(&mut parts, &mut buffer);
                index += 2;
                continue;
            }
            // `2>&1` and `*>&1` duplicate a stream: that `&` belongs to the
            // redirect, not to a separator.
            if matches!(character, '&' | '|' | ';' | '\n' | '\r')
                && !(character == '&' && buffer.ends_with('>'))
            {
                flush(&mut parts, &mut buffer);
                index += 1;
                continue;
            }
        }
        buffer.push(character);
        index += 1;
    }
    flush(&mut parts, &mut buffer);
    for expression in nested {
        parts.extend(split_command_parts(&expression));
    }
    parts
}

/// The tokens of one PowerShell part: POSIX quoting, no escape character.
pub(crate) fn split_command_tokens(part: &str) -> Vec<String> {
    split_tokens(part, WordSplit::LiteralBackslash)
}

// --------------------------------------------------------------------------
// Names
// --------------------------------------------------------------------------

fn strip_quotes(value: &str) -> &str {
    value.trim_matches(['"', '\''])
}

fn strip_executable_suffix(value: &str) -> &str {
    for suffix in EXECUTABLE_SUFFIXES {
        if let Some(start) = value.len().checked_sub(suffix.len())
            && value.is_char_boundary(start)
            && value[start..].eq_ignore_ascii_case(suffix)
        {
            return &value[..start];
        }
    }
    value
}

/// The last component of a Windows path, as `PureWindowsPath.name` reads it:
/// after the drive, the UNC share and every separator, skipping `.`.
pub(crate) fn windows_basename(value: &str) -> String {
    let normalized = strip_quotes(value).replace('/', "\\");
    let rest = if let Some(unc) = normalized.strip_prefix("\\\\") {
        // `\\server\share` is the drive; only what follows it has a name.
        let mut pieces = unc.splitn(3, '\\');
        let _server = pieces.next();
        let _share = pieces.next();
        pieces.next().unwrap_or_default().to_owned()
    } else if normalized.chars().nth(1) == Some(':') {
        normalized.chars().skip(2).collect()
    } else {
        normalized
    };
    rest.split('\\')
        .rfind(|component| !component.is_empty() && *component != ".")
        .unwrap_or_default()
        .to_owned()
}

/// The lowercased program name a Windows command token runs.
pub(crate) fn windows_command_name(value: &str) -> String {
    strip_executable_suffix(&windows_basename(value)).to_lowercase()
}

/// The program a part invokes and its arguments, reading past a leading `&`.
pub(crate) fn invoked_command(tokens: &[String]) -> (String, &[String]) {
    if tokens.first().map(String::as_str) == Some("&") && tokens.len() > 1 {
        return (strip_quotes(&tokens[1]).to_owned(), &tokens[2..]);
    }
    (strip_quotes(&tokens[0]).to_owned(), &tokens[1..])
}

/// The cmdlet a bare, unsuffixed alias names; `C:\tools\rm.exe` is a program,
/// not the alias.
fn alias_target(executable: &str) -> Option<&'static str> {
    let value = strip_quotes(executable);
    if value != windows_basename(value) || value != strip_executable_suffix(value) {
        return None;
    }
    let lowered = value.to_lowercase();
    POWERSHELL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == lowered)
        .map(|(_, target)| *target)
}

fn join_form(first: &str, rest: &[String]) -> String {
    std::iter::once(first)
        .chain(rest.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

/// Every lowercased form a part is matched under.
pub(crate) fn command_match_forms(command: &str, include_path_basename: bool) -> Vec<String> {
    let tokens = split_command_tokens(command);
    if tokens.is_empty() {
        return Vec::new();
    }
    let (executable, rest) = invoked_command(&tokens);
    let basename = windows_basename(&executable);
    let mut candidates = vec![executable.clone()];
    if include_path_basename || executable == basename {
        candidates.push(basename.clone());
        candidates.push(strip_executable_suffix(&basename).to_owned());
    }
    if let Some(target) = alias_target(&executable) {
        candidates.push(target.to_owned());
    }
    let mut forms = Vec::new();
    if tokens[0] == "&" {
        forms.push(tokens.join(" ").trim().to_lowercase());
    }
    let mut seen = forms.iter().cloned().collect::<BTreeSet<_>>();
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let form = join_form(&candidate, rest);
        if seen.insert(form.clone()) {
            forms.push(form);
        }
    }
    forms
}

/// The forms a list entry matches under: itself, and the cmdlet its alias
/// names.
pub(crate) fn policy_pattern_forms(pattern: &str) -> Vec<String> {
    let normalized = pattern.to_lowercase();
    let tokens = split_command_tokens(pattern);
    if tokens.is_empty() {
        return vec![normalized];
    }
    let (executable, rest) = invoked_command(&tokens);
    let Some(target) = alias_target(&executable) else {
        return vec![normalized];
    };
    let canonical = join_form(target, rest);
    if canonical == normalized {
        vec![normalized]
    } else {
        vec![normalized, canonical]
    }
}

fn matches_policy_pattern(command: &str, pattern: &str, include_path_basename: bool) -> bool {
    let patterns = policy_pattern_forms(pattern);
    command_match_forms(command, include_path_basename)
        .iter()
        .any(|form| {
            patterns
                .iter()
                .any(|pattern| ShellCommandLists::matches(pattern, form))
        })
}

impl ShellCommandLists {
    pub(super) fn windows_denied(&self, part: &str) -> Option<&str> {
        self.denylist
            .iter()
            .find(|pattern| matches_policy_pattern(part, pattern, true))
            .map(String::as_str)
    }

    /// A part that takes no argument, matched under every form against every
    /// form of a standalone entry.
    pub(super) fn windows_denied_standalone(&self, part: &str) -> Option<&str> {
        let tokens = split_command_tokens(part);
        if tokens.is_empty() || !invoked_command(&tokens).1.is_empty() {
            return None;
        }
        let forms = command_match_forms(part, true);
        self.denylist_standalone
            .iter()
            .find(|pattern| {
                policy_pattern_forms(pattern)
                    .iter()
                    .any(|pattern| forms.contains(pattern))
            })
            .map(String::as_str)
    }

    pub(super) fn windows_allowed(&self, part: &str) -> bool {
        self.allowlist
            .iter()
            .any(|pattern| matches_policy_pattern(part, pattern, false))
    }

    pub(super) fn windows_sensitive(&self, part: &str) -> Option<&str> {
        self.sensitive_patterns
            .iter()
            .find(|pattern| matches_policy_pattern(part, pattern, true))
            .map(String::as_str)
    }
}

// --------------------------------------------------------------------------
// Tokens, redirections and paths
// --------------------------------------------------------------------------

pub(crate) fn looks_like_option(token: &str) -> bool {
    if token.starts_with('-') {
        return true;
    }
    let Some(body) = token.strip_prefix('/') else {
        return false;
    };
    !body.is_empty() && !body.starts_with('/') && !body.contains(['/', '\\', ':'])
}

pub(crate) fn looks_like_path(token: &str) -> bool {
    let value = strip_quotes(token);
    value.starts_with(['~', '.'])
        || value.starts_with("\\\\")
        || value.chars().nth(1) == Some(':')
        || value.contains(['/', '\\'])
}

/// The value an option carries after `:` or `=`, as in `-Path:C:\x`.
pub(crate) fn attached_parameter_value(token: &str) -> Option<String> {
    if !token.starts_with('-') {
        return None;
    }
    ATTACHED_VALUE
        .as_ref()?
        .captures(token)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

fn environment_value<'a>(environment: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let name = name.to_lowercase();
    environment
        .iter()
        .find(|(key, _)| key.to_lowercase() == name)
        .map(|(_, value)| value.as_str())
}

fn home_value(environment: &[(String, String)]) -> Option<String> {
    if let Some(profile) = environment_value(environment, "USERPROFILE") {
        return Some(profile.to_owned());
    }
    Some(format!(
        "{}{}",
        environment_value(environment, "HOMEDRIVE")?,
        environment_value(environment, "HOMEPATH")?
    ))
}

/// A token with the variables PowerShell would expand replaced, and whether
/// any part of it stays dynamic.
///
/// `$PWD`, `$HOME` and `$env:NAME` are known; any other variable, a leftover
/// `$` or an array subexpression is not, and a path that holds one cannot be
/// positioned.
pub(crate) fn expand_powershell_path(
    token: &str,
    command_cwd: &str,
    environment: &[(String, String)],
) -> (String, bool) {
    let mut value = strip_quotes(token).to_owned();
    let mut unresolved = false;
    if let Some(variable) = VARIABLE.as_ref() {
        value = variable
            .replace_all(&value, |captures: &regex::Captures<'_>| {
                let whole = captures
                    .get(0)
                    .map_or("", |whole| whole.as_str())
                    .to_owned();
                let (scope, name) = match captures.name("braced_name") {
                    Some(braced) => match braced.as_str().split_once(':') {
                        Some((scope, name)) => (Some(scope.to_owned()), name.to_owned()),
                        None => (None, braced.as_str().to_owned()),
                    },
                    None => (
                        captures
                            .name("scope")
                            .map(|scope| scope.as_str().to_owned()),
                        captures
                            .name("name")
                            .map_or_else(String::new, |name| name.as_str().to_owned()),
                    ),
                };
                let lowered = name.to_lowercase();
                if scope.is_none() && lowered == "pwd" {
                    return command_cwd.to_owned();
                }
                let resolved = if scope.is_none() && lowered == "home" {
                    home_value(environment)
                } else if scope
                    .as_deref()
                    .is_some_and(|scope| scope.eq_ignore_ascii_case("env"))
                {
                    environment_value(environment, &name).map(ToOwned::to_owned)
                } else {
                    None
                };
                resolved.unwrap_or_else(|| {
                    unresolved = true;
                    whole
                })
            })
            .into_owned();
    }
    unresolved = unresolved || value.contains('$') || value.contains("@(");
    if (value == "~" || value.starts_with("~/") || value.starts_with("~\\"))
        && let Some(home) = crate::config::user_home_directory()
    {
        value = format!("{}{}", home.display(), &value[1..]);
    }
    (value, unresolved)
}

/// The target an output redirection at `index` names, and where reading
/// stopped. A descriptor duplication (`>&2`) names none.
fn read_redirection_target(characters: &[char], mut index: usize) -> (Option<String>, usize) {
    if characters.get(index) == Some(&'>') {
        index += 1;
    }
    while characters
        .get(index)
        .is_some_and(|character| character.is_whitespace())
    {
        index += 1;
    }
    if characters.get(index) == Some(&'&') {
        return (None, index + 1);
    }
    let mut target = String::new();
    let mut quote = None;
    let mut escaped = false;
    while index < characters.len() {
        let character = characters[index];
        if escaped {
            target.push(character);
            escaped = false;
            index += 1;
            continue;
        }
        if character == '`' {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                target.push(character);
            }
            index += 1;
            continue;
        }
        if quote.is_none()
            && (character.is_whitespace() || matches!(character, '&' | '|' | ';' | '>'))
        {
            break;
        }
        target.push(character);
        index += 1;
    }
    let target = target.trim();
    ((!target.is_empty()).then(|| target.to_owned()), index)
}

fn redirection_targets(command: &str) -> Vec<String> {
    let characters = command.chars().collect::<Vec<_>>();
    let mut targets = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if character == '`' {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            index += 1;
            continue;
        }
        if quote.is_some() || character != '>' {
            index += 1;
            continue;
        }
        let (target, next) = read_redirection_target(&characters, index + 1);
        index = next;
        if let Some(target) = target {
            targets.push(target);
        }
    }
    targets
}

/// The output redirections that can write a file: `$null` and `NUL` discard.
pub(crate) fn file_redirection_targets(command: &str) -> Vec<String> {
    redirection_targets(command)
        .into_iter()
        .filter(|target| !matches!(target.to_lowercase().as_str(), "$null" | "nul" | "nul:"))
        .collect()
}

/// Where `token` reaches outside the workspace, as a glob, and whether it is
/// dynamic: a variable that stays unexpanded, a provider other than the file
/// system, or a drive-relative path such as `C:notes.txt`.
fn path_reach(
    token: &str,
    context: &ShellPolicyContext,
    environment: &[(String, String)],
) -> (Option<String>, bool) {
    let cwd = super::render_policy_path(&context.working_directory);
    let (mut value, unresolved) = expand_powershell_path(token, &cwd, environment);
    let mut unsupported_provider = false;
    if let Some(captures) = PROVIDER_PATH
        .as_ref()
        .and_then(|provider| provider.captures(&value))
    {
        let provider = captures
            .name("provider")
            .map_or("", |provider| provider.as_str());
        unsupported_provider = !provider.eq_ignore_ascii_case("filesystem");
        if !unsupported_provider {
            value = captures
                .name("path")
                .map_or_else(String::new, |path| path.as_str().to_owned());
        }
    }
    if unresolved || unsupported_provider {
        return (None, true);
    }
    if !looks_like_path(&value) {
        return (None, false);
    }
    // A drive with no root is relative to that drive's own current directory,
    // which nothing here can know.
    let characters = value.chars().collect::<Vec<_>>();
    if !value.starts_with("\\\\")
        && characters.get(1) == Some(&':')
        && !matches!(characters.get(2), Some('\\' | '/'))
    {
        return (None, true);
    }
    (
        escaping_glob(ShellFlavor::PowerShell, context, &value),
        false,
    )
}

/// Reference `_analyze_windows_paths`: the directories the parts reach
/// outside the workspace, as globs, and the tokens that cannot be positioned.
fn analyze_paths(
    parts: &[String],
    context: &ShellPolicyContext,
    environment: &[(String, String)],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut globs = BTreeSet::new();
    let mut dynamic = BTreeSet::new();
    if let Some(glob) = escaping_directory_glob(context, &context.working_directory) {
        globs.insert(glob);
    }
    let mut collect = |token: &str| {
        let (glob, is_dynamic) = path_reach(token, context, environment);
        if let Some(glob) = glob {
            globs.insert(glob);
        }
        if is_dynamic {
            dynamic.insert(token.to_owned());
        }
    };
    for part in parts {
        let tokens = split_command_tokens(part);
        if tokens.is_empty() {
            continue;
        }
        let (executable, arguments) = invoked_command(&tokens);
        if executable != windows_basename(&executable) {
            collect(&executable);
        }
        for target in file_redirection_targets(part) {
            collect(&target);
        }
        let program = windows_command_name(&executable);
        if program.is_empty() || NON_PATH_COMMANDS.contains(&program.as_str()) {
            continue;
        }
        for token in arguments {
            if looks_like_option(token) {
                if let Some(value) = attached_parameter_value(token) {
                    collect(&value);
                    continue;
                }
                if token.starts_with('-') || SLASH_OPTION_COMMANDS.contains(&program.as_str()) {
                    continue;
                }
            }
            collect(token);
        }
    }
    (globs, dynamic)
}

fn command_requirement(pattern: String, label: String) -> PermissionRequirement {
    PermissionRequirement {
        scope: PermissionScope::CommandPattern,
        invocation_pattern: pattern.clone(),
        session_pattern: pattern,
        label,
        literal: false,
    }
}

/// Reference `_resolve_windows_permission`.
pub(super) fn analyze_powershell(
    command: &str,
    context: &ShellPolicyContext,
    lists: &ShellCommandLists,
) -> ShellAnalysis {
    let parts = split_command_parts(command);
    if parts.is_empty() {
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
    let guardrails = match guardrail_requirements(&parts, context, lists, GuardrailDialect::Windows)
    {
        Ok(guardrails) => guardrails,
        Err(reason) => return refusal(reason),
    };
    // The host environment with the call's overrides merged in: an override
    // replaces a variable spelled the same way in place, and the first entry
    // matching a name case-insensitively is the one read.
    let mut environment = std::env::vars().collect::<Vec<_>>();
    for (key, value) in &context.environment {
        match environment.iter_mut().find(|(existing, _)| existing == key) {
            Some((_, slot)) => slot.clone_from(value),
            None => environment.push((key.clone(), value.clone())),
        }
    }
    let (outside, dynamic) = analyze_paths(&parts, context, &environment);

    let mut context_required = context.context_requirements.clone();
    let targets = parts
        .iter()
        .flat_map(|part| file_redirection_targets(part))
        .collect::<BTreeSet<_>>();
    context_required.extend(targets.into_iter().map(|target| {
        command_requirement(
            format!("output redirection: {target}"),
            format!("output redirection ({target})"),
        )
    }));

    let mut rationale = Vec::new();
    let sensitive = parts
        .iter()
        .any(|part| lists.windows_sensitive(part).is_some());
    let unconditional = !sensitive
        && context_required.is_empty()
        && (lists.permission == PermissionMode::Always
            || (parts.iter().all(|part| lists.windows_allowed(part))
                && outside.is_empty()
                && dynamic.is_empty()));
    if unconditional && guardrails.is_empty() {
        rationale.push("every command is allowed and stays inside the workspace".to_owned());
        return ShellAnalysis {
            mode: PermissionMode::Always,
            rationale,
            commands,
            path_operands: Vec::new(),
            requirements: Vec::new(),
        };
    }

    let outside = outside.into_iter().collect::<Vec<_>>();
    let mut requirements = command_requirements(
        &parts,
        &outside,
        false,
        |part| lists.windows_sensitive(part),
        |part| lists.windows_allowed(part),
        &mut rationale,
    );
    requirements.extend(context_required);
    requirements.extend(dynamic.into_iter().map(|path| {
        rationale.push(format!("`{path}` cannot be positioned before it runs"));
        command_requirement(
            format!("dynamic path: {path}"),
            format!("dynamic PowerShell path ({path})"),
        )
    }));
    requirements.extend(guardrails);
    if requirements.is_empty() {
        return deferred(lists, rationale, commands, Vec::new());
    }
    ShellAnalysis {
        mode: PermissionMode::Ask,
        rationale,
        commands,
        path_operands: Vec::new(),
        requirements,
    }
}

/// Whether a PowerShell session may be a pager: reference
/// `BashStdin.resolve_permission` for the Windows families.
pub(super) fn runs_pager(command: &str, pagers: &[&str]) -> bool {
    super::expand_guardrail_commands(&split_command_parts(command), WordSplit::LiteralBackslash)
        .iter()
        .any(|part| {
            let tokens = split_command_tokens(part);
            !tokens.is_empty()
                && pagers.contains(&windows_command_name(&invoked_command(&tokens).0).as_str())
        })
}

#[cfg(test)]
mod windows_tests;
