//! `vibe mcp`: the sub-command surface, its help renders and its error grammar.
//!
//! Reference `vibe/cli/mcp_command.py`. The reference builds three argparse
//! parsers, dispatches on the sub-command name, and funnels every post-parse
//! failure back through `parser.error`, which prints the usage line, one
//! `{prog}: error: {message}` line, and exits 2. This module reproduces that
//! shape over a clap declaration: clap owns the argument surface, so
//! `crate::cli_surface_parity_tests` can read it back and compare it against
//! the recorded argparse actions, while the usage line, the help render and the
//! error grammar are written here because clap spells all three differently.
//!
//! The intercept runs before the top-level parser (`crates/vibe-cli/src/main.rs`),
//! which is where the reference puts its own (`vibe/cli/entrypoint.py:258`), so
//! `mcp` is never read as a positional prompt.
//!
//! Every persistence path opens a user-scope store, matching the user-only
//! harness the reference builds for this command (`mcp_command.py:383-390`):
//! `vibe mcp remove` never edits a project configuration.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgAction, Command};
use toml::Table;
use vibe_core::auth::{KeyringBackend, NativeKeyringBackend, delete_mcp_oauth_credential};
use vibe_core::config::{ConfigPaths, ConfigSource, LayeredConfig};

/// The program name every usage line and every error line carries.
const PROG: &str = "vibe mcp";

/// The exit code an argument failure carries, which is argparse's own.
const USAGE_EXIT: u8 = 2;

/// argparse's help column: two past the widest invocation, capped.
const MAX_HELP_POSITION: usize = 24;

/// Whether `arguments` is a `vibe mcp` invocation, which is decided before the
/// interactive parser sees them because `mcp` is not a prompt.
#[must_use]
pub fn intercepts(arguments: &[String]) -> bool {
    arguments.first().map(String::as_str) == Some("mcp")
}

// --------------------------------------------------------------------------
// The declaration
// --------------------------------------------------------------------------

/// The `vibe mcp` parser, its two sub-commands and their arguments.
///
/// The value names are the ones argparse renders: a declared `metavar` where
/// the reference declares one, the choice set where it declares choices, and
/// the uppercased destination otherwise.
#[must_use]
pub(crate) fn declaration() -> Command {
    let mut command = Command::new(PROG)
        // The description is the block a help render prints under its usage
        // line. Only the root parser declares one: the reference passes
        // `description` to `ArgumentParser` and only `help` to `add_parser`,
        // so a sub-command's blurb is listed by its parent and never printed
        // again as its own description.
        .long_about("Configure the MCP servers a session may reach.")
        .disable_help_subcommand(true)
        .subcommand(add_declaration())
        .subcommand(remove_declaration());
    command.build();
    command
}

fn add_declaration() -> Command {
    Command::new("add")
        .about("Store an MCP server in the user configuration.")
        .arg(
            Arg::new("name")
                .value_name("NAME")
                .required(true)
                .help("Name the server is configured under."),
        )
        .arg(
            Arg::new("transport")
                .long("transport")
                .value_name("{http,streamable-http,stdio}")
                .value_parser(["http", "streamable-http", "stdio"])
                .default_value("streamable-http")
                .help("Wire protocol the server speaks."),
        )
        .arg(
            Arg::new("url")
                .long("url")
                .value_name("URL")
                .help("Endpoint a remote transport connects to."),
        )
        .arg(
            Arg::new("command")
                .long("command")
                .value_name("COMMAND")
                .help("Executable a stdio server is launched from."),
        )
        .arg(
            Arg::new("arg")
                .long("arg")
                .value_name("VALUE")
                .action(ArgAction::Append)
                .help("Argument passed to the stdio command; repeatable."),
        )
        .arg(
            Arg::new("env")
                .long("env")
                .value_name("NAME=VALUE")
                .action(ArgAction::Append)
                .help("Variable set for the stdio command; repeatable."),
        )
        .arg(
            Arg::new("header")
                .long("header")
                .value_name("NAME=VALUE")
                .action(ArgAction::Append)
                .help("Header sent with every request; repeatable."),
        )
        .arg(
            Arg::new("api_key_env")
                .long("api-key-env")
                .visible_alias("bearer-token-env-var")
                .value_name("VAR")
                .help("Variable holding the API key."),
        )
        .arg(
            Arg::new("api_key_header")
                .long("api-key-header")
                .value_name("HEADER")
                .help("Header the API key is sent in."),
        )
        .arg(
            Arg::new("api_key_format")
                .long("api-key-format")
                .value_name("FORMAT")
                .help("Template the API key is formatted with."),
        )
        .arg(
            Arg::new("no_login")
                .long("no-login")
                .action(ArgAction::SetTrue)
                .help("Store the server without an OAuth login."),
        )
        .arg(
            Arg::new("startup_timeout_sec")
                .long("startup-timeout-sec")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(f64))
                .help("Seconds allowed for the server to start."),
        )
        .arg(
            Arg::new("tool_timeout_sec")
                .long("tool-timeout-sec")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(f64))
                .help("Seconds allowed for one tool call."),
        )
}

fn remove_declaration() -> Command {
    Command::new("remove")
        .about("Drop an MCP server from the user configuration.")
        .arg(
            Arg::new("name")
                .value_name("NAME")
                .required(true)
                .help("Name of the server to drop."),
        )
}

// --------------------------------------------------------------------------
// The environment a run resolves against
// --------------------------------------------------------------------------

/// Everything `vibe mcp` touches outside its own argument vector.
///
/// Both halves are injected rather than resolved inside the command, because a
/// replay that drove the real ones would read, and on a hit rewrite, the
/// developer's own configuration and credential store.
pub struct McpEnvironment {
    vibe_home: PathBuf,
    working_directory: PathBuf,
    keyring: Box<dyn KeyringBackend>,
}

impl McpEnvironment {
    /// The environment the installed binary runs against.
    #[must_use]
    pub fn from_process() -> Self {
        let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vibe_home =
            crate::tui::startup::workspace_paths_for(None, &working_directory).vibe_home;
        Self {
            vibe_home,
            working_directory,
            keyring: Box::new(NativeKeyringBackend::new()),
        }
    }

    /// An environment over a named home and credential store, for the replay
    /// and the unit tests.
    ///
    /// Only a test builds one: the binary always resolves the real home, and a
    /// replay that did the same would read, and on a hit rewrite, the
    /// developer's own configuration.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_home(
        vibe_home: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        keyring: Box<dyn KeyringBackend>,
    ) -> Self {
        Self {
            vibe_home: vibe_home.into(),
            working_directory: working_directory.into(),
            keyring,
        }
    }

    /// The user-scope store every persistence path here writes through.
    fn store(&self) -> LayeredConfig {
        LayeredConfig::new(
            ConfigPaths {
                vibe_home: self.vibe_home.clone(),
                working_directory: self.working_directory.clone(),
            },
            Table::new(),
        )
        .with_sources(BTreeSet::from([ConfigSource::User]))
    }
}

// --------------------------------------------------------------------------
// The run
// --------------------------------------------------------------------------

/// Runs `vibe mcp <subcommand>` and answers with the process exit code.
///
/// The dispatch is argparse's: the first token names the sub-command, a token
/// the parser does not declare is a choice failure against `mcp_command`, and
/// anything the sub-command could not place is reported by the root parser.
#[must_use]
pub fn run(
    arguments: &[String],
    environment: &McpEnvironment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let root = declaration();
    match arguments.split_first() {
        None => print_help(&root, stdout),
        Some((first, _)) if first == "-h" || first == "--help" => print_help(&root, stdout),
        Some((first, rest)) if root.find_subcommand(first.as_str()).is_some() => {
            let Some(sub) = root.find_subcommand(first.as_str()).cloned() else {
                return USAGE_EXIT;
            };
            dispatch(&root, sub, first, rest, environment, stdout, stderr)
        }
        Some((first, _)) if first.starts_with('-') => {
            fail(&root, PROG, &unrecognized(first), stderr)
        }
        Some((first, _)) => fail(
            &root,
            PROG,
            &format!(
                "argument mcp_command: invalid choice: '{first}' (choose from {})",
                subcommand_names(&root).join(", ")
            ),
            stderr,
        ),
    }
}

fn dispatch(
    root: &Command,
    sub: Command,
    name: &str,
    rest: &[String],
    environment: &McpEnvironment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let prog = format!("{PROG} {name}");
    let argv = std::iter::once(prog.clone()).chain(rest.iter().cloned());
    let matches = match sub.clone().try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return print_help(&sub, stdout);
        }
        Err(error) => {
            let (scope, message) = translate(&error);
            return match scope {
                Scope::Root => fail(root, PROG, &message, stderr),
                Scope::Sub => fail(&sub, &prog, &message, stderr),
            };
        }
    };
    match name {
        "remove" => {
            let requested = matches
                .get_one::<String>("name")
                .cloned()
                .unwrap_or_default();
            match remove(&requested, environment) {
                Ok(message) => write_line(stdout, &message),
                Err(message) => fail(&sub, &prog, &message, stderr),
            }
        }
        // `add` is EP-100's: the surface is declared so the parser answers the
        // same shapes, and the operation itself still refuses.
        _ => {
            let _ = writeln!(
                stderr,
                "vibe mcp add is not implemented by this runtime; add a server with `/mcp add <url>` in the session"
            );
            1
        }
    }
}

/// Deletes the credentials first and the configuration entry second.
///
/// Reference `remove_mcp_server_and_credentials` and the reason it states: a
/// credential deletion can fail on a locked keyring, so doing it first leaves
/// the configuration untouched and needs no restore, and the write that races
/// other writers happens last.
fn remove(name: &str, environment: &McpEnvironment) -> Result<String, String> {
    let store = environment.store();
    if let Some(resource) = store.persisted_oauth_mcp_server(name, &environment.working_directory) {
        delete_mcp_oauth_credential(environment.keyring.as_ref(), &resource).map_err(
            |failure| format!("could not delete the OAuth credentials for `{name}`: {failure}"),
        )?;
    }
    let removal = store
        .persist_mcp_remove(name)
        .map_err(|error| error.to_string())?;
    Ok(if removal.removed {
        format!("Removed MCP server `{}`.", removal.name)
    } else {
        format!(
            "MCP server `{}` is not configured in the user config.",
            removal.name
        )
    })
}

// --------------------------------------------------------------------------
// The error grammar
// --------------------------------------------------------------------------

/// Which parser reports a failure, which is what decides the prog it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Root,
    Sub,
}

/// Reads one clap failure as the argparse message the reference prints.
fn translate(error: &clap::Error) -> (Scope, String) {
    match error.kind() {
        ErrorKind::MissingRequiredArgument => (
            Scope::Sub,
            format!(
                "the following arguments are required: {}",
                context(error, ContextKind::InvalidArg).join(", ")
            ),
        ),
        ErrorKind::InvalidValue => {
            let flag = flag(error);
            let value = context(error, ContextKind::InvalidValue).join(", ");
            if value.is_empty() {
                return (
                    Scope::Sub,
                    format!("argument {flag}: expected one argument"),
                );
            }
            (
                Scope::Sub,
                format!(
                    "argument {flag}: invalid choice: '{value}' (choose from {})",
                    context(error, ContextKind::ValidValue).join(", ")
                ),
            )
        }
        // Only the two timeout options parse a value, so a value this port
        // accepted the shape of but could not read is always a float.
        ErrorKind::ValueValidation => (
            Scope::Sub,
            format!(
                "argument {}: invalid float value: '{}'",
                flag(error),
                context(error, ContextKind::InvalidValue).join(", ")
            ),
        ),
        // argparse places what no parser could consume on the root parser,
        // which is why an extra positional names `vibe mcp` and not the
        // sub-command that was actually running.
        ErrorKind::UnknownArgument | ErrorKind::TooManyValues => (
            Scope::Root,
            unrecognized(&context(error, ContextKind::InvalidArg).join(" ")),
        ),
        _ => (
            Scope::Sub,
            error
                .to_string()
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ")
                .to_owned(),
        ),
    }
}

fn unrecognized(extras: &str) -> String {
    format!("unrecognized arguments: {extras}")
}

/// The flag a failure names, without the value name clap appends to it.
fn flag(error: &clap::Error) -> String {
    context(error, ContextKind::InvalidArg)
        .first()
        .and_then(|argument| argument.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default()
}

/// One context entry, as the plain strings argparse would print.
fn context(error: &clap::Error, kind: ContextKind) -> Vec<String> {
    let strip = |value: &str| value.trim_matches(|c| c == '<' || c == '>').to_owned();
    match error.get(kind) {
        Some(ContextValue::String(value)) => vec![strip(value)],
        Some(ContextValue::Strings(values)) => values.iter().map(|value| strip(value)).collect(),
        Some(ContextValue::Number(value)) => vec![value.to_string()],
        _ => Vec::new(),
    }
}

/// argparse's `parser.error`: the usage line, one message line, exit 2.
fn fail(command: &Command, prog: &str, message: &str, stderr: &mut dyn Write) -> u8 {
    let _ = writeln!(stderr, "{}", usage_line(command));
    let _ = writeln!(stderr, "{prog}: error: {message}");
    USAGE_EXIT
}

fn write_line(stdout: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(stdout, "{message}");
    0
}

// --------------------------------------------------------------------------
// The renders
// --------------------------------------------------------------------------

fn subcommand_names(command: &Command) -> Vec<String> {
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
fn usage_line(command: &Command) -> String {
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
fn print_help(command: &Command, stdout: &mut dyn Write) -> u8 {
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
    if !positionals.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "positional arguments:");
        for (indent, invocation, description) in &positionals {
            render_entry(
                stdout,
                position,
                *indent,
                invocation,
                description.as_deref(),
            );
        }
    }
    if !options.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "options:");
        for (indent, invocation, description) in &options {
            render_entry(
                stdout,
                position,
                *indent,
                invocation,
                description.as_deref(),
            );
        }
    }
    0
}

#[cfg(test)]
mod mcp_command_tests;
