//! Replays the committed CLI surface corpus against this port's own parsers.
//!
//! The corpus is captured by `tests/runtime-parity/cli-surface-oracle.py` from
//! the pinned reference's `vibe` binary, a black box since v3.0.0 moved the
//! client to Rust: every help render decomposed into its usage line,
//! subcommands, arguments and options, and every refused argument vector with
//! its exit code, its streams and the first line of each. The replay builds
//! the same views from this port's clap declarations and from driving each
//! vector through the reading the binary itself does, and ledgers every
//! difference under the bounded, typed rules of `vibe_core::parity::replay`.
//!
//! The startup vectors fail on the directories they name, which only a
//! process boundary shows, so `tests/startup_directory_failures.rs` replays
//! them against the binary and this module leaves them out.

use std::path::PathBuf;

use clap::{Arg, Command, CommandFactory};
use regex::Regex;
use serde_json::{Map, Value, json};
use vibe_core::parity::replay::{Band, Capture, OwnProse, Tally, differences, entries, scenarios};
use vibe_core::parity::{REFERENCE_COMMIT, REFERENCE_VERSION, is_pin_commit};

use crate::{Arguments, mcp_command};

/// The corpus, compiled in so the replay reads no path at run time.
const CORPUS: &str = include_str!("../tests/runtime-parity/cli-surface.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-cli/tests/runtime-parity/cli-surface.json";

const CAPTURE: Capture = Capture {
    script: "crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py",
    family: &["--corpus"],
    arguments: &[],
};

/// The accepted divergences the entries below stand on.
const OWN_DESCRIPTIONS: &str = "The CLI help's descriptions are this port's own";

/// This port's own descriptions, under every option, argument and command the
/// reference describes, within half to twice the reference's length.
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &["help/*"],
        pointer: "/description",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "7",
        accepted: OWN_DESCRIPTIONS,
        reason: "a command's help opens with this port's own description of it",
    },
    OwnProse {
        scenarios: &["help/*"],
        pointer: "/*/*/description",
        count: 30,
        band: Band::DEFAULT,
        directives: None,
        row: "7",
        accepted: OWN_DESCRIPTIONS,
        reason: "an option's or a subcommand's help line is this port's own sentence",
    },
];

// --------------------------------------------------------------------------
// The views both sides are compared in
// --------------------------------------------------------------------------

/// The reference's help record as the replay compares it: every list keyed by
/// the name it is found under, so a reordered or an added entry is one
/// difference and not a shift of all those after it.
fn reference_help(observed: &Value) -> Value {
    let keyed = |list: &Value, key: &dyn Fn(&Map<String, Value>) -> String| -> Value {
        Value::Object(
            list.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .map(|item| {
                    let mut item = item.clone();
                    let name = key(&item);
                    item.remove("name");
                    (name, Value::Object(item))
                })
                .collect(),
        )
    };
    let by_name = |item: &Map<String, Value>| item["name"].as_str().unwrap_or_default().to_owned();
    let by_flag = |item: &Map<String, Value>| {
        item["long"]
            .as_str()
            .or_else(|| item["short"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    json!({
        "description": observed["description"],
        "usage": observed["usage"],
        "commands": keyed(&observed["commands"], &by_name),
        "trailingCommands": keyed(&observed["trailingCommands"], &by_name),
        "arguments": keyed(&observed["arguments"], &by_name),
        "options": keyed(&observed["options"], &by_flag),
        "sections": observed["sections"],
    })
}

fn described(text: Option<String>) -> Value {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .map_or(Value::Null, |text| prose(&text))
}

/// A text this binary words itself, as the oracle records the reference's
/// (`prose` in `crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py`):
/// its length in code points and the SHA-256 of its UTF-8 bytes, under the
/// key an own-words entry reads as prose.
fn prose(text: &str) -> Value {
    json!({
        "prose": text.chars().count(),
        "sha256": vibe_core::text::sha256_hex(text.as_bytes()),
    })
}

fn help_text(argument: &Arg) -> Option<String> {
    argument
        .get_long_help()
        .or_else(|| argument.get_help())
        .map(ToString::to_string)
}

/// How clap spells a value in a help entry: `<NAME>`, `[<NAME>]` when it may
/// be left out, `...` after it when it repeats.
fn value_spelling(argument: &Arg) -> (Value, bool) {
    if !argument.get_action().takes_values() {
        return (Value::Null, false);
    }
    let name = argument
        .get_value_names()
        .and_then(|names| names.first())
        .map_or_else(
            || argument.get_id().to_string().to_uppercase(),
            ToString::to_string,
        );
    let range = argument.get_num_args().unwrap_or_default();
    let optional = range.min_values() == 0;
    let repeats = if range.max_values() > 1 { "..." } else { "" };
    let spelled = if optional {
        format!("[<{name}>]{repeats}")
    } else {
        format!("<{name}>{repeats}")
    };
    (json!(spelled), optional)
}

fn option_view(argument: &Arg) -> (String, Value) {
    let long = argument.get_long().map(|long| format!("--{long}"));
    let short = argument.get_short().map(|short| format!("-{short}"));
    let (value, optional) = value_spelling(argument);
    let mut view = Map::new();
    view.insert("short".to_owned(), json!(short));
    view.insert("long".to_owned(), json!(long));
    view.insert("value".to_owned(), value.clone());
    view.insert("optionalValue".to_owned(), json!(optional));
    // This port builds clap without its `env` feature, so no option of its
    // declaration reads a variable: where the reference annotates one, the
    // view differs by that key.
    if !value.is_null() && !argument.is_hide_default_value_set() {
        let defaults: Vec<String> = argument
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        if !defaults.is_empty() {
            view.insert("default".to_owned(), json!(defaults.join(", ")));
        }
    }
    if !value.is_null() && !argument.is_hide_possible_values_set() {
        let choices: Vec<String> = argument
            .get_possible_values()
            .iter()
            .filter(|choice| !choice.is_hide_set())
            .map(|choice| choice.get_name().to_owned())
            .collect();
        if !choices.is_empty() {
            view.insert("possibleValues".to_owned(), json!(choices));
        }
    }
    let aliases: Vec<String> = argument
        .get_visible_aliases()
        .into_iter()
        .flatten()
        .map(|alias| format!("--{alias}"))
        .collect();
    if !aliases.is_empty() {
        view.insert("aliases".to_owned(), json!(aliases));
    }
    view.insert("description".to_owned(), described(help_text(argument)));
    let key = long.or(short).unwrap_or_default();
    (key, Value::Object(view))
}

/// The declaration this port answers a command path with, built, so clap has
/// resolved its implicit help flag and help subcommand.
fn port_command(path: &[String]) -> Option<Command> {
    let words: Vec<&str> = path.iter().map(String::as_str).collect();
    let mut command = match words.first() {
        Some(&"mcp") => {
            let mut command = mcp_command::declaration();
            command.build();
            words[1..].iter().try_fold(command, |command, word| {
                command.find_subcommand(word).cloned()
            })?
        }
        _ => {
            let mut command = Arguments::command();
            command.build();
            words.iter().try_fold(command, |command, word| {
                command.find_subcommand(word).cloned()
            })?
        }
    };
    command.build();
    Some(command)
}

fn port_help(path: &[String]) -> Value {
    let Some(mut command) = port_command(path) else {
        return json!({"absent": true});
    };
    let usage = command.render_usage().to_string();
    let usage = usage.trim().trim_start_matches("Usage: ").to_owned();
    let mut options = Map::new();
    let mut arguments = Map::new();
    for argument in command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
    {
        if argument.is_positional() {
            let name = argument
                .get_value_names()
                .and_then(|names| names.first())
                .map_or_else(
                    || argument.get_id().to_string().to_uppercase(),
                    ToString::to_string,
                );
            let required = argument.is_required_set();
            let spelled = if required {
                format!("<{name}>")
            } else {
                format!("[{name}]")
            };
            arguments.insert(
                spelled,
                json!({"required": required, "description": described(help_text(argument))}),
            );
        } else {
            let (key, view) = option_view(argument);
            options.insert(key, view);
        }
    }
    let commands: Map<String, Value> = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| {
            (
                subcommand.get_name().to_owned(),
                json!({"description": described(subcommand.get_about().map(ToString::to_string))}),
            )
        })
        .collect();
    let mut sections = Vec::new();
    if !arguments.is_empty() {
        sections.push("Arguments");
    }
    if !commands.is_empty() {
        sections.push("Commands");
    }
    if !options.is_empty() {
        sections.push("Options");
    }
    json!({
        "description": described(
            command
                .get_long_about()
                .or_else(|| command.get_about())
                .map(ToString::to_string)
        ),
        "usage": usage,
        "commands": commands,
        "trailingCommands": {},
        "arguments": arguments,
        "options": options,
        "sections": sections,
    })
}

// --------------------------------------------------------------------------
// Driving a vector
// --------------------------------------------------------------------------

/// The error lines clap words itself, which the corpus keeps in cleartext.
fn clap_templates() -> Vec<Regex> {
    [
        r"^error: unexpected argument '[^']*' found$",
        r"^error: a value is required for '[^']*' but none was supplied$",
        r"^error: invalid value '[^']*' for '[^']*'$",
        r"^error: the following required arguments were not provided:$",
        r"^error: unrecognized subcommand '[^']*'$",
        r"^error: the argument '[^']*' cannot be used with '[^']*'$",
        r"^error: the argument '[^']*' cannot be used multiple times$",
        r"^error: '[^']*' requires a subcommand but one was not provided$",
        r"^Usage: [^ ].*$",
        r"^vibe [0-9][0-9A-Za-z.+-]*$",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("a template compiles"))
    .collect()
}

fn first_line(stream: &str) -> Option<&str> {
    stream.lines().find(|line| !line.trim().is_empty())
}

fn line_record(templates: &[Regex], line: Option<&str>) -> Value {
    match line {
        None => Value::Null,
        Some(line) if templates.iter().any(|template| template.is_match(line)) => json!(line),
        Some(line) => prose(line),
    }
}

/// One root vector, read the way the binary reads it
/// (`crates/vibe-cli/src/main.rs`): a vector that parses would start a session,
/// which the reference refuses without a terminal, so it is recorded as
/// parsed and writing nothing.
fn drive_root(argv: &[String]) -> (i32, String, String) {
    let full: Vec<String> = std::iter::once("vibe".to_owned())
        .chain(argv.iter().cloned())
        .collect();
    match crate::argv::parse_arguments(full) {
        Ok(_) => (0, String::new(), String::new()),
        Err(failure) if failure.use_stderr => {
            (i32::from(failure.exit), String::new(), failure.rendered)
        }
        Err(failure) => (i32::from(failure.exit), failure.rendered, String::new()),
    }
}

/// A credential store that answers every deletion, so a replayed `remove`
/// never reaches the developer's own keyring.
struct AbsentKeyring;

impl vibe_core::auth::KeyringBackend for AbsentKeyring {
    fn get(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<String>, vibe_core::auth::KeyringFailure> {
        Ok(None)
    }

    fn set(
        &self,
        _service: &str,
        _account: &str,
        _secret: &str,
    ) -> Result<(), vibe_core::auth::KeyringFailure> {
        Ok(())
    }

    fn delete(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<(), vibe_core::auth::KeyringFailure> {
        Ok(())
    }
}

/// A login the replay never lets run: every recorded `add` vector fails
/// before one could start.
struct RefusingLogin;

impl mcp_command::McpOAuthLogin for RefusingLogin {
    fn login<'a>(
        &'a self,
        _authentication: &'a vibe_core::mcp::McpAuthenticationService,
        _name: &'a str,
        _on_url: vibe_core::auth::AuthUrlSink,
    ) -> mcp_command::LoginFuture<'a, ()> {
        Box::pin(async { Err("the replay must never start an OAuth login".to_owned()) })
    }

    fn open(&self, _url: &str) -> Result<(), String> {
        Err("the replay must never open a browser".to_owned())
    }
}

/// One `vibe mcp` vector, against a temporary home so the developer's own
/// configuration is never read.
fn drive_mcp(argv: &[String]) -> (i32, String, String) {
    let root = tempfile::tempdir().expect("a temporary home for the replay");
    let vibe_home: PathBuf = root.path().join("vibe-home");
    let workspace: PathBuf = root.path().join("workspace");
    std::fs::create_dir_all(&vibe_home).expect("the temporary home");
    std::fs::create_dir_all(&workspace).expect("the temporary workspace");
    let environment = mcp_command::McpEnvironment::for_home(
        &vibe_home,
        &workspace,
        std::sync::Arc::new(AbsentKeyring),
        Box::new(RefusingLogin),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the replay")
        .block_on(mcp_command::run(
            argv,
            &environment,
            &mut stdout,
            &mut stderr,
        ));
    (
        i32::from(exit),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn port_case(templates: &[Regex], argv: &[String]) -> Value {
    let (exit, stdout, stderr) = match argv.first().map(String::as_str) {
        Some("mcp") => drive_mcp(&argv[1..]),
        _ => drive_root(argv),
    };
    let renders_help = matches!(argv.last().map(String::as_str), Some("--help" | "-h"));
    json!({
        "exit": exit,
        "stdout": !stdout.is_empty(),
        "stderr": !stderr.is_empty(),
        "stdoutFirstLine": if renders_help {
            Value::Null
        } else {
            line_record(templates, first_line(&stdout))
        },
        "stderrFirstLine": line_record(templates, first_line(&stderr)),
    })
}

fn words(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_records_the_pinned_binary() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let commit = corpus["reference"]["commit"].as_str().unwrap_or_default();
    assert!(is_pin_commit(commit), "the corpus records {commit}");
    if commit == REFERENCE_COMMIT {
        assert_eq!(corpus["reference"]["version"], REFERENCE_VERSION);
    }
    let recorded = scenarios(&corpus);
    assert!(
        recorded.contains_key("help/vibe"),
        "the root help is recorded"
    );
    assert!(
        recorded
            .keys()
            .any(|name| name.starts_with("case/startup/")),
        "the startup vectors are recorded"
    );
}

#[test]
fn the_port_answers_every_surface_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let templates = clap_templates();
    let mut tally = Tally::new(CORPUS, entries(PROSE));
    for (name, scenario) in scenarios(&corpus) {
        // Replayed at the process boundary by `tests/startup_directory_failures.rs`.
        if name.starts_with("case/startup/") {
            continue;
        }
        let (reference, port) = if name.starts_with("help/") {
            let path = words(&scenario["scenario"]["command"]);
            (reference_help(&scenario["observed"]), port_help(&path))
        } else {
            let argv = words(&scenario["scenario"]["argv"]);
            (scenario["observed"].clone(), port_case(&templates, &argv))
        };
        let found = differences(&reference, &port);
        tally.record(&name, &reference, &port, found);
    }
    tally.conclude("cli surface parity:");
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("cli surface");
}

#[test]
fn own_words_and_ledger_entries_are_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    vibe_core::parity::replay::assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}
