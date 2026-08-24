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
    let command = Arguments::command();
    match command.try_get_matches_from(argv) {
        Ok(matches) => Arguments::from_arg_matches(&matches).map_err(|error| failure(&error)),
        Err(error) => Err(failure(&error)),
    }
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
    let command = Arguments::command();
    let usage = command.clone().render_usage().to_string();
    // argparse spells the word in lower case, and prints the block before the
    // one line that names what went wrong.
    let usage = usage
        .strip_prefix("Usage: ")
        .map_or(usage.clone(), |rest| format!("usage: {rest}"));
    format!("{usage}\nvibe: error: {message}\n")
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
                "argument {argument}: invalid choice: '{value}' (choose from {choices})"
            ))
        }
        ErrorKind::ValueValidation => {
            let raw = context(error, ContextKind::InvalidArg)?;
            let argument = option_name(&command, &raw);
            let value = context(error, ContextKind::InvalidValue)?;
            let kind = argparse_type(&command, &raw)?;
            Some(format!(
                "argument {argument}: invalid {kind} value: '{value}'"
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

#[cfg(test)]
mod argv_tests;
