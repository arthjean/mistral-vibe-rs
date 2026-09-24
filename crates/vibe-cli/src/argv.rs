//! Reading the interactive argv, and refusing it the way the reference does.
//!
//! Both parsers exit 2 on a refusal and write it to standard error, and there
//! the agreement stops: argparse prints its usage block and one line reading
//! `prog: error: message`, while clap prints a paragraph, a tip and a pointer
//! at `--help`. The sentences argparse renders here are CPython's own
//! templates rather than anything the reference wrote, so this port reproduces
//! them exactly and reports them in the same shape
//! (`vibe/cli/entrypoint.py:27` builds the parser on the default handler).

use std::ffi::OsString;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, Command, CommandFactory, Error, FromArgMatches};

use crate::Arguments;

pub(crate) mod reading;

/// The first argument the reference reads as `--check-upgrade`.
const UPDATE_COMMAND: &str = "update";

/// The arguments only this port declares. Each answers to its exact spelling
/// alone, so none of them can capture an abbreviation the reference resolves
/// to one of its own options (`--p` is `--prompt` there, not
/// `--provider-style`).
pub(crate) const PORT_ONLY_ARGUMENTS: &[&str] = &[
    "tool_filters",
    "provider_style",
    "model",
    "input_price",
    "output_price",
    "api_base",
    "credential_environment",
    "session_root",
    "fake_response",
];

/// The agent `--smart-approve` selects when `--agent` names none.
pub const SMART_APPROVE_AGENT: &str = "smart-approve";

/// A refusal, already rendered, with where it goes and what it exits.
pub struct ParseFailure {
    /// The whole block to write, newline-terminated.
    pub rendered: String,
    /// 2 for a refusal, 0 for the help and the version documents.
    pub exit: u8,
    /// Whether the block belongs on standard error.
    pub use_stderr: bool,
}

/// The interactive arguments, or the block that explains why there are none.
///
/// # Errors
///
/// Returns the rendered refusal when the argv does not parse, and the rendered
/// document when it asked for the help or the version instead.
pub fn parse_arguments<I, T>(argv: I) -> Result<Arguments, ParseFailure>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut argv = argv.into_iter().map(Into::into).collect::<Vec<OsString>>();
    // Reference `parse_arguments`: `vibe update` is `vibe --check-upgrade`,
    // decided on the first argument alone before the parser sees it, so
    // `update` anywhere else is still a prompt (`vibe/cli/entrypoint.py:206-208`).
    if let Some(first) = argv.get_mut(1)
        && first == UPDATE_COMMAND
    {
        *first = OsString::from("--check-upgrade");
    }
    let command = Arguments::command();
    let reading = reading::read(&command, argv, PORT_ONLY_ARGUMENTS);
    let matches = command
        .try_get_matches_from(reading.argv)
        .map_err(|error| failure(&error))?;
    if let Some(message) = reading.refusal {
        return Err(refusal(&message));
    }
    let mut arguments = Arguments::from_arg_matches(&matches).map_err(|error| failure(&error))?;
    // Reference `parse_arguments`: smart approve is a Unified Harness gate, so
    // the flag also asks for that harness, and it names the agent only when
    // `--agent` did not (`vibe/cli/entrypoint.py:210-217`). The rewrite
    // follows the parse, which is why `--smart-approve --legacy-harness` is
    // accepted there with both harness flags set.
    if arguments.smart_approve {
        arguments.experimental_harness = true;
        if arguments.agent.is_none() {
            arguments.agent = Some(SMART_APPROVE_AGENT.to_owned());
        }
    }
    Ok(arguments)
}

/// A refusal argparse raises and clap has no kind for, in the same shape as
/// the ones [`render`] rebuilds.
fn refusal(message: &str) -> ParseFailure {
    ParseFailure {
        rendered: format!("{}\nvibe: error: {message}\n", usage()),
        exit: 2,
        use_stderr: true,
    }
}

/// argparse's usage block: clap's, with the word in lower case.
fn usage() -> String {
    let usage = Arguments::command().render_usage().to_string();
    usage
        .strip_prefix("Usage: ")
        .map_or(usage.clone(), |rest| format!("usage: {rest}"))
}

fn failure(error: &Error) -> ParseFailure {
    ParseFailure {
        rendered: render(error),
        exit: error.exit_code().clamp(0, i32::from(u8::MAX)) as u8,
        use_stderr: error.use_stderr(),
    }
}

/// The block this port writes for one clap refusal.
///
/// A kind with no argparse sentence falls back on clap's own render: an
/// unrecognized refusal that says something the user can act on is worth more
/// than one shaped like the reference and empty.
fn render(error: &Error) -> String {
    let Some(message) = argparse_message(error) else {
        return error.render().to_string();
    };
    // argparse prints the usage block before the one line that names what
    // went wrong.
    format!("{}\nvibe: error: {message}\n", usage())
}

/// The sentence CPython's `argparse` renders for this refusal, where this port
/// can rebuild it from what clap reports.
fn argparse_message(error: &Error) -> Option<String> {
    let command = Arguments::command();
    match error.kind() {
        ErrorKind::UnknownArgument => Some(format!(
            "unrecognized arguments: {}",
            context(error, ContextKind::InvalidArg)?
        )),
        ErrorKind::InvalidValue => {
            let argument = option_name(&command, &context(error, ContextKind::InvalidArg)?);
            // clap reports an absent value and a rejected one under the same
            // kind, and tells them apart only by the value it carries.
            let value = context(error, ContextKind::InvalidValue)?;
            if value.is_empty() {
                return Some(format!("argument {argument}: expected one argument"));
            }
            let choices = values(error, ContextKind::ValidValue)?.join(", ");
            Some(format!(
                "argument {argument}: invalid choice: {} (choose from {choices})",
                reading::python_repr(&value)
            ))
        }
        ErrorKind::ValueValidation => {
            let raw = context(error, ContextKind::InvalidArg)?;
            let argument = option_name(&command, &raw);
            let value = context(error, ContextKind::InvalidValue)?;
            let kind = argparse_type(&command, &raw)?;
            Some(format!(
                "argument {argument}: invalid {kind} value: {}",
                reading::python_repr(&value)
            ))
        }
        ErrorKind::ArgumentConflict => {
            // argparse reports the pair from the argument it was reading when
            // it found the conflict, which is the later of the two; clap names
            // the earlier one first, so the two halves are read the other way
            // round here.
            let subject = option_name(&command, &context(error, ContextKind::PriorArg)?);
            let prior = spellings(&command, &context(error, ContextKind::InvalidArg)?);
            Some(format!(
                "argument {subject}: not allowed with argument {prior}"
            ))
        }
        _ => None,
    }
}

fn context(error: &Error, kind: ContextKind) -> Option<String> {
    error.get(kind).and_then(|value| match value {
        ContextValue::String(value) => Some(value.clone()),
        _ => None,
    })
}

fn values(error: &Error, kind: ContextKind) -> Option<Vec<String>> {
    error.get(kind).and_then(|value| match value {
        ContextValue::Strings(values) => Some(values.clone()),
        _ => None,
    })
}

/// The argument clap names in `--output <{text,json,streaming}>`, without the
/// value name argparse leaves out of its error lines.
fn declared<'a>(command: &'a Command, reported: &str) -> Option<&'a Arg> {
    let spelling = reported.split_whitespace().next()?;
    command.get_arguments().find(|argument| {
        argument
            .get_long()
            .is_some_and(|long| format!("--{long}") == spelling)
            || argument
                .get_short()
                .is_some_and(|short| format!("-{short}") == spelling)
    })
}

/// The name argparse prints for one argument: the long spelling where there is
/// one, which is what `_get_action_name` returns for every flag this parser
/// declares.
fn option_name(command: &Command, reported: &str) -> String {
    declared(command, reported)
        .and_then(Arg::get_long)
        .map_or_else(
            || {
                reported
                    .split_whitespace()
                    .next()
                    .unwrap_or(reported)
                    .to_owned()
            },
            |long| format!("--{long}"),
        )
}

/// Every spelling of one argument, joined the way argparse joins
/// `option_strings` when it names the argument a conflict was found against.
fn spellings(command: &Command, reported: &str) -> String {
    let Some(argument) = declared(command, reported) else {
        return reported.to_owned();
    };
    let mut names = Vec::new();
    if let Some(short) = argument.get_short() {
        names.push(format!("-{short}"));
    }
    if let Some(long) = argument.get_long() {
        names.push(format!("--{long}"));
    }
    if names.is_empty() {
        return reported.to_owned();
    }
    names.join("/")
}

/// The name of the Python callable the reference passes as `type=`, which is
/// the word argparse prints when the conversion fails.
///
/// Rust spells the same two conversions `u32`, `u64` and `f64`, and a message
/// naming those would not be the reference's, so the pairing is declared here
/// beside the flags it covers (`vibe/cli/entrypoint.py:60-100`).
fn argparse_type(command: &Command, reported: &str) -> Option<&'static str> {
    match declared(command, reported)?.get_id().as_str() {
        "max_turns" | "max_tokens" => Some("int"),
        "max_price" | "input_price" | "output_price" => Some("float"),
        _ => None,
    }
}

/// Python's `int()` over one argument, which is the `type=int` the reference
/// declares for `--max-turns` and `--max-tokens`.
///
/// Surrounding whitespace, a sign and single underscores between digits are
/// accepted, as there. Python's integers are unbounded and this one is not: a
/// value past the range of `i64` saturates, which keeps the budget it sets
/// (none reached, or spent already) and loses only the digits.
///
/// # Errors
///
/// Returns an error for anything `int()` would refuse, which clap reports as a
/// failed conversion and [`render`] as argparse's `invalid int value`.
pub fn python_int(value: &str) -> Result<i64, String> {
    let trimmed = value.trim_matches(is_python_whitespace);
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if !has_python_digit_grouping(digits) {
        return Err(format!("invalid int value: {value:?}"));
    }
    let magnitude = digits
        .bytes()
        .filter(u8::is_ascii_digit)
        .fold(0_i128, |total, digit| {
            total
                .saturating_mul(10)
                .saturating_add(i128::from(digit - b'0'))
        });
    let signed = if negative { -magnitude } else { magnitude };
    Ok(i64::try_from(signed).unwrap_or(if negative { i64::MIN } else { i64::MAX }))
}

/// Python's `float()` over one argument, the `type=float` of `--max-price`.
///
/// # Errors
///
/// Returns an error for anything `float()` would refuse.
pub fn python_float(value: &str) -> Result<f64, String> {
    let trimmed = value.trim_matches(is_python_whitespace);
    let characters: Vec<char> = trimmed.chars().collect();
    // An underscore only ever separates two digits.
    let grouped = characters.iter().enumerate().all(|(index, character)| {
        *character != '_'
            || (index > 0
                && characters[index - 1].is_ascii_digit()
                && characters.get(index + 1).is_some_and(char::is_ascii_digit))
    });
    let plain: String = characters.iter().filter(|c| **c != '_').collect();
    match plain.parse::<f64>() {
        Ok(parsed) if grouped && !plain.is_empty() => Ok(parsed),
        _ => Err(format!("invalid float value: {value:?}")),
    }
}

/// Digits, optionally grouped by single underscores, as Python's integer
/// literal grammar allows them.
fn has_python_digit_grouping(digits: &str) -> bool {
    !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'_')
}

/// `str.strip()`'s notion of whitespace, which adds the four ASCII separator
/// controls to Unicode's.
fn is_python_whitespace(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

#[cfg(test)]
mod argv_tests;
