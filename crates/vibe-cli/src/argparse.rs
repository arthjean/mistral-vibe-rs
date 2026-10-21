//! CPython `argparse`'s refusal grammar and help render, for every argv this
//! port reads over a clap declaration.
//!
//! The reference builds its parsers on argparse's default handler, which
//! prints a usage block, one `{prog}: error: {message}` line and exits 2. The
//! sentences are CPython's own templates rather than anything the reference
//! wrote, so they are reproduced exactly, once, here: the interactive parser
//! ([`crate::argv`]) and `vibe mcp` ([`crate::mcp_command`]) each decide which
//! refusal a clap failure is, and this module says it.

use std::fmt;
use std::io::Write;

use clap::error::{ContextKind, ContextValue};
use clap::{Arg, ArgAction, Command};
use vibe_core::pycompat;

/// The exit code an argument refusal carries, which is argparse's own.
pub(crate) const USAGE_EXIT: u8 = 2;

/// argparse's help column: two past the widest invocation, capped.
const MAX_HELP_POSITION: usize = 24;

/// One refusal argparse raises, rendered as the sentence it prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// What no parser consumed, joined by spaces.
    Unrecognized(String),
    /// The required arguments that were not given.
    Required(Vec<String>),
    /// An abbreviation that names more than one option.
    Ambiguous {
        token: String,
        matches: Vec<String>,
    },
    /// A flag given `=value`, which argparse refuses rather than strips.
    IgnoredExplicit {
        argument: String,
        value: String,
    },
    ExpectedOne {
        argument: String,
    },
    InvalidChoice {
        argument: String,
        value: String,
        choices: Vec<String>,
    },
    /// A value the `type=` callable named `kind` refused.
    InvalidValue {
        argument: String,
        kind: &'static str,
        value: String,
    },
    /// Two arguments of one mutually exclusive group.
    NotAllowed {
        argument: String,
        prior: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrecognized(extras) => write!(formatter, "unrecognized arguments: {extras}"),
            Self::Required(names) => write!(
                formatter,
                "the following arguments are required: {}",
                names.join(", ")
            ),
            Self::Ambiguous { token, matches } => write!(
                formatter,
                "ambiguous option: {token} could match {}",
                matches.join(", ")
            ),
            Self::IgnoredExplicit { argument, value } => write!(
                formatter,
                "argument {argument}: ignored explicit argument {}",
                pycompat::repr_str(value)
            ),
            Self::ExpectedOne { argument } => {
                write!(formatter, "argument {argument}: expected one argument")
            }
            Self::InvalidChoice {
                argument,
                value,
                choices,
            } => write!(
                formatter,
                "argument {argument}: invalid choice: {} (choose from {})",
                pycompat::repr_str(value),
                choices.join(", ")
            ),
            Self::InvalidValue {
                argument,
                kind,
                value,
            } => write!(
                formatter,
                "argument {argument}: invalid {kind} value: {}",
                pycompat::repr_str(value)
            ),
            Self::NotAllowed { argument, prior } => write!(
                formatter,
                "argument {argument}: not allowed with argument {prior}"
            ),
        }
    }
}

/// argparse's `parser.error` block: the usage, then the one line naming what
/// went wrong, newline-terminated.
pub(crate) fn error_block(usage: &str, prog: &str, message: &str) -> String {
    format!("{usage}\n{prog}: error: {message}\n")
}

/// One string a clap failure carries under `kind`.
pub(crate) fn context(error: &clap::Error, kind: ContextKind) -> Option<String> {
    match error.get(kind)? {
        ContextValue::String(value) => Some(value.clone()),
        _ => None,
    }
}

/// The list of strings a clap failure carries under `kind`.
pub(crate) fn values(error: &clap::Error, kind: ContextKind) -> Option<Vec<String>> {
    match error.get(kind)? {
        ContextValue::Strings(values) => Some(values.clone()),
        _ => None,
    }
}

// --------------------------------------------------------------------------
// The help render
// --------------------------------------------------------------------------

pub(crate) fn subcommand_names(command: &Command) -> Vec<String> {
    command
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect()
}

fn prog_of(command: &Command) -> String {
    command
        .get_bin_name()
        .unwrap_or_else(|| command.get_name())
        .to_owned()
}

/// The value a flag or a positional takes, as argparse renders it.
fn value_display(argument: &Arg) -> Option<String> {
    if matches!(
        argument.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count | ArgAction::Help
    ) {
        return None;
    }
    argument
        .get_value_names()
        .and_then(<[clap::builder::Str]>::first)
        .map(|name| name.as_str().to_owned())
}

/// One argument as argparse spells it in a usage line: its first spelling and
/// the value it takes.
fn usage_form(argument: &Arg) -> String {
    let head = match (argument.get_short(), argument.get_long()) {
        (Some(short), _) => format!("-{short}"),
        (None, Some(long)) => format!("--{long}"),
        (None, None) => argument.get_id().to_string(),
    };
    match value_display(argument) {
        Some(value) => format!("{head} {value}"),
        None => head,
    }
}

/// Every spelling of one argument, joined the way a help block lists them.
fn help_form(argument: &Arg) -> String {
    if argument.is_positional() {
        return value_display(argument).unwrap_or_else(|| argument.get_id().to_string());
    }
    let value = value_display(argument);
    let mut spellings = Vec::new();
    if let Some(short) = argument.get_short() {
        spellings.push(format!("-{short}"));
    }
    if let Some(long) = argument.get_long() {
        spellings.push(format!("--{long}"));
    }
    for alias in argument.get_visible_aliases().unwrap_or_default() {
        spellings.push(format!("--{alias}"));
    }
    spellings
        .iter()
        .map(|spelling| match &value {
            Some(value) => format!("{spelling} {value}"),
            None => spelling.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The options of `command`, with the help flag first as argparse orders it.
fn options_of(command: &Command) -> Vec<&Arg> {
    let mut options: Vec<&Arg> = command
        .get_arguments()
        .filter(|argument| !argument.is_positional())
        .collect();
    options.sort_by_key(|argument| usize::from(argument.get_id() != "help"));
    options
}

/// argparse's usage line, on one line.
///
/// The reference wraps it at the terminal width; this port does not, because
/// nothing reads the wrapped shape and the wrapping rule is argparse's own
/// rather than a contract the two surfaces share.
pub(crate) fn usage_line(command: &Command) -> String {
    let mut parts = vec![format!("usage: {}", prog_of(command))];
    for argument in options_of(command) {
        parts.push(format!("[{}]", usage_form(argument)));
    }
    let names = subcommand_names(command);
    if !names.is_empty() {
        parts.push(format!("{{{}}} ...", names.join(",")));
    }
    for positional in command.get_positionals() {
        let form = help_form(positional);
        parts.push(if positional.is_required_set() {
            form
        } else {
            format!("[{form}]")
        });
    }
    parts.join(" ")
}

/// argparse's help column: two past the widest invocation, capped at 24.
fn help_position(entries: &[(usize, String, Option<String>)]) -> usize {
    let widest = entries
        .iter()
        .map(|(indent, invocation, _)| indent + invocation.chars().count())
        .max()
        .unwrap_or(0);
    (widest + 2).min(MAX_HELP_POSITION)
}

fn render_entry(
    out: &mut dyn Write,
    position: usize,
    indent: usize,
    invocation: &str,
    description: Option<&str>,
) {
    let padding = " ".repeat(indent);
    let Some(description) = description else {
        let _ = writeln!(out, "{padding}{invocation}");
        return;
    };
    if indent + invocation.chars().count() <= position.saturating_sub(2) {
        let width = position - indent - 2;
        let _ = writeln!(out, "{padding}{invocation:<width$}  {description}");
    } else {
        let _ = writeln!(out, "{padding}{invocation}");
        let _ = writeln!(out, "{}{description}", " ".repeat(position));
    }
}

/// argparse's help render: the usage line, the description, the positional
/// block and the options block.
pub(crate) fn print_help(command: &Command, stdout: &mut dyn Write) {
    let _ = writeln!(stdout, "{}", usage_line(command));
    if let Some(description) = command.get_long_about() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "{description}");
    }
    let mut positionals: Vec<(usize, String, Option<String>)> = Vec::new();
    let names = subcommand_names(command);
    if !names.is_empty() {
        positionals.push((2, format!("{{{}}}", names.join(",")), None));
        for sub in command.get_subcommands() {
            positionals.push((
                4,
                sub.get_name().to_owned(),
                sub.get_about().map(ToString::to_string),
            ));
        }
    }
    for positional in command.get_positionals() {
        positionals.push((
            2,
            help_form(positional),
            positional.get_help().map(ToString::to_string),
        ));
    }
    let options: Vec<(usize, String, Option<String>)> = options_of(command)
        .into_iter()
        .map(|argument| {
            let description = if argument.get_id() == "help" {
                Some("show this help message and exit".to_owned())
            } else {
                argument.get_help().map(ToString::to_string)
            };
            (2, help_form(argument), description)
        })
        .collect();
    let mut every = positionals.clone();
    every.extend(options.iter().cloned());
    let position = help_position(&every);
    for (heading, entries) in [
        ("positional arguments:", &positionals),
        ("options:", &options),
    ] {
        if entries.is_empty() {
            continue;
        }
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "{heading}");
        for (indent, invocation, description) in entries {
            render_entry(
                stdout,
                position,
                *indent,
                invocation,
                description.as_deref(),
            );
        }
    }
}
