//! What a command's options mean for its approval, read per program.
//!
//! Reference `vibe/core/tools/builtins/_shell_command_policy.py`. Allowlisting a
//! reader such as `sort` or `git log` grants what the reader does with its
//! input, not an option that writes a file, runs a helper or sets the clock.
//! Each table below names, for one program, the options that take the call out
//! of the grant and the option values that name a path the operand walk must
//! position. Option grammar is modeled per program because the programs
//! disagree about it: which short options take a value, whether an abbreviated
//! long option is accepted, and where a cluster stops.

use super::FIND_EXECUTION_PREDICATES;

/// What one command's tokens ask of the permission resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommandPolicy {
    /// An option withholds the grant the allowlist would give.
    pub(crate) requires_approval: bool,
    /// The positional operands are paths even though the program is not one
    /// of the path-inspecting commands.
    pub(crate) inspect_positional_paths: bool,
    /// A git reader whose repository configuration can run a helper.
    pub(crate) inspect_git_repository: bool,
    /// Option values that name a path.
    pub(crate) option_path_values: Vec<String>,
}

impl CommandPolicy {
    fn asking(requires_approval: bool) -> Self {
        Self {
            requires_approval,
            ..Self::default()
        }
    }

    fn reading(option_path_values: Vec<String>) -> Self {
        Self {
            option_path_values,
            ..Self::default()
        }
    }
}

/// The programs whose policy can withhold a grant.
///
/// Reference `_OPTION_GATED_COMMANDS`. The others only nominate path
/// candidates, which become outside-directory requirements of their own.
const OPTION_GATED_COMMANDS: [&str; 15] = [
    "date",
    "du",
    "file",
    "find",
    "git",
    "less",
    "md5sum",
    "more",
    "sha1sum",
    "sha256sum",
    "shasum",
    "sort",
    "tree",
    "uniq",
    "wc",
];

const WINDOWS_EXECUTABLE_SUFFIXES: [&str; 4] = [".exe", ".cmd", ".bat", ".com"];

/// The program a command token names: unquoted, without its directory, folded
/// to lowercase and without a Windows executable suffix.
pub(crate) fn command_name(token: &str) -> String {
    let unquoted = token.trim_matches(['"', '\'']).replace('\\', "/");
    let base = unquoted
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    WINDOWS_EXECUTABLE_SUFFIXES
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .map_or_else(|| base.clone(), ToOwned::to_owned)
}

/// Whether the program has option guardrails, which is what makes its grant
/// literal: a trailing `*` would cover the option the guardrail asks about.
pub(crate) fn has_option_guardrails<S: AsRef<str>>(tokens: &[S]) -> bool {
    tokens
        .first()
        .is_some_and(|first| OPTION_GATED_COMMANDS.contains(&command_name(first.as_ref()).as_str()))
}

/// The policy `tokens` resolve to, program first.
pub(crate) fn analyze_command_policy(tokens: &[String]) -> CommandPolicy {
    let Some(first) = tokens.first() else {
        return CommandPolicy::default();
    };
    let args = &tokens[1..];
    match command_name(first).as_str() {
        "date" => date_policy(args),
        "diff" => diff_policy(args),
        "du" => du_policy(args),
        "file" => file_policy(args),
        "find" => find_policy(args),
        "git" => git_policy(args),
        "grep" => grep_policy(args),
        "less" | "more" => less_policy(args),
        "md5sum" | "sha1sum" | "sha256sum" | "shasum" => checksum_policy(args),
        "sort" => sort_policy(args),
        "tree" => tree_policy(args),
        "uniq" => uniq_policy(args),
        "wc" => files0_from_policy(args),
        _ => CommandPolicy::default(),
    }
}

/// The operands and option values of `tokens` that may name a path.
///
/// Reference `path_candidates`: the option values always, and the positional
/// operands when the caller inspects this program or its policy asks for them.
/// A `--` ends the options, a `chmod` mode is never a path.
pub(crate) fn path_candidates(tokens: &[String], inspect_positional_paths: bool) -> Vec<String> {
    let Some(first) = tokens.first() else {
        return Vec::new();
    };
    let policy = analyze_command_policy(tokens);
    let mut candidates = policy.option_path_values;
    if !(inspect_positional_paths || policy.inspect_positional_paths) {
        return candidates;
    }
    let program = command_name(first);
    let mut options_ended = false;
    for token in &tokens[1..] {
        if token == "--" {
            options_ended = true;
            continue;
        }
        if !options_ended && token.starts_with('-') {
            continue;
        }
        if program == "chmod" && token.starts_with('+') {
            continue;
        }
        candidates.push(token.clone());
    }
    candidates
}

// --------------------------------------------------------------------------
// Option grammar
// --------------------------------------------------------------------------

/// Whether `token` spells the long `option`, whole or abbreviated, with or
/// without an attached value.
fn matches_long_option(token: &str, option: &str) -> bool {
    let name = token.split_once('=').map_or(token, |(name, _)| name);
    name == option || (name.starts_with("--") && name != "--" && option.starts_with(name))
}

fn matches_any_long(token: &str, options: &[&str]) -> bool {
    options
        .iter()
        .any(|option| matches_long_option(token, option))
}

/// Whether a short cluster reaches one of `options` before an option that
/// takes the rest of the cluster as its value.
fn contains_short_option(token: &str, options: &[char], value_options: &[char]) -> bool {
    if !token.starts_with('-') || token.starts_with("--") {
        return false;
    }
    for option in token.chars().skip(1) {
        if options.contains(&option) {
            return true;
        }
        if value_options.contains(&option) {
            return false;
        }
    }
    false
}

/// The tokens before a `--`.
fn option_tokens(args: &[String]) -> &[String] {
    args.iter()
        .position(|token| token == "--")
        .map_or(args, |end| &args[..end])
}

/// The values the long `options` carry, attached or in the next token.
fn long_option_values(args: &[String], options: &[&str]) -> Vec<String> {
    let tokens = option_tokens(args);
    let mut values = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !matches_any_long(token, options) {
            continue;
        }
        match token.split_once('=') {
            Some((_, attached)) if !attached.is_empty() => values.push(attached.to_owned()),
            Some(_) => {}
            None => {
                if let Some(next) = tokens.get(index + 1) {
                    values.push(next.clone());
                }
            }
        }
    }
    values
}

/// The values the short `options` carry, the rest of the cluster or the next
/// token.
fn short_option_values(args: &[String], options: &[char], value_options: &[char]) -> Vec<String> {
    let tokens = option_tokens(args);
    let mut values = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !token.starts_with('-') || token.starts_with("--") {
            continue;
        }
        let cluster = token.chars().collect::<Vec<_>>();
        for (position, option) in cluster.iter().enumerate().skip(1) {
            if options.contains(option) {
                let attached = cluster[position + 1..].iter().collect::<String>();
                if !attached.is_empty() {
                    values.push(attached);
                } else if let Some(next) = tokens.get(index + 1) {
                    values.push(next.clone());
                }
                break;
            }
            if value_options.contains(option) {
                break;
            }
        }
    }
    values
}

/// Python's `str.isdigit` on ASCII text: non-empty and digits only.
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|character| character.is_ascii_digit())
}

// --------------------------------------------------------------------------
// Per-program policies
// --------------------------------------------------------------------------

fn sort_policy(args: &[String]) -> CommandPolicy {
    let writes = [
        "--compress-program",
        "--files0-from",
        "--output",
        "--temporary-directory",
    ];
    CommandPolicy {
        requires_approval: option_tokens(args).iter().any(|token| {
            matches_any_long(token, &writes)
                || contains_short_option(token, &['o', 'T'], &['k', 'S', 't'])
        }),
        option_path_values: long_option_values(args, &["--random-source"]),
        ..CommandPolicy::default()
    }
}

fn grep_policy(args: &[String]) -> CommandPolicy {
    let mut values = long_option_values(args, &["--exclude-from", "--file"]);
    values.extend(short_option_values(
        args,
        &['f'],
        &['A', 'B', 'C', 'D', 'd', 'e', 'm'],
    ));
    CommandPolicy::reading(values)
}

fn file_policy(args: &[String]) -> CommandPolicy {
    let mut values = long_option_values(args, &["--files-from"]);
    values.extend(short_option_values(args, &['f'], &['e', 'F', 'P']));
    let mut magic = long_option_values(args, &["--magic-file"]);
    magic.extend(short_option_values(args, &['m'], &['e', 'F', 'P']));
    values.extend(
        magic
            .iter()
            .flat_map(|value| value.split(':').map(ToOwned::to_owned)),
    );
    let gated = [
        "--compile",
        "--files-from",
        "--uncompress",
        "--uncompress-noreport",
    ];
    CommandPolicy {
        requires_approval: option_tokens(args).iter().any(|token| {
            matches_any_long(token, &gated)
                || contains_short_option(token, &['C', 'f', 'z', 'Z'], &['e', 'F', 'm', 'P'])
        }),
        option_path_values: values,
        ..CommandPolicy::default()
    }
}

fn names_files0_from(args: &[String]) -> bool {
    option_tokens(args)
        .iter()
        .any(|token| matches_long_option(token, "--files0-from"))
}

fn files0_from_policy(args: &[String]) -> CommandPolicy {
    CommandPolicy {
        requires_approval: names_files0_from(args),
        option_path_values: long_option_values(args, &["--files0-from"]),
        ..CommandPolicy::default()
    }
}

fn du_policy(args: &[String]) -> CommandPolicy {
    let mut values = long_option_values(args, &["--files0-from"]);
    values.extend(long_option_values(args, &["--exclude-from"]));
    values.extend(short_option_values(args, &['X'], &['B', 'd', 't']));
    CommandPolicy {
        requires_approval: names_files0_from(args),
        option_path_values: values,
        ..CommandPolicy::default()
    }
}

/// For one short `date` cluster: whether it holds `-j`, whether it holds
/// `-f`, and whether its last option takes the next token as its value.
fn date_short_option_state(token: &str) -> (bool, bool, bool) {
    let cluster = token.chars().collect::<Vec<_>>();
    let mut no_set = false;
    let mut input_format = false;
    for (position, option) in cluster.iter().enumerate().skip(1) {
        no_set = no_set || *option == 'j';
        input_format = input_format || *option == 'f';
        if *option == 'I' {
            break;
        }
        if ['d', 'f', 'r', 'v', 'z'].contains(option) {
            return (no_set, input_format, position + 1 == cluster.len());
        }
    }
    (no_set, input_format, false)
}

/// BSD's positional clock setter, `[[[[[cc]yy]mm]dd]HH]MM[.ss]`.
fn matches_bsd_date_setting_operand(value: &str) -> bool {
    let (date, seconds) = match value.split_once('.') {
        Some((date, seconds)) => (date, Some(seconds)),
        None => (value, None),
    };
    is_digits(date)
        && [2, 4, 6, 8, 10, 12].contains(&date.chars().count())
        && seconds.is_none_or(|seconds| seconds.chars().count() == 2 && is_digits(seconds))
}

/// Whether a `date` call sets the clock through a BSD positional operand.
fn date_has_setting_operand(args: &[String]) -> bool {
    let mut no_set = false;
    let mut input_format = false;
    let mut positional = Vec::new();
    let mut skip_next = false;
    let mut options_ended = false;
    for token in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if options_ended {
            if !token.starts_with('+') {
                positional.push(token.as_str());
            }
            continue;
        }
        if token == "--" {
            options_ended = true;
            continue;
        }
        if token.starts_with('+') {
            continue;
        }
        if token.starts_with("--") {
            if !token.contains('=')
                && ["--date", "--file", "--reference", "--rfc-3339"].contains(&token.as_str())
            {
                skip_next = true;
            }
            continue;
        }
        if token.starts_with('-') && token != "-" {
            let (token_no_set, token_input_format, takes_next) = date_short_option_state(token);
            no_set = no_set || token_no_set;
            input_format = input_format || token_input_format;
            skip_next = takes_next;
            continue;
        }
        positional.push(token.as_str());
    }
    if no_set || positional.is_empty() {
        return false;
    }
    input_format
        || positional
            .iter()
            .any(|value| matches_bsd_date_setting_operand(value))
}

fn date_policy(args: &[String]) -> CommandPolicy {
    let requires_approval = date_has_setting_operand(args)
        || option_tokens(args).iter().any(|token| {
            matches_long_option(token, "--set")
                || contains_short_option(token, &['s'], &['d', 'f', 'I', 'r'])
        });
    let mut values = long_option_values(args, &["--file"]);
    values.extend(short_option_values(args, &['f'], &['d', 's']));
    CommandPolicy {
        requires_approval,
        option_path_values: values,
        ..CommandPolicy::default()
    }
}

fn diff_policy(args: &[String]) -> CommandPolicy {
    let mut values = long_option_values(args, &["--exclude-from", "--from-file", "--to-file"]);
    values.extend(short_option_values(
        args,
        &['X'],
        &['C', 'D', 'F', 'I', 'L', 'S', 'U', 'W', 'x'],
    ));
    CommandPolicy::reading(values)
}

fn find_policy(args: &[String]) -> CommandPolicy {
    CommandPolicy::asking(
        option_tokens(args)
            .iter()
            .any(|token| FIND_EXECUTION_PREDICATES.contains(&token.as_str())),
    )
}

fn checksum_policy(args: &[String]) -> CommandPolicy {
    CommandPolicy::asking(option_tokens(args).iter().any(|token| {
        matches_long_option(token, "--check") || contains_short_option(token, &['c'], &['a'])
    }))
}

fn tree_policy(args: &[String]) -> CommandPolicy {
    // tree resumes scanning a cluster after an option that took a value, so
    // any `o` in a short cluster reaches the output-file option.
    let requires_approval = option_tokens(args).iter().any(|token| {
        matches_long_option(token, "--output")
            || (token.starts_with('-') && !token.starts_with("--") && token[1..].contains('o'))
    });
    CommandPolicy {
        requires_approval,
        inspect_positional_paths: true,
        option_path_values: long_option_values(args, &["--gitfile"]),
        ..CommandPolicy::default()
    }
}

fn git_policy(args: &[String]) -> CommandPolicy {
    let Some(subcommand) = args
        .first()
        .filter(|first| ["diff", "log", "status"].contains(&first.as_str()))
    else {
        return CommandPolicy::default();
    };
    let subcommand_args = &args[1..];
    let options = option_tokens(subcommand_args);
    let risky = [
        "--ext-diff",
        "--help",
        "--output",
        "--remerge-diff",
        "--show-signature",
        "--textconv",
    ];
    let remerges = long_option_values(subcommand_args, &["--diff-merges"])
        .iter()
        .any(|value| matches!(value.to_lowercase().as_str(), "r" | "remerge"));
    let requires_approval = options.iter().any(|token| matches_any_long(token, &risky)) || remerges;
    let mut option_paths = long_option_values(subcommand_args, &["--pathspec-from-file"]);
    if subcommand != "status" {
        option_paths.extend(short_option_values(subcommand_args, &['O'], &[]));
    }
    CommandPolicy {
        requires_approval,
        inspect_positional_paths: subcommand == "diff"
            && options.iter().any(|token| token == "--no-index"),
        // A plain reader stays granted in an ordinary repository; the resolver
        // reads the repository's own configuration before it grants.
        inspect_git_repository: true,
        option_path_values: option_paths,
    }
}

fn uniq_policy(args: &[String]) -> CommandPolicy {
    let mut positional = 0_usize;
    let mut skip_next = false;
    let mut options_ended = false;
    for token in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if options_ended {
            positional += 1;
            continue;
        }
        if token == "--" {
            options_ended = true;
            continue;
        }
        if token.starts_with("--") {
            if !token.contains('=')
                && ["--check-chars", "--skip-chars", "--skip-fields"].contains(&token.as_str())
            {
                skip_next = true;
            }
            continue;
        }
        let legacy_plus = token.strip_prefix('+').is_some_and(is_digits);
        if (token.starts_with('-') && token != "-") || legacy_plus {
            let cluster = token.chars().collect::<Vec<_>>();
            for (position, option) in cluster.iter().enumerate().skip(1) {
                if ['f', 's', 'w'].contains(option) {
                    if position + 1 == cluster.len() {
                        skip_next = true;
                    }
                    break;
                }
            }
            continue;
        }
        positional += 1;
    }
    // A second operand is the output file uniq writes.
    CommandPolicy::asking(positional >= 2)
}

// --------------------------------------------------------------------------
// less and more
// --------------------------------------------------------------------------

/// The long options that take a value in the next token.
const LESS_LONG_VALUE_OPTIONS: [&str; 23] = [
    "--autosave",
    "--buffers",
    "--cmd",
    "--color",
    "--emouse",
    "--end-prompt",
    "--jump-target",
    "--lesskey-content",
    "--lesskey-context",
    "--lesskey-file",
    "--lesskey-src",
    "--log-file",
    "--max-back-scroll",
    "--max-forw-scroll",
    "--pattern",
    "--prompt",
    "--quotes",
    "--rscroll",
    "--shift",
    "--tabs",
    "--tag",
    "--tag-file",
    "--window",
];

/// The long options that write a file, run a command or load key bindings.
const LESS_APPROVAL_LONG_OPTIONS: [&str; 9] = [
    "--autosave",
    "--cmd",
    "--lesskey-content",
    "--lesskey-context",
    "--lesskey-file",
    "--lesskey-src",
    "--log-file",
    "--tag",
    "--tag-file",
];

const LESS_LONG_NUMERIC_VALUE_OPTIONS: [&str; 13] = [
    "--buffers",
    "--header",
    "--jump-target",
    "--line-num-width",
    "--match-shift",
    "--max-back-scroll",
    "--max-forw-scroll",
    "--modelines",
    "--shift",
    "--status-col-width",
    "--tabs",
    "--wheel-lines",
    "--window",
];

const LESS_SHORT_VALUE_OPTIONS: &str = "\"#DbhjkopPtTxyz";
const LESS_SHORT_NUMERIC_VALUE_OPTIONS: &str = "#bhjxyz";
const LESS_APPROVAL_SHORT_OPTIONS: &str = "koOtT";

fn is_less_long_string_value_option(option: &str) -> bool {
    LESS_LONG_VALUE_OPTIONS
        .iter()
        .chain(&["--intr", "--search-options"])
        .any(|candidate| matches_long_option(option, candidate))
}

/// Python's `str.isprintable`: no control, format, separator or unassigned
/// character other than the ASCII space.
fn is_printable(text: &str) -> bool {
    text.chars().all(|character| {
        character == ' '
            || !(character.is_control()
                || character.is_whitespace()
                || matches!(
                    character,
                    '\u{ad}'
                        | '\u{600}'..='\u{605}'
                        | '\u{61c}'
                        | '\u{6dd}'
                        | '\u{70f}'
                        | '\u{180e}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2064}'
                        | '\u{2066}'..='\u{206f}'
                        | '\u{feff}'
                        | '\u{fff9}'..='\u{fffb}'
                        | '\u{e000}'..='\u{f8ff}'
                ))
    })
}

/// A resumed option text, read as a short cluster when it has no sign.
fn resumed_requires_approval(suffix: &str) -> bool {
    let suffix = if suffix.starts_with(['-', '+']) {
        suffix.to_owned()
    } else {
        format!("-{suffix}")
    };
    less_policy(&[suffix]).requires_approval
}

fn normalize_less_long_option(token: &str) -> String {
    let token = match token.strip_prefix("--+") {
        Some(rest) => format!("--{rest}"),
        None => token.to_owned(),
    };
    match token.split_once('=') {
        Some((option, value)) => format!("{}={value}", option.to_lowercase()),
        None => token.to_lowercase(),
    }
}

fn less_startup_requires_approval(token: &str) -> bool {
    let command = token
        .strip_prefix("++")
        .unwrap_or_else(|| token.get(1..).unwrap_or_default());
    if command == "g"
        || command == "G"
        || (!command.is_empty() && command.chars().all(|character| character.is_ascii_digit()))
    {
        return false;
    }
    if !(command.starts_with(['/', '?']) && is_printable(command)) {
        return true;
    }
    less_string_value_requires_approval(&command[1..])
}

/// Whether what follows a numeric value, where less resumes parsing, asks.
fn less_numeric_value_requires_approval(value: &str) -> bool {
    let characters = value.chars().collect::<Vec<_>>();
    let mut index = 0;
    if characters.len() > 1
        && characters[0] == '-'
        && (characters[1].is_ascii_digit() || characters[1] == '.')
    {
        index = 1;
    }
    while index < characters.len()
        && (characters[index].is_ascii_digit() || matches!(characters[index], '.' | ','))
    {
        index += 1;
    }
    let suffix = characters[index..].iter().collect::<String>();
    !suffix.is_empty() && resumed_requires_approval(&suffix)
}

/// Whether what follows the `$` that ends an attached string value asks.
fn less_string_value_requires_approval(value: &str) -> bool {
    match value.split_once('$') {
        Some((_, suffix)) if !suffix.is_empty() => resumed_requires_approval(suffix),
        _ => false,
    }
}

fn less_long_option_requires_approval(token: &str) -> bool {
    if matches_any_long(token, &LESS_APPROVAL_LONG_OPTIONS) {
        return true;
    }
    let Some((option, attached)) = token.split_once('=') else {
        return false;
    };
    if matches_any_long(option, &LESS_LONG_NUMERIC_VALUE_OPTIONS) {
        return less_numeric_value_requires_approval(attached);
    }
    if is_less_long_string_value_option(option) {
        return less_string_value_requires_approval(attached);
    }
    // A string option a newer less adds shares the `$` terminator grammar, so
    // a risky resumed suffix asks before the option is in the table.
    attached.contains('$') && less_string_value_requires_approval(attached)
}

/// For one short cluster: whether it asks, and whether it takes the next
/// token as its value.
fn less_short_option_action(token: &str) -> (bool, bool) {
    let characters = token.chars().collect::<Vec<_>>();
    let mut index = 1;
    while index < characters.len() {
        let remainder = characters[index..].iter().collect::<String>();
        if remainder.starts_with("--") {
            return (
                less_long_option_requires_approval(&normalize_less_long_option(&remainder)),
                false,
            );
        }
        let option = characters[index];
        if option == '$' {
            index += 1;
            continue;
        }
        if option == '+' {
            return (less_startup_requires_approval(&remainder), false);
        }
        if option.is_ascii_digit() {
            return (less_numeric_value_requires_approval(&remainder), false);
        }
        if LESS_APPROVAL_SHORT_OPTIONS.contains(option) {
            return (true, false);
        }
        if !LESS_SHORT_VALUE_OPTIONS.contains(option) {
            index += 1;
            continue;
        }
        let attached = characters[index + 1..].iter().collect::<String>();
        let requires_approval = !attached.is_empty()
            && if LESS_SHORT_NUMERIC_VALUE_OPTIONS.contains(option) {
                less_numeric_value_requires_approval(&attached)
            } else {
                less_string_value_requires_approval(&attached)
            };
        return (requires_approval, attached.is_empty());
    }
    (false, false)
}

fn less_policy(args: &[String]) -> CommandPolicy {
    if args.iter().any(|argument| !is_printable(argument)) {
        return CommandPolicy::asking(true);
    }
    for argument in args {
        let Some((_, resumed)) = argument.split_once('$') else {
            continue;
        };
        let resumed = resumed.trim_start();
        if !resumed.is_empty() && resumed_requires_approval(resumed) {
            return CommandPolicy::asking(true);
        }
    }
    let mut skip_next = false;
    for argument in args {
        let ends_options = argument == "--";
        for token in argument.split_whitespace() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if token == "--" && ends_options {
                return CommandPolicy::default();
            }
            if token.starts_with('+') && less_startup_requires_approval(token) {
                return CommandPolicy::asking(true);
            }
            if token.starts_with("--") {
                let normalized = normalize_less_long_option(token);
                if less_long_option_requires_approval(&normalized) {
                    return CommandPolicy::asking(true);
                }
                // Only an exact spelling hides the next token: an abbreviation
                // or an option an older less does not know leaves it exposed.
                if !normalized.contains('=')
                    && LESS_LONG_VALUE_OPTIONS.contains(&normalized.as_str())
                {
                    skip_next = true;
                }
                continue;
            }
            if !token.starts_with('-') {
                continue;
            }
            let (requires_approval, takes_next) = less_short_option_action(token);
            if requires_approval {
                return CommandPolicy::asking(true);
            }
            skip_next = takes_next;
        }
    }
    CommandPolicy::default()
}

#[cfg(test)]
mod command_policy_tests;
