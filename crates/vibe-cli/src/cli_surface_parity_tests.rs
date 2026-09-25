//! Replays the committed CLI surface corpus against this port's own parser.
//!
//! The corpus is captured by `tests/runtime-parity/cli-surface-oracle.py` from
//! the pinned reference. It is replayed here unconditionally, so a flag that
//! moves fails `cargo test` on a machine that holds no reference checkout; only
//! the live probe, which re-captures and compares byte for byte, needs one.
//!
//! Every difference the replay finds has to be named by the ledger below, and
//! every ledger entry has to still reproduce. Row 7 of `docs/parity.md` is then
//! a reading of the summary line this file prints rather than a count someone
//! made by hand.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::{Arg, ArgAction, Command, CommandFactory};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use vibe_core::parity::{
    REFERENCE_COMMIT, REFERENCE_VERSION, off_pin_reason, pinned_interpreter, reference_root,
};

use crate::{Arguments, OutputMode, mcp_command};

/// The corpus, compiled in so the replay reads no path at run time.
const CORPUS: &str = include_str!("../tests/runtime-parity/cli-surface.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const SCHEMA_VERSION: u32 = 1;

/// The floors the corpus may not fall below. They are the same numbers the
/// capture prints, restated here so a corpus that lost coverage fails the suite
/// and not only the machine that recaptured it.
const CASE_FLOOR: usize = 185;
const ACTION_FLOOR: usize = 34;
const PARSER_FLOOR: usize = 3;

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    interpreter: Interpreter,
    capture: Capture,
    parsers: Vec<ParserRecord>,
    cases: Vec<Case>,
    unavailable: Vec<Unavailable>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
    version: String,
    source_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Interpreter {
    major: u32,
    minor: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Capture {
    columns: u32,
    files_opened_by_build: u32,
    session_home_created_by_build: bool,
    configuration_modules_imported_by_build: Vec<String>,
    session_home_created_by_entrypoint_import: bool,
    session_home_created_by_mcp_import: bool,
    real_user_configuration_untouched: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ParserRecord {
    parser: String,
    prog: String,
    formatter: String,
    allow_abbrev: bool,
    has_description: bool,
    has_epilog: bool,
    actions: Vec<ActionRecord>,
    action_groups: Vec<ActionGroup>,
    mutually_exclusive_groups: Vec<ExclusiveGroup>,
    help: HelpRecord,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionRecord {
    class: String,
    option_strings: Vec<String>,
    dest: String,
    nargs: Value,
    #[serde(rename = "const")]
    constant: Value,
    default: Value,
    choices: Option<Vec<String>>,
    metavar: Value,
    required: bool,
    #[serde(rename = "type")]
    value_type: Option<String>,
    invocation: String,
    help_suppressed: bool,
    help: Option<Described>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionGroup {
    title: String,
    dests: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExclusiveGroup {
    required: bool,
    dests: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HelpRecord {
    columns: u32,
    line_count: usize,
    sha256: String,
    lines: Vec<HelpLine>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HelpLine {
    kind: String,
    cleartext: Option<String>,
    described: Option<Described>,
    indent: usize,
    chars: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Described {
    marker: String,
    chars: usize,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Case {
    case: String,
    parser: String,
    argv: Vec<String>,
    exit: i32,
    streams: Streams,
    stdout_last_line: Option<StreamLine>,
    stderr_last_line: Option<StreamLine>,
    namespace: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Streams {
    stdout: bool,
    stderr: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StreamLine {
    cleartext: Option<String>,
    described: Option<Described>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Unavailable {
    id: String,
    argv: String,
    reason: String,
}

fn corpus() -> Corpus {
    serde_json::from_str(CORPUS).expect("the committed CLI surface corpus parses")
}

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// One difference between the reference and this port that the suite accepts,
/// named by the story that closes it.
///
/// `pointer` is matched as a prefix so a difference reported deeper than the
/// entry names is still covered, which is what lets one entry stand for a
/// parser this port does not declare at all.
struct Divergence {
    parser: &'static str,
    case: &'static str,
    pointer: &'static str,
    closed_by: &'static str,
    row: &'static str,
    why: &'static str,
}

impl Divergence {
    fn covers(&self, difference: &Difference) -> bool {
        self.parser == difference.parser
            && self.case == difference.case
            && difference.pointer.starts_with(self.pointer)
    }
}

#[derive(Debug)]
struct Difference {
    parser: String,
    case: String,
    pointer: String,
    reference: Value,
    port: Value,
}

impl Difference {
    fn new(parser: &str, case: &str, pointer: String, reference: Value, port: Value) -> Self {
        Self {
            parser: parser.to_owned(),
            case: case.to_owned(),
            pointer,
            reference,
            port,
        }
    }
}

// --------------------------------------------------------------------------
// Declarations
// --------------------------------------------------------------------------

/// The option spellings a clap argument answers to, in the reference's shape.
fn port_option_strings(argument: &Arg) -> Vec<String> {
    let mut strings = Vec::new();
    if let Some(short) = argument.get_short() {
        strings.push(format!("-{short}"));
    }
    if let Some(long) = argument.get_long() {
        strings.push(format!("--{long}"));
    }
    for alias in argument.get_visible_aliases().unwrap_or_default() {
        strings.push(format!("--{alias}"));
    }
    strings
}

fn matches_option(argument: &Arg, option: &str) -> bool {
    if let Some(long) = option.strip_prefix("--") {
        return argument.get_long() == Some(long)
            || argument
                .get_visible_aliases()
                .unwrap_or_default()
                .contains(&long);
    }
    let short = option.strip_prefix('-').and_then(|rest| {
        let mut characters = rest.chars();
        characters.next().filter(|_| characters.next().is_none())
    });
    short.is_some() && argument.get_short() == short
}

fn find_argument<'a>(command: &'a Command, action: &ActionRecord) -> Option<&'a Arg> {
    if action.option_strings.is_empty() {
        return command
            .get_positionals()
            .find(|argument| argument.get_id() == action.dest.as_str());
    }
    command.get_arguments().find(|argument| {
        action
            .option_strings
            .iter()
            .any(|option| matches_option(argument, option))
    })
}

/// The value count the reference's `nargs` declares, as clap's `(min, max)`.
fn reference_value_count(action: &ActionRecord) -> Value {
    match &action.nargs {
        Value::Null => json!([1, 1]),
        Value::Number(number) if number.as_u64() == Some(0) => json!([0, 0]),
        Value::String(nargs) if nargs == "?" => json!([0, 1]),
        other => json!(other),
    }
}

fn port_value_count(argument: &Arg) -> Value {
    argument.get_num_args().map_or(json!(null), |range| {
        let (minimum, maximum) = (range.min_values(), range.max_values());
        // The two surfaces spell an omissible positional differently: argparse
        // widens its `nargs` to `?`, clap keeps one value and drops the
        // requirement. Read clap's spelling in argparse's terms.
        if argument.is_positional() && !argument.is_required_set() && minimum == 1 {
            return json!([0, maximum]);
        }
        json!([minimum, maximum])
    })
}

/// The reference's declared default, normalized to how clap spells one.
///
/// argparse stores a default for every action, clap only where one was
/// declared, so the two agree on "no default" in three spellings: `None`, the
/// empty list an append action starts from, and no clap default at all.
fn reference_default(action: &ActionRecord) -> Value {
    match &action.default {
        Value::Null => Value::Null,
        // argparse's own marker for "keep this out of the namespace", which is
        // an implementation detail of the version action rather than a default.
        Value::String(marker) if marker == "==SUPPRESS==" => Value::Null,
        Value::Array(items) if items.is_empty() => Value::Null,
        Value::Bool(value) => json!(value.to_string()),
        Value::Number(number) => json!(number.to_string()),
        other => other.clone(),
    }
}

fn port_default(argument: &Arg) -> Value {
    let defaults: Vec<String> = argument
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    match defaults.len() {
        0 => Value::Null,
        1 => json!(defaults[0]),
        _ => json!(defaults),
    }
}

/// The values this port accepts, where accepting a value is what the argument
/// is for. clap gives a boolean flag the possible values `true` and `false`,
/// which is how it spells its own presence rather than a choice a caller makes.
fn port_choices(argument: &Arg) -> Option<Vec<String>> {
    if takes_no_value(argument) {
        return None;
    }
    let choices: Vec<String> = argument
        .get_possible_values()
        .iter()
        .map(|value| value.get_name().to_owned())
        .collect();
    (!choices.is_empty()).then_some(choices)
}

fn takes_no_value(argument: &Arg) -> bool {
    matches!(
        argument.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count | ArgAction::Help
    ) || argument
        .get_num_args()
        .is_some_and(|range| range.max_values() == 0)
}

/// The value name the reference's help prints for this action.
///
/// argparse derives it when the action declares no metavar: the choice set
/// where there are choices, the uppercased destination otherwise, and nothing
/// at all for an action that takes no value.
fn reference_value_name(action: &ActionRecord) -> Value {
    if !action.metavar.is_null() {
        return action.metavar.clone();
    }
    if reference_value_count(action) == json!([0, 0]) {
        return Value::Null;
    }
    match &action.choices {
        Some(choices) => json!(format!("{{{}}}", choices.join(","))),
        None => json!(action.dest.to_uppercase()),
    }
}

fn port_value_name(argument: &Arg) -> Value {
    if takes_no_value(argument) {
        return Value::Null;
    }
    match argument.get_value_names() {
        Some(names) if names.len() == 1 => json!(names[0].as_str()),
        Some([]) => Value::Null,
        Some(names) => json!(names.iter().map(|name| name.as_str()).collect::<Vec<_>>()),
        None => Value::Null,
    }
}

/// Compares every recorded action against the argument this port resolves for
/// it, and reports the arguments this port adds that the reference declares
/// nowhere.
fn port_parser(parser: &str) -> Option<Command> {
    // clap resolves an argument's value count, its default and the implicit
    // help flag while building, so an unbuilt command answers about half the
    // questions the corpus asks with `None`.
    match parser {
        "root" => {
            let mut command = Arguments::command();
            command.build();
            Some(command)
        }
        "mcp" => Some(mcp_command::declaration()),
        "mcp-add" => mcp_command::declaration().find_subcommand("add").cloned(),
        "mcp-remove" => mcp_command::declaration()
            .find_subcommand("remove")
            .cloned(),
        _ => None,
    }
}

fn compare_declarations(record: &ParserRecord, differences: &mut Vec<Difference>) {
    let Some(command) = port_parser(&record.parser) else {
        // A parser this port does not declare reports every action the
        // reference records as missing rather than as different.
        for action in &record.actions {
            differences.push(Difference::new(
                &record.parser,
                &record.parser,
                format!("/actions/{}", action.dest),
                json!(action.invocation),
                Value::Null,
            ));
        }
        return;
    };
    let mut matched: BTreeSet<String> = BTreeSet::new();
    for action in &record.actions {
        let case = action.dest.as_str();
        // The two surfaces model a sub-command differently: argparse declares
        // one positional whose choices are the sub-command names, clap
        // declares the sub-commands themselves. Compare the names.
        if action.class == "_SubParsersAction" {
            let reference: Vec<String> = action.choices.clone().unwrap_or_default();
            let port: Vec<String> = command
                .get_subcommands()
                .map(|sub| sub.get_name().to_owned())
                .collect();
            if reference != port {
                differences.push(Difference::new(
                    &record.parser,
                    case,
                    "/subcommands".to_owned(),
                    json!(reference),
                    json!(port),
                ));
            }
            continue;
        }
        let Some(argument) = find_argument(&command, action) else {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/present".to_owned(),
                json!(action.invocation),
                Value::Null,
            ));
            continue;
        };
        matched.insert(argument.get_id().to_string());
        let mut reference_options = action.option_strings.clone();
        reference_options.sort();
        let mut port_options = port_option_strings(argument);
        port_options.sort();
        if reference_options != port_options {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/optionStrings".to_owned(),
                json!(reference_options),
                json!(port_options),
            ));
        }
        let reference_count = reference_value_count(action);
        let port_count = port_value_count(argument);
        if reference_count != port_count {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/valueCount".to_owned(),
                reference_count,
                port_count,
            ));
        }
        let reference_value = reference_default(action);
        let port_value = port_default(argument);
        if reference_value != port_value {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/default".to_owned(),
                reference_value,
                port_value,
            ));
        }
        if action.choices != port_choices(argument) {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/choices".to_owned(),
                json!(action.choices),
                json!(port_choices(argument)),
            ));
        }
        let reference_name = reference_value_name(action);
        if reference_name != port_value_name(argument) {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/valueName".to_owned(),
                reference_name,
                port_value_name(argument),
            ));
        }
        if action.help_suppressed != argument.is_hide_set() {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/hidden".to_owned(),
                json!(action.help_suppressed),
                json!(argument.is_hide_set()),
            ));
        }
    }
    for argument in command.get_arguments() {
        let id = argument.get_id().to_string();
        if matched.contains(&id) {
            continue;
        }
        differences.push(Difference::new(
            &record.parser,
            &id,
            "/extraArgument".to_owned(),
            Value::Null,
            json!(port_option_strings(argument)),
        ));
    }
    compare_help(record, &command, differences);
}

/// The bare name an epilog entry opens with, read the way the capture reads
/// one: letters, digits, `_`, `*` and `-`, closed by two spaces or by the end
/// of the line, so no sentence can pass for a name.
fn epilog_name(stripped: &str) -> Option<&str> {
    let mut end = 0;
    for (index, character) in stripped.char_indices() {
        let allowed = if index == 0 {
            character.is_ascii_alphabetic() || character == '_'
        } else {
            character.is_ascii_alphanumeric()
                || character == '_'
                || character == '*'
                || character == '-'
        };
        if !allowed {
            break;
        }
        end = index + character.len_utf8();
    }
    let rest = stripped.get(end..)?;
    (end > 0 && (rest.is_empty() || rest.starts_with("  "))).then(|| &stripped[..end])
}

/// The headings and the entry names this port's epilog carries, in order.
fn port_epilog(command: &Command) -> (Vec<String>, Vec<String>) {
    let mut headings = Vec::new();
    let mut entries = Vec::new();
    let Some(epilog) = command.get_after_help() else {
        return (headings, entries);
    };
    let rendered = epilog.to_string();
    for line in rendered.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        if !line.starts_with(' ') && stripped.ends_with(':') {
            headings.push(line.to_owned());
        } else if let Some(name) = epilog_name(stripped) {
            entries.push(name.to_owned());
        }
    }
    (headings, entries)
}

/// Compares the help this port renders against the one the corpus records,
/// by structure rather than by text.
///
/// The two renders cannot be diffed: argparse and clap wrap their usage
/// blocks differently and title their sections differently, and the NOTICE
/// boundary forbids reproducing the reference's own sentences, so every
/// description here is written for this repository. What a reader compares the
/// two helps by is what is reproduced: which arguments are listed, in which
/// order, and which names the epilog blocks carry. The prose itself is
/// reported once as a difference the ledger accepts.
fn compare_help(record: &ParserRecord, command: &Command, differences: &mut Vec<Difference>) {
    let reference_order: Vec<String> = record
        .actions
        .iter()
        .filter(|action| !action.help_suppressed && action.class != "_SubParsersAction")
        .filter_map(|action| find_argument(command, action))
        .map(|argument| argument.get_id().to_string())
        .collect();
    let port_order: Vec<String> = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .map(|argument| argument.get_id().to_string())
        .collect();
    if reference_order != port_order {
        differences.push(Difference::new(
            &record.parser,
            "help",
            "/help/order".to_owned(),
            json!(reference_order),
            json!(port_order),
        ));
    }
    let reference_headings: Vec<&str> = record
        .help
        .lines
        .iter()
        .filter(|line| line.kind == "epilogHeading")
        .filter_map(|line| line.cleartext.as_deref())
        .collect();
    let reference_entries: Vec<&str> = record
        .help
        .lines
        .iter()
        .filter(|line| line.kind == "epilogEntry")
        .filter_map(|line| line.cleartext.as_deref())
        .collect();
    let (port_headings, port_entries) = port_epilog(command);
    if reference_headings != port_headings {
        differences.push(Difference::new(
            &record.parser,
            "help",
            "/help/epilogHeadings".to_owned(),
            json!(reference_headings),
            json!(port_headings),
        ));
    }
    if reference_entries != port_entries {
        differences.push(Difference::new(
            &record.parser,
            "help",
            "/help/epilogEntries".to_owned(),
            json!(reference_entries),
            json!(port_entries),
        ));
    }
    // The prose is reported once, for the parser whose help this port
    // publishes as its own front door. A sub-command's help reaches the ledger
    // through the `--help` case the corpus drives it with.
    let rendered = command.clone().render_help().to_string();
    let digest = hex::encode(Sha256::digest(rendered.as_bytes()));
    if record.parser == "root" && digest != record.help.sha256 {
        differences.push(Difference::new(
            &record.parser,
            "help",
            "/help/prose".to_owned(),
            json!(record.help.sha256),
            json!(digest),
        ));
    }
}

/// Every option the help lists says what it is for.
///
/// The reference describes all of them, so an option rendered with an empty
/// body is a hole in this port's help rather than a difference of prose.
#[test]
fn every_listed_option_carries_a_description() {
    let mut command = Arguments::command();
    command.build();
    let undescribed: Vec<String> = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .filter(|argument| {
            argument
                .get_help()
                .map(|help| help.to_string().trim().is_empty())
                .unwrap_or(true)
        })
        .map(|argument| argument.get_id().to_string())
        .collect();
    assert!(
        undescribed.is_empty(),
        "these arguments render an empty help body: {}",
        undescribed.join(", ")
    );
}

// --------------------------------------------------------------------------
// argv outcomes
// --------------------------------------------------------------------------

#[derive(Debug)]
struct Outcome {
    exit: i32,
    streams: Streams,
    namespace: Option<BTreeMap<String, Value>>,
    /// The last line each stream carried, which is what the corpus records
    /// either as cleartext or as a digest.
    stdout_last_line: Option<String>,
    stderr_last_line: Option<String>,
}

/// The 22 dest names of the reference's namespace, read off this port's parsed
/// arguments.
///
/// The two surfaces name three fields differently, which is a difference in
/// spelling and not in surface: `add_dir` is `add_directories` here, and both
/// `--resume` and `--prompt` answer a bare flag with a marker the other side
/// spells otherwise (`True` there, the empty string here).
fn port_namespace(arguments: &Arguments) -> BTreeMap<String, Value> {
    let output = match arguments.output {
        OutputMode::Text => "text",
        OutputMode::Json => "json",
        OutputMode::Streaming => "streaming",
    };
    let paths = |values: &[PathBuf]| {
        json!(
            values
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        )
    };
    BTreeMap::from([
        ("add_dir".to_owned(), paths(&arguments.add_directories)),
        ("agent".to_owned(), json!(arguments.agent)),
        ("auto_approve".to_owned(), json!(arguments.auto_approve)),
        ("check_upgrade".to_owned(), json!(arguments.check_upgrade)),
        (
            "continue_session".to_owned(),
            json!(arguments.continue_session),
        ),
        ("disabled_tools".to_owned(), json!(arguments.disabled_tools)),
        ("enabled_tools".to_owned(), json!(arguments.enabled_tools)),
        (
            "experimental_harness".to_owned(),
            json!(arguments.experimental_harness),
        ),
        ("legacy_harness".to_owned(), json!(arguments.legacy_harness)),
        ("smart_approve".to_owned(), json!(arguments.smart_approve)),
        ("initial_prompt".to_owned(), json!(arguments.initial_prompt)),
        ("max_price".to_owned(), json!(arguments.max_price)),
        ("max_tokens".to_owned(), json!(arguments.max_tokens)),
        ("max_turns".to_owned(), json!(arguments.max_turns)),
        ("output".to_owned(), json!(output)),
        ("prompt".to_owned(), json!(arguments.prompt)),
        ("resume".to_owned(), json!(arguments.resume)),
        ("setup".to_owned(), json!(arguments.setup)),
        ("teleport".to_owned(), json!(arguments.teleport)),
        ("trust".to_owned(), json!(arguments.trust)),
        (
            "workdir".to_owned(),
            json!(
                arguments
                    .workdir
                    .as_ref()
                    .map(|path| path.display().to_string())
            ),
        ),
        (
            "worktree".to_owned(),
            // A bare flag stores the reference's `const=True`.
            match &arguments.worktree {
                None => Value::Null,
                Some(None) => Value::Bool(true),
                Some(Some(name)) => json!(name),
            },
        ),
    ])
}

/// The reference namespace value, normalized to the shape this port stores.
///
/// An append action reports `None` when it never fired and this port reports an
/// empty list, and `--resume` without a value is `True` there against the empty
/// string here, which is the same "the flag was passed bare" in two spellings.
fn normalized_reference_value(dest: &str, value: &Value) -> Value {
    match dest {
        "add_dir" | "enabled_tools" | "disabled_tools" if value.is_null() => json!([]),
        "resume" if value == &json!(true) => json!(""),
        _ => value.clone(),
    }
}

/// One root vector, driven through the reading the binary itself does, so the
/// line the replay compares is the line the user is shown
/// (`crates/vibe-cli/src/main.rs:28`).
fn drive_root(argv: &[String]) -> Outcome {
    let full: Vec<String> = std::iter::once("vibe".to_owned())
        .chain(argv.iter().cloned())
        .collect();
    match crate::argv::parse_arguments(full) {
        Ok(arguments) => Outcome {
            exit: 0,
            streams: Streams {
                stdout: false,
                stderr: false,
            },
            namespace: Some(port_namespace(&arguments)),
            stdout_last_line: None,
            stderr_last_line: None,
        },
        Err(failure) => {
            let line = failure
                .rendered
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .map(str::to_owned);
            Outcome {
                exit: i32::from(failure.exit),
                streams: Streams {
                    stdout: !failure.use_stderr,
                    stderr: failure.use_stderr,
                },
                namespace: None,
                stdout_last_line: line.clone().filter(|_| !failure.use_stderr),
                stderr_last_line: line.filter(|_| failure.use_stderr),
            }
        }
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

fn last_line(stream: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stream);
    text.lines().next_back().map(ToOwned::to_owned)
}

/// A login the replay never lets run.
///
/// Every recorded `add` vector either declines the login or fails before one
/// could start; the one that would open a browser is in the corpus's
/// `unavailable` block, so reaching this is a defect in the replay.
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

/// The home the `vibe mcp` vectors run against.
///
/// The command goes through the configuration store, so the replay hands it a
/// temporary home rather than letting it resolve the ambient one: reading, and
/// on a hit rewriting, the developer's own `~/.vibe/config.toml` is not
/// something a test may do. One home serves every vector of a replay because
/// the capture sequenced them that way: the vectors that store a server run
/// before the ones that read it back, and a home per case would leave those
/// with nothing to find.
struct McpSession {
    _root: tempfile::TempDir,
    vibe_home: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

impl McpSession {
    fn open() -> Self {
        let root = tempfile::tempdir().expect("a temporary home for the replay");
        let vibe_home = root.path().join("vibe-home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&vibe_home).expect("the temporary home");
        std::fs::create_dir_all(&workspace).expect("the temporary workspace");
        Self {
            _root: root,
            vibe_home,
            workspace,
        }
    }

    /// Runs one `vibe mcp` vector and answers its exit code and both streams.
    fn run(&self, argv: &[String]) -> (u8, Vec<u8>, Vec<u8>) {
        let environment = mcp_command::McpEnvironment::for_home(
            &self.vibe_home,
            &self.workspace,
            std::sync::Arc::new(AbsentKeyring),
            Box::new(RefusingLogin),
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        // The command is async because its OAuth login is; the replay drives
        // it on a runtime of its own rather than becoming async itself.
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
        (exit, stdout, stderr)
    }

    fn drive(&self, argv: &[String]) -> Outcome {
        let (exit, stdout, stderr) = self.run(argv);
        Outcome {
            exit: i32::from(exit),
            streams: Streams {
                stdout: !stdout.is_empty(),
                stderr: !stderr.is_empty(),
            },
            namespace: None,
            stdout_last_line: last_line(&stdout),
            stderr_last_line: last_line(&stderr),
        }
    }
}

/// Whether the line this port printed is the one the corpus recorded.
///
/// The capture keeps a line cleartext when every word of it is argparse's own
/// and describes it otherwise, so a recorded line is either compared in full,
/// compared on the prefix argparse contributed, or compared by digest when the
/// whole sentence belongs to the reference.
fn stream_line_matches(recorded: &StreamLine, found: Option<&str>) -> bool {
    let found = found.unwrap_or_default();
    match (&recorded.cleartext, &recorded.described) {
        (Some(cleartext), None) => found == cleartext,
        (Some(prefix), Some(_)) => found.starts_with(prefix.as_str()),
        (None, Some(described)) => {
            hex::encode(Sha256::digest(found.as_bytes())) == described.sha256
        }
        (None, None) => found.is_empty(),
    }
}

/// Reports the stream lines this port disagrees with, without ever committing
/// the reference sentence a described line stands for.
fn compare_stream_lines(
    case: &Case,
    outcome: &Outcome,
    stdlib_sentences_only: bool,
    differences: &mut Vec<Difference>,
) {
    for (pointer, recorded, found) in [
        (
            "/stdoutLastLine",
            case.stdout_last_line.as_ref(),
            outcome.stdout_last_line.as_deref(),
        ),
        (
            "/stderrLastLine",
            case.stderr_last_line.as_ref(),
            outcome.stderr_last_line.as_deref(),
        ),
    ] {
        let Some(recorded) = recorded else {
            continue;
        };
        // A line the capture digested is a sentence the reference wrote, and
        // NOTICE keeps it out of this repository, so where only the standard
        // library's own templates are comparable the rest is left alone.
        if stdlib_sentences_only && recorded.described.is_some() {
            continue;
        }
        if stream_line_matches(recorded, found) {
            continue;
        }
        let expected = match (&recorded.cleartext, &recorded.described) {
            (Some(cleartext), None) => json!(cleartext),
            (cleartext, Some(described)) => json!({
                "cleartext": cleartext,
                "described": described.marker,
                "chars": described.chars,
            }),
            (None, None) => Value::Null,
        };
        differences.push(Difference::new(
            &case.parser,
            &case.case,
            pointer.to_owned(),
            expected,
            json!(found),
        ));
    }
}

fn compare_case(case: &Case, session: &McpSession, differences: &mut Vec<Difference>) {
    let outcome = match case.parser.as_str() {
        // A startup failure is decided after the parse, from the directory the
        // process sits in, and what it proves is which stream carried the
        // report. Neither survives an in-process drive, so these cases are
        // replayed against the binary by
        // `tests/startup_directory_failures.rs`.
        "startup" => return,
        "root" => drive_root(&case.argv),
        _ => session.drive(&case.argv),
    };
    if case.exit != outcome.exit {
        differences.push(Difference::new(
            &case.parser,
            &case.case,
            "/exit".to_owned(),
            json!(case.exit),
            json!(outcome.exit),
        ));
    }
    if case.streams != outcome.streams {
        differences.push(Difference::new(
            &case.parser,
            &case.case,
            "/streams".to_owned(),
            json!({"stdout": case.streams.stdout, "stderr": case.streams.stderr}),
            json!({"stdout": outcome.streams.stdout, "stderr": outcome.streams.stderr}),
        ));
    }
    // The root parser reports through two kinds of sentence: the reference's
    // own, which US-318 owns and the corpus only ever digested, and CPython's
    // `argparse` templates, which the corpus commits in cleartext because this
    // port reproduces them. Only the second kind is comparable here.
    compare_stream_lines(case, &outcome, case.parser == "root", differences);
    // A namespace is only comparable where both sides parsed; where one of them
    // refused the vector the exit code above already carries the difference.
    let (Some(reference), Some(port)) = (case.namespace.as_ref(), outcome.namespace.as_ref())
    else {
        return;
    };
    for (dest, value) in reference {
        let expected = normalized_reference_value(dest, value);
        let found = port.get(dest).cloned().unwrap_or(Value::Null);
        if expected != found {
            differences.push(Difference::new(
                &case.parser,
                &case.case,
                format!("/namespace/{dest}"),
                expected,
                found,
            ));
        }
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

fn replay(corpus: &Corpus) -> Vec<Difference> {
    let mut differences = Vec::new();
    for record in &corpus.parsers {
        compare_declarations(record, &mut differences);
    }
    // One home per replay rather than one per process: the tests that call
    // this may run concurrently, and a shared home would let them sequence
    // each other's servers.
    let session = McpSession::open();
    for case in &corpus.cases {
        compare_case(case, &session, &mut differences);
    }
    differences
}

#[test]
fn the_committed_corpus_carries_the_surface_the_replay_needs() {
    let corpus = corpus();
    assert_eq!(
        corpus.schema_version, SCHEMA_VERSION,
        "the corpus schema moved: expected version {SCHEMA_VERSION}, found {}",
        corpus.schema_version
    );
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.reference.version, REFERENCE_VERSION);
    assert!(
        corpus
            .reference
            .source_files
            .contains(&"vibe/cli/entrypoint.py".to_owned())
    );
    assert_eq!(corpus.interpreter.major, 3);
    assert!(corpus.interpreter.minor >= 12);
    assert_eq!(corpus.capture.columns, 80);
    // The capture proves the parsers are built without touching the machine it
    // ran on; a corpus that lost the proof is a corpus of something else.
    assert_eq!(corpus.capture.files_opened_by_build, 0);
    assert!(!corpus.capture.session_home_created_by_build);
    assert!(
        corpus
            .capture
            .configuration_modules_imported_by_build
            .is_empty()
    );
    assert!(!corpus.capture.session_home_created_by_entrypoint_import);
    assert!(corpus.capture.session_home_created_by_mcp_import);
    assert!(corpus.capture.real_user_configuration_untouched);

    assert!(
        corpus.parsers.len() >= PARSER_FLOOR,
        "{} parsers is below the floor of {PARSER_FLOOR}",
        corpus.parsers.len()
    );
    let actions: usize = corpus.parsers.iter().map(|p| p.actions.len()).sum();
    assert!(
        actions >= ACTION_FLOOR,
        "{actions} recorded actions is below the floor of {ACTION_FLOOR}"
    );
    assert!(
        corpus.cases.len() >= CASE_FLOOR,
        "{} argv cases is below the floor of {CASE_FLOOR}",
        corpus.cases.len()
    );

    let mut seen = BTreeSet::new();
    let mut startup = 0_usize;
    for case in &corpus.cases {
        assert!(
            seen.insert(case.case.clone()),
            "two cases share the id {}",
            case.case
        );
        assert!(matches!(case.parser.as_str(), "root" | "mcp" | "startup"));
        if case.parser == "startup" {
            startup += 1;
            // A startup vector is recorded for its exit code and its stream
            // alone: the report itself is a reference sentence, and the path
            // one of them prints is the capture's own temporary directory.
            assert_eq!(case.exit, 1, "the startup case {} did not fail", case.case);
            assert!(
                case.stdout_last_line.is_none() && case.stderr_last_line.is_none(),
                "the startup case {} carries a captured line",
                case.case
            );
        }
    }
    assert_eq!(
        startup, 6,
        "the corpus no longer carries the six startup failures \
         `tests/startup_directory_failures.rs` replays"
    );
    for entry in &corpus.unavailable {
        assert!(!entry.id.is_empty(), "an unavailable entry carries no id");
        assert!(
            !entry.argv.is_empty() && !entry.reason.is_empty(),
            "the unavailable entry {} carries no argv or reason",
            entry.id
        );
    }
    for record in &corpus.parsers {
        let declared: BTreeSet<&str> = record
            .actions
            .iter()
            .map(|action| action.dest.as_str())
            .collect();
        let grouped: BTreeSet<&str> = record
            .action_groups
            .iter()
            .flat_map(|group| group.dests.iter().map(String::as_str))
            .collect();
        assert_eq!(
            declared, grouped,
            "the {} groups do not partition its actions",
            record.parser
        );
        for action in &record.actions {
            assert!(
                action.class.starts_with('_') && action.class.ends_with("Action"),
                "{} is recorded with the class {}, which is not an argparse action",
                action.dest,
                action.class
            );
            // Only a sub-command's own positional is required. The root
            // parser asks for nothing, which is what lets a bare `vibe` start
            // a session, and `add_subparsers` leaves its choice optional,
            // which is what makes a bare `vibe mcp` print its help.
            let required = record.parser != "root"
                && action.option_strings.is_empty()
                && action.class != "_SubParsersAction";
            assert_eq!(
                action.required, required,
                "the requirement on {}/{} is not the one its position implies",
                record.parser, action.dest
            );
            // Two shapes store a value the caller never typed: a boolean
            // flag, and an optional whose `nargs` is `?` and which therefore
            // answers a bare occurrence with its const. A positional with the
            // same `nargs` falls back on its default instead.
            let stores_a_fixed_value = action.class == "_StoreTrueAction"
                || (action.nargs == json!("?") && !action.option_strings.is_empty());
            assert_eq!(
                !action.constant.is_null(),
                stores_a_fixed_value,
                "the const on {}/{} does not follow what it stores",
                record.parser,
                action.dest
            );
        }
    }
    let root = corpus
        .parsers
        .iter()
        .find(|record| record.parser == "root")
        .expect("the root parser is recorded");
    let typed: BTreeMap<&str, Option<&str>> = root
        .actions
        .iter()
        .map(|action| (action.dest.as_str(), action.value_type.as_deref()))
        .collect();
    assert_eq!(typed.get("max_turns"), Some(&Some("int")));
    assert_eq!(typed.get("max_tokens"), Some(&Some("int")));
    assert_eq!(typed.get("max_price"), Some(&Some("float")));
    assert_eq!(typed.get("prompt"), Some(&None));
    // The two bare-flag markers are not the same value, and the difference is
    // what makes `--prompt` alone an empty prompt and `--resume` alone a
    // request for the session picker.
    let constants: BTreeMap<&str, &Value> = root
        .actions
        .iter()
        .map(|action| (action.dest.as_str(), &action.constant))
        .collect();
    assert_eq!(constants.get("prompt"), Some(&&json!("")));
    assert_eq!(constants.get("resume"), Some(&&json!(true)));
    assert_eq!(root.prog, "vibe");
    assert!(root.allow_abbrev, "the reference infers long-flag prefixes");
    assert_eq!(root.formatter, "RawDescriptionHelpFormatter");
    assert!(root.has_description && root.has_epilog);
    assert_eq!(
        root.mutually_exclusive_groups.len(),
        2,
        "the harness group and the continuation group are the only exclusions the root \
         parser declares"
    );
    let harness = &root.mutually_exclusive_groups[0];
    assert!(!harness.required);
    assert_eq!(
        harness.dests,
        vec!["experimental_harness", "legacy_harness"]
    );
    let group = &root.mutually_exclusive_groups[1];
    assert!(!group.required);
    assert_eq!(group.dests, vec!["continue_session", "resume"]);
    assert_eq!(
        root.action_groups
            .iter()
            .map(|group| group.title.as_str())
            .collect::<Vec<_>>(),
        vec!["positional arguments", "options"]
    );
}

/// Every sentence the reference wrote is a digest, and every digest says so.
#[test]
fn the_corpus_carries_no_reference_authored_sentence() {
    let corpus = corpus();
    let mut digests = 0usize;
    let check = |described: &Described, at: &str| {
        assert_eq!(
            described.marker, "<described>",
            "the digest at {at} carries no marker"
        );
        assert_eq!(
            described.sha256.len(),
            64,
            "the digest at {at} is not a sha256"
        );
        assert!(described.chars > 0, "the digest at {at} describes nothing");
    };
    for record in &corpus.parsers {
        for action in &record.actions {
            if let Some(described) = &action.help {
                check(described, &action.dest);
                digests += 1;
            }
            // An invocation is flags and metavars, which the reference declared
            // rather than wrote, so it stays readable.
            assert!(
                !action.invocation.contains(". "),
                "the invocation for {} reads like a sentence",
                action.dest
            );
        }
        for (index, line) in record.help.lines.iter().enumerate() {
            if let Some(described) = &line.described {
                check(described, &format!("{}/{index}", record.parser));
                digests += 1;
            }
            if let Some(cleartext) = &line.cleartext {
                assert!(
                    matches!(
                        line.kind.as_str(),
                        "usage" | "heading" | "invocation" | "epilogHeading" | "epilogEntry"
                    ),
                    "a {} line carries cleartext at {}/{index}",
                    line.kind,
                    record.parser
                );
                assert!(
                    !cleartext.contains(". "),
                    "the cleartext at {}/{index} reads like a sentence",
                    record.parser
                );
            }
        }
    }
    let mut templates = 0usize;
    for case in &corpus.cases {
        for line in [&case.stdout_last_line, &case.stderr_last_line]
            .into_iter()
            .flatten()
        {
            if let Some(described) = &line.described {
                check(described, &case.case);
                digests += 1;
            }
        }
        // A root refusal is CPython's own message, which the capture keeps in
        // cleartext so US-312 and US-320 can reproduce it word for word.
        if case.parser == "root" && case.streams.stderr {
            let line = case
                .stderr_last_line
                .as_ref()
                .expect("a case that wrote to stderr recorded its last line");
            assert!(
                line.cleartext.is_some() && line.described.is_none(),
                "the refusal for {} is not readable, so nothing can reproduce it",
                case.case
            );
            templates += 1;
        }
    }
    assert!(
        templates >= 20,
        "{templates} readable refusals is too few to pin the failure surface"
    );
    assert!(
        digests > 100,
        "{digests} digests is too few for a surface this size"
    );
}

/// The help render is decomposed rather than summarized: the line count, the
/// indentation and the character count have to add up to a render the reference
/// could have printed.
#[test]
fn every_help_render_is_decomposed_line_by_line() {
    let corpus = corpus();
    for record in &corpus.parsers {
        let help = &record.help;
        assert_eq!(help.columns, 80);
        assert_eq!(help.line_count, help.lines.len());
        assert_eq!(help.sha256.len(), 64);
        assert!(
            help.lines
                .first()
                .is_some_and(|line| line.kind == "usage" && line.cleartext.is_some()),
            "the {} render does not open on its usage block",
            record.parser
        );
        for line in &help.lines {
            match line.kind.as_str() {
                "blank" => {
                    assert_eq!(line.chars, 0);
                    assert!(line.cleartext.is_none() && line.described.is_none());
                }
                "usage" | "heading" | "epilogHeading" => {
                    assert!(line.cleartext.is_some() && line.described.is_none());
                }
                "invocation" | "epilogEntry" => assert!(line.cleartext.is_some()),
                "description" | "continuation" | "epilogContinuation" => {
                    assert!(line.cleartext.is_none() && line.described.is_some());
                }
                other => panic!("the {} render carries a {other} line", record.parser),
            }
            assert!(line.chars >= line.indent);
        }
        let invocations = help
            .lines
            .iter()
            .filter(|line| line.kind == "invocation")
            .count();
        let visible = record
            .actions
            .iter()
            .filter(|action| !action.help_suppressed)
            .count();
        assert!(
            invocations >= visible,
            "the {} render prints {invocations} invocations for {visible} visible actions",
            record.parser
        );
    }
}

/// Every `vibe mcp` help render prints a description block exactly where the
/// reference prints one.
///
/// The description is the one help element the two surfaces can be compared on
/// directly: NOTICE forbids reproducing the reference's prose, so every other
/// line diverges on length, but whether a parser describes itself at all is
/// structure. argparse gives `ArgumentParser` a `description` and `add_parser`
/// only a `help`, so the root parser prints a description and neither
/// sub-command does; the corpus records that as `hasDescription`.
#[test]
fn only_the_parser_the_reference_describes_prints_a_description() {
    /// The blocks argparse opens after the usage line, which is what a line
    /// under it is when it is not a description.
    const HEADINGS: &[&str] = &["positional arguments:", "options:"];
    let corpus = corpus();
    let session = McpSession::open();
    for (parser, argv) in [
        ("mcp", &[][..]),
        ("mcp-add", &["add", "-h"][..]),
        ("mcp-remove", &["remove", "-h"][..]),
    ] {
        let record = corpus
            .parsers
            .iter()
            .find(|record| record.parser == parser)
            .unwrap_or_else(|| panic!("the {parser} parser is recorded"));
        let argv: Vec<String> = argv.iter().map(|token| (*token).to_owned()).collect();
        let (exit, stdout, stderr) = session.run(&argv);
        assert_eq!(exit, 0, "{parser} answered {exit} for its help");
        assert!(stderr.is_empty(), "{parser} wrote its help to stderr");
        let rendered = String::from_utf8(stdout).expect("the help render is UTF-8");
        let lines: Vec<&str> = rendered.lines().collect();
        let described = lines
            .iter()
            .skip_while(|line| !line.is_empty())
            .nth(1)
            .is_some_and(|line| !HEADINGS.contains(line));
        assert_eq!(
            described, record.has_description,
            "the {parser} render describes itself where the reference does not, or the reverse:\n{rendered}"
        );
    }
}

#[test]
fn every_ledger_entry_names_what_closes_it() {
    let rows: BTreeSet<&str> = SCORECARD
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|line| line.split_once(" |"))
        .map(|(number, _)| number.trim())
        .filter(|number| number.parse::<u32>().is_ok())
        .collect();
    assert!(
        !rows.is_empty(),
        "no numbered row was found in docs/parity.md"
    );
    for entry in LEDGER {
        assert!(
            entry.closed_by.starts_with("US-")
                || matches!(entry.closed_by, "RECORDED" | "ACCEPTED"),
            "the ledger entry {}/{} names {} as what closes it",
            entry.parser,
            entry.case,
            entry.closed_by
        );
        assert!(
            rows.contains(entry.row),
            "the ledger entry {}/{} names row {}, which docs/parity.md does not carry",
            entry.parser,
            entry.case,
            entry.row
        );
        assert!(
            entry.pointer.starts_with('/') && !entry.pointer.contains('*'),
            "the ledger entry {}/{} names the pointer {}, which is not one concrete pointer",
            entry.parser,
            entry.case,
            entry.pointer
        );
        assert!(
            entry.why.len() > 20,
            "the ledger entry {}/{} explains nothing",
            entry.parser,
            entry.case
        );
    }
}

#[test]
fn no_ledger_entry_outlives_the_difference_it_names() {
    let corpus = corpus();
    let differences = replay(&corpus);
    let stale: Vec<String> = LEDGER
        .iter()
        .filter(|entry| !differences.iter().any(|found| entry.covers(found)))
        .map(|entry| format!("{}/{} at {}", entry.parser, entry.case, entry.pointer))
        .collect();
    assert!(
        stale.is_empty(),
        "{} ledger entries no longer reproduce, so the divergence they record is closed: {}",
        stale.len(),
        stale.join(", ")
    );
}

#[test]
fn replaying_the_corpus_finds_no_difference_the_ledger_does_not_name() {
    let corpus = corpus();
    let differences = replay(&corpus);
    let unlisted: Vec<String> = differences
        .iter()
        .filter(|found| !LEDGER.iter().any(|entry| entry.covers(found)))
        .map(|found| {
            format!(
                "{}/{}{}: reference {} against {}",
                found.parser, found.case, found.pointer, found.reference, found.port
            )
        })
        .collect();
    assert!(
        unlisted.is_empty(),
        "{} differences no ledger entry names:\n  {}",
        unlisted.len(),
        unlisted.join("\n  ")
    );

    let actions: usize = corpus.parsers.iter().map(|p| p.actions.len()).sum();
    let exercised = LEDGER
        .iter()
        .filter(|entry| differences.iter().any(|found| entry.covers(found)))
        .count();
    let diverging: BTreeSet<&str> = differences
        .iter()
        .map(|found| found.case.as_str())
        .collect();
    let matched = corpus
        .cases
        .iter()
        .filter(|case| !diverging.contains(case.case.as_str()))
        .count();
    println!(
        "cli surface: {matched} of {} argv cases match this port, {actions} actions across \
         {} parsers replayed from {}, {exercised} ledger entries exercised",
        corpus.cases.len(),
        corpus.parsers.len(),
        &REFERENCE_COMMIT[..12],
    );
}

/// Recaptures the corpus against the local checkout and refuses any drift.
///
/// This is the only part of the replay that needs a reference: it skips with a
/// printed reason when the checkout is missing or sits at another commit, which
/// is the ordinary case on CI.
#[test]
fn the_committed_corpus_still_matches_a_fresh_capture() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "CLI surface") {
        eprintln!("{reason}");
        return;
    }
    let Some(interpreter) = pinned_interpreter(&root) else {
        eprintln!("skipping the live CLI surface probe: the checkout carries no interpreter");
        return;
    };
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let oracle = manifest.join("tests/runtime-parity/cli-surface-oracle.py");
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above the crate");
    let output = Process::new(&interpreter)
        .arg(&oracle)
        .arg("--check")
        .current_dir(workspace)
        .output()
        .expect("the pinned interpreter runs the CLI surface oracle");
    assert!(
        output.status.success(),
        "a fresh capture differs from the committed corpus:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// --------------------------------------------------------------------------
// The divergences this port still carries
// --------------------------------------------------------------------------

const LEDGER: &[Divergence] = &[
    Divergence {
        parser: "root",
        case: "tool_filters",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--allowed-tool exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "provider_style",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--provider-style exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "model",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--model exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "input_price",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--input-price exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "output_price",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--output-price exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "api_base",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--api-base exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "credential_environment",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--credential-environment exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "session_root",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--session-root exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "fake_response",
        pointer: "/extraArgument",
        closed_by: "ACCEPTED",
        row: "7",
        why: "--fake-response exists only in this port, is hidden from the help, and is kept because the runtime it drives has no counterpart upstream",
    },
    Divergence {
        parser: "root",
        case: "help",
        pointer: "/help/prose",
        closed_by: "ACCEPTED",
        row: "7",
        why: "NOTICE forbids reproducing the reference's own sentences, so every option description and every epilog sentence here is written for this repository and the two renders never carry the same text; the arguments and epilog names the two renders do not share are recorded by their own entries",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-help",
        pointer: "/stdoutLastLine",
        closed_by: "ACCEPTED",
        row: "7",
        why: "the last line of the `add` help carries an option description, and NOTICE forbids reproducing the reference's prose, so this port writes its own while the render's shape matches",
    },
];
