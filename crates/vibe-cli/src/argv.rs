//! Reading the interactive argv, and refusing it the way the reference does.
//!
//! Both parsers exit 2 on a refusal and write it to standard error, and there
//! the agreement stops: argparse prints its usage block and one line reading
//! `prog: error: message`, while clap prints a paragraph, a tip and a pointer
//! at `--help`. [`crate::argparse`] reads a clap failure as the argparse
//! refusal it is and renders it (`vibe/cli/entrypoint.py:27` builds the
//! parser on the default handler).

use std::ffi::OsString;

use clap::{Arg, CommandFactory, Error, FromArgMatches};
use vibe_core::pycompat;

use crate::Arguments;
use crate::argparse::{self, Refusal, error_block};

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
    if let Some(refusal) = reading.refusal {
        return Err(refused(&refusal));
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
fn refused(refusal: &Refusal) -> ParseFailure {
    ParseFailure {
        rendered: error_block(&usage(), "vibe", &refusal.to_string()),
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
    let Some(refusal) = argparse::refusal(error, &Arguments::command(), argparse_type) else {
        return error.render().to_string();
    };
    error_block(&usage(), "vibe", &refusal.to_string())
}

/// The name of the Python callable the reference passes as `type=`, which is
/// the word argparse prints when the conversion fails.
///
/// Rust spells the same two conversions `u32`, `u64` and `f64`, and a message
/// naming those would not be the reference's, so the pairing is declared here
/// beside the flags it covers (`vibe/cli/entrypoint.py:60-100`).
fn argparse_type(argument: &Arg) -> Option<&'static str> {
    match argument.get_id().as_str() {
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
    let parsed =
        pycompat::parse_int(value).ok_or_else(|| format!("invalid int value: {value:?}"))?;
    Ok(i64::try_from(parsed).unwrap_or(if parsed < 0 { i64::MIN } else { i64::MAX }))
}

/// Python's `float()` over one argument, the `type=float` of `--max-price`.
///
/// # Errors
///
/// Returns an error for anything `float()` would refuse.
pub fn python_float(value: &str) -> Result<f64, String> {
    pycompat::parse_float(value).ok_or_else(|| format!("invalid float value: {value:?}"))
}

#[cfg(test)]
mod argv_tests;
