//! The `vibe-acp` command line, read the way the reference's argparse parser
//! reads it (`vibe/acp/entrypoint.py`, `parse_arguments`): long options match
//! by unique prefix, short flags may be bundled, `--help` and `--version`
//! answer as soon as they are read, and any other failure prints the usage and
//! exits with status 2.

use std::ffi::OsString;

/// What the process was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Arguments {
    pub(crate) setup: bool,
    pub(crate) experimental_harness: bool,
    pub(crate) legacy_harness: bool,
}

/// A parse that ends the process before the server starts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    /// Printed to stdout, status 0.
    Print(String),
    /// Printed to stderr, status 2.
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    Help,
    Version,
    Setup,
    ExperimentalHarness,
    LegacyHarness,
}

const LONG: [(&str, Flag); 5] = [
    ("--help", Flag::Help),
    ("--version", Flag::Version),
    ("--setup", Flag::Setup),
    ("--experimental-harness", Flag::ExperimentalHarness),
    ("--legacy-harness", Flag::LegacyHarness),
];

pub(crate) fn parse(argv: impl IntoIterator<Item = OsString>) -> Result<Arguments, Exit> {
    let mut argv = argv.into_iter();
    let prog = argv
        .next()
        .and_then(|first| {
            std::path::Path::new(&first)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "vibe-acp".to_owned());
    let mut arguments = Arguments::default();
    let mut unrecognized = Vec::new();
    let mut positional_only = false;
    for raw in argv {
        let word = raw.to_string_lossy().into_owned();
        if positional_only || word == "-" || !word.starts_with('-') {
            unrecognized.push(word);
            continue;
        }
        if word == "--" {
            // argparse keeps the separator among the arguments it rejects.
            positional_only = true;
            unrecognized.push(word);
            continue;
        }
        let flags = if let Some(long) = word.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (format!("--{name}"), Some(value.to_owned())),
                None => (word.clone(), None),
            };
            let matches = LONG
                .iter()
                .filter(|(option, _)| option.starts_with(&name))
                .collect::<Vec<_>>();
            let exact = matches.iter().find(|(option, _)| *option == name);
            let (option, flag) = match (exact, matches.as_slice()) {
                (Some(found), _) | (None, [found]) => **found,
                (None, []) => {
                    unrecognized.push(word);
                    continue;
                }
                (None, several) => {
                    let names = several
                        .iter()
                        .map(|(option, _)| *option)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(error(
                        &prog,
                        &format!("ambiguous option: {name} could match {names}"),
                    ));
                }
            };
            if let Some(value) = value {
                return Err(error(
                    &prog,
                    &format!(
                        "argument {}: ignored explicit argument '{value}'",
                        display(option)
                    ),
                ));
            }
            vec![flag]
        } else {
            // Every bundled short option exits, so the first letter decides.
            match word.chars().nth(1) {
                Some('h') => vec![Flag::Help],
                Some('v') => vec![Flag::Version],
                _ => {
                    unrecognized.push(word);
                    continue;
                }
            }
        };
        for flag in flags {
            match flag {
                Flag::Help => return Err(Exit::Print(help(&prog))),
                Flag::Version => {
                    return Err(Exit::Print(format!(
                        "{prog} {}\n",
                        env!("CARGO_PKG_VERSION")
                    )));
                }
                Flag::Setup => arguments.setup = true,
                Flag::ExperimentalHarness if arguments.legacy_harness => {
                    return Err(conflict(
                        &prog,
                        "--experimental-harness",
                        "--legacy-harness",
                    ));
                }
                Flag::LegacyHarness if arguments.experimental_harness => {
                    return Err(conflict(
                        &prog,
                        "--legacy-harness",
                        "--experimental-harness",
                    ));
                }
                Flag::ExperimentalHarness => arguments.experimental_harness = true,
                Flag::LegacyHarness => arguments.legacy_harness = true,
            }
        }
    }
    if !unrecognized.is_empty() {
        return Err(error(
            &prog,
            &format!("unrecognized arguments: {}", unrecognized.join(" ")),
        ));
    }
    Ok(arguments)
}

/// How argparse names an option in an error: every spelling it declares.
fn display(option: &str) -> &'static str {
    match option {
        "--help" => "-h/--help",
        "--version" => "-v/--version",
        "--setup" => "--setup",
        "--experimental-harness" => "--experimental-harness",
        _ => "--legacy-harness",
    }
}

fn usage(prog: &str) -> String {
    let indent = " ".repeat("usage: ".len() + prog.len() + 1);
    format!(
        "usage: {prog} [-h] [-v] [--setup]\n{indent}[--experimental-harness | --legacy-harness]\n"
    )
}

fn help(prog: &str) -> String {
    format!(
        "{}\nServe Mistral Vibe to an editor over the Agent Client Protocol\n\n\
         options:\n  \
         -h, --help            print this help, then exit\n  \
         -v, --version         print the version, then exit\n  \
         --setup               save an API key, then exit\n  \
         --experimental-harness\n                        \
         run sessions on the Unified Harness backend\n  \
         --legacy-harness      run sessions on the legacy harness\n",
        usage(prog)
    )
}

fn error(prog: &str, message: &str) -> Exit {
    Exit::Error(format!("{}{prog}: error: {message}\n", usage(prog)))
}

fn conflict(prog: &str, option: &str, other: &str) -> Exit {
    error(
        prog,
        &format!("argument {option}: not allowed with argument {other}"),
    )
}
