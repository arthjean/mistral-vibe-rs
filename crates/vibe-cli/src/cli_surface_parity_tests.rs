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
use vibe_core::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

use crate::{Arguments, OutputMode, mcp_command};

/// The corpus, compiled in so the replay reads no path at run time.
const CORPUS: &str = include_str!("../tests/runtime-parity/cli-surface.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const SCHEMA_VERSION: u32 = 1;

/// The floors the corpus may not fall below. They are the same numbers the
/// capture prints, restated here so a corpus that lost coverage fails the suite
/// and not only the machine that recaptured it.
const CASE_FLOOR: usize = 120;
const ACTION_FLOOR: usize = 34;
const PARSER_FLOOR: usize = 3;

/// The case ids the replay records without driving, and why.
///
/// `vibe mcp remove <name>` is the one shape this port answers by going through
/// the configuration store, and the store resolves its root from the ambient
/// environment (`crates/vibe-cli/src/tui/startup.rs:114-130`). Driving it here
/// would read, and on a hit rewrite, the developer's own `~/.vibe/config.toml`.
/// Setting the variable for the duration is not open to a test either, because
/// `std::env::set_var` is unsafe and this workspace forbids `unsafe_code`. So
/// the reference's answer is recorded and this port's is left to US-313, which
/// gives the command an injectable store.
///
/// The list is audited against the shape it claims to describe by
/// `every_case_that_reaches_the_configuration_store_is_named`, so a new
/// `remove` vector in the corpus fails the suite rather than silently running
/// against a real home.
const UNDRIVEN: &[&str] = &[
    "mcp-remove-help",
    "mcp-remove-absent",
    "mcp-remove-stdio-server",
];

const UNDRIVEN_REASON: &str = "this port's removal path resolves the session home from the \
     ambient environment, so replaying it would read and could rewrite the developer's own user \
     configuration";

/// Whether driving this case would reach the configuration store.
fn reaches_the_configuration_store(case: &Case) -> bool {
    case.parser == "mcp" && matches!(case.argv.as_slice(), [command, _] if command == "remove")
}

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
fn compare_declarations(record: &ParserRecord, differences: &mut Vec<Difference>) {
    if record.parser != "root" {
        // This port declares no `vibe mcp` parser at all, so every action it
        // records is missing rather than different.
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
    }
    // clap resolves an argument's value count, its default and the implicit
    // help flag while building, so an unbuilt command answers about half the
    // questions the corpus asks with `None`.
    let mut command = Arguments::command();
    command.build();
    let mut matched: BTreeSet<String> = BTreeSet::new();
    for action in &record.actions {
        let case = action.dest.as_str();
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
        if action.metavar != port_value_name(argument) {
            differences.push(Difference::new(
                &record.parser,
                case,
                "/valueName".to_owned(),
                action.metavar.clone(),
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
}

// --------------------------------------------------------------------------
// argv outcomes
// --------------------------------------------------------------------------

#[derive(Debug)]
struct Outcome {
    exit: i32,
    streams: Streams,
    namespace: Option<BTreeMap<String, Value>>,
}

/// The 19 dest names the reference's namespace carries, read off this port's
/// parsed arguments.
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
        ("worktree".to_owned(), json!(arguments.worktree)),
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

fn drive_root(argv: &[String]) -> Outcome {
    let command = Arguments::command();
    let full = std::iter::once("vibe".to_owned()).chain(argv.iter().cloned());
    match command.try_get_matches_from(full) {
        Ok(matches) => {
            let arguments = <Arguments as clap::FromArgMatches>::from_arg_matches(&matches)
                .expect("a parse this port accepted builds its arguments");
            Outcome {
                exit: 0,
                streams: Streams {
                    stdout: false,
                    stderr: false,
                },
                namespace: Some(port_namespace(&arguments)),
            }
        }
        Err(error) => Outcome {
            exit: error.exit_code(),
            streams: Streams {
                stdout: !error.use_stderr(),
                stderr: error.use_stderr(),
            },
            namespace: None,
        },
    }
}

fn drive_mcp(argv: &[String]) -> Outcome {
    let mut output = Vec::new();
    match mcp_command::run(argv, &mut output) {
        Ok(()) => Outcome {
            exit: 0,
            streams: Streams {
                stdout: !output.is_empty(),
                stderr: false,
            },
            namespace: None,
        },
        Err(_) => Outcome {
            exit: 1,
            streams: Streams {
                stdout: !output.is_empty(),
                stderr: true,
            },
            namespace: None,
        },
    }
}

fn compare_case(case: &Case, differences: &mut Vec<Difference>) {
    let outcome = match case.parser.as_str() {
        "root" => drive_root(&case.argv),
        _ => drive_mcp(&case.argv),
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
    // A namespace is only comparable where both sides parsed; where one of them
    // refused the vector the exit code above already carries the difference.
    let (Some(reference), Some(port)) = (case.namespace.as_ref(), outcome.namespace.as_ref())
    else {
        return;
    };
    for (dest, value) in reference {
        let expected = normalized_reference_value(dest, value);
        let Some(found) = port.get(dest) else {
            continue;
        };
        if &expected != found {
            differences.push(Difference::new(
                &case.parser,
                &case.case,
                format!("/namespace/{dest}"),
                expected,
                found.clone(),
            ));
        }
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

fn replay(corpus: &Corpus) -> Vec<Difference> {
    let undriven: BTreeSet<&str> = UNDRIVEN.iter().copied().collect();
    let mut differences = Vec::new();
    for record in &corpus.parsers {
        compare_declarations(record, &mut differences);
    }
    for case in &corpus.cases {
        if undriven.contains(case.case.as_str()) {
            continue;
        }
        compare_case(case, &mut differences);
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
    assert_eq!(corpus.reference.version, "2.24.0");
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
    for case in &corpus.cases {
        assert!(
            seen.insert(case.case.clone()),
            "two cases share the id {}",
            case.case
        );
        assert!(matches!(case.parser.as_str(), "root" | "mcp"));
    }
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
        1,
        "the continuation group is the only exclusion the root parser declares"
    );
    let group = &root.mutually_exclusive_groups[0];
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
    assert!(
        UNDRIVEN_REASON.len() > 20,
        "the undriven cases explain nothing"
    );
}

/// Every argv shape that would reach a real configuration store is named, and
/// nothing else is skipped.
#[test]
fn every_case_that_reaches_the_configuration_store_is_named() {
    let corpus = corpus();
    let reaching: BTreeSet<&str> = corpus
        .cases
        .iter()
        .filter(|case| reaches_the_configuration_store(case))
        .map(|case| case.case.as_str())
        .collect();
    let named: BTreeSet<&str> = UNDRIVEN.iter().copied().collect();
    assert_eq!(
        reaching, named,
        "the corpus carries a `vibe mcp remove` vector the replay would drive against a real home"
    );
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
        .count()
        - UNDRIVEN.len();
    println!(
        "cli surface: {matched} of {} argv cases match this port, {actions} actions across \
         {} parsers replayed from {}, {exercised} ledger entries exercised, {} cases undriven",
        corpus.cases.len(),
        corpus.parsers.len(),
        &REFERENCE_COMMIT[..12],
        UNDRIVEN.len()
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
    // The value names the two help renders print for the same option.
    Divergence {
        parser: "root",
        case: "initial_prompt",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value PROMPT and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "prompt",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value TEXT and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "max_turns",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value N and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "max_price",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value DOLLARS and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "max_tokens",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value N and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "enabled_tools",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value TOOL and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "disabled_tools",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value TOOL and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "agent",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value NAME and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "workdir",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value DIR and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "add_dir",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value DIR and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "resume",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference names this value SESSION_ID and this port derives it from the field name, so the two help renders disagree on what the option takes",
    },
    Divergence {
        parser: "root",
        case: "output",
        pointer: "/valueName",
        closed_by: "US-318",
        row: "7",
        why: "the reference prints the three accepted values in place of a value name, and this port prints a value name derived from the field",
    },
    // The nine flags this port adds for its own runtime, all hidden.
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
    // The three `vibe mcp` parsers this port does not declare at all.
    Divergence {
        parser: "mcp",
        case: "mcp",
        pointer: "/actions",
        closed_by: "US-311",
        row: "7",
        why: "this port intercepts `vibe mcp` before parsing and matches the argument slice by hand, so it declares no parser and none of the reference's actions",
    },
    Divergence {
        parser: "mcp-add",
        case: "mcp-add",
        pointer: "/actions",
        closed_by: "US-314",
        row: "7",
        why: "this port declares no `add` sub-parser, so the positional and the twelve options the reference publishes have no counterpart here",
    },
    Divergence {
        parser: "mcp-remove",
        case: "mcp-remove",
        pointer: "/actions",
        closed_by: "US-313",
        row: "7",
        why: "this port matches the `remove` argument slice by hand, so the reference's positional and its help action have no declared counterpart",
    },
    // argparse infers long-flag prefixes and this port does not.
    Divergence {
        parser: "root",
        case: "prefix--version",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--version",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--prompt",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--prompt",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--max-turns",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--max-turns",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--max-price",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--max-price",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--max-tokens",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--max-tokens",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--enabled-tools",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--enabled-tools",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--disabled-tools",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--disabled-tools",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--output",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--output",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--agent",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--agent",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--auto-approve",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--auto-approve",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--setup",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--setup",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--check-upgrade",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--check-upgrade",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--workdir",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--workdir",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--worktree",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--worktree",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--add-dir",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--add-dir",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--trust",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--trust",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--teleport",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--teleport",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--continue",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--continue",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--resume",
        pointer: "/exit",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    Divergence {
        parser: "root",
        case: "prefix--resume",
        pointer: "/streams",
        closed_by: "US-322",
        row: "7",
        why: "the reference builds its parser with argparse's prefix inference on and this port declares no `infer_long_args`, so the abbreviation it resolves is an unknown flag here",
    },
    // Values argparse accepts at the parse boundary and clap refuses.
    Divergence {
        parser: "root",
        case: "output-repeated",
        pointer: "/exit",
        closed_by: "US-323",
        row: "7",
        why: "argparse lets the last occurrence of a store option overwrite the earlier one, and clap refuses the second occurrence outright",
    },
    Divergence {
        parser: "root",
        case: "output-repeated",
        pointer: "/streams",
        closed_by: "US-323",
        row: "7",
        why: "argparse lets the last occurrence of a store option overwrite the earlier one, and clap refuses the second occurrence outright",
    },
    Divergence {
        parser: "root",
        case: "repeated-agent",
        pointer: "/exit",
        closed_by: "US-323",
        row: "7",
        why: "argparse lets the last occurrence of a store option overwrite the earlier one, and clap refuses the second occurrence outright",
    },
    Divergence {
        parser: "root",
        case: "repeated-agent",
        pointer: "/streams",
        closed_by: "US-323",
        row: "7",
        why: "argparse lets the last occurrence of a store option overwrite the earlier one, and clap refuses the second occurrence outright",
    },
    Divergence {
        parser: "root",
        case: "negative-max-turns",
        pointer: "/exit",
        closed_by: "US-323",
        row: "7",
        why: "argparse hands a leading-hyphen numeric to `type=int` as the option's value, and clap reads it as an unknown short flag",
    },
    Divergence {
        parser: "root",
        case: "negative-max-turns",
        pointer: "/streams",
        closed_by: "US-323",
        row: "7",
        why: "argparse hands a leading-hyphen numeric to `type=int` as the option's value, and clap reads it as an unknown short flag",
    },
    Divergence {
        parser: "root",
        case: "negative-max-tokens",
        pointer: "/exit",
        closed_by: "US-323",
        row: "7",
        why: "argparse hands a leading-hyphen numeric to `type=int` as the option's value, and clap reads it as an unknown short flag",
    },
    Divergence {
        parser: "root",
        case: "negative-max-tokens",
        pointer: "/streams",
        closed_by: "US-323",
        row: "7",
        why: "argparse hands a leading-hyphen numeric to `type=int` as the option's value, and clap reads it as an unknown short flag",
    },
    Divergence {
        parser: "root",
        case: "negative-max-price",
        pointer: "/exit",
        closed_by: "US-323",
        row: "7",
        why: "argparse hands a leading-hyphen numeric to `type=float` as the option's value, and clap reads it as an unknown short flag",
    },
    Divergence {
        parser: "root",
        case: "negative-max-price",
        pointer: "/streams",
        closed_by: "US-323",
        row: "7",
        why: "argparse hands a leading-hyphen numeric to `type=float` as the option's value, and clap reads it as an unknown short flag",
    },
    // The `vibe mcp` argv shapes, none of which this port answers yet.
    Divergence {
        parser: "mcp",
        case: "mcp-bare",
        pointer: "/exit",
        closed_by: "US-311",
        row: "7",
        why: "the reference prints the `vibe mcp` help and exits 0 where this port answers every unrecognized shape with a one-line usage string on stderr",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-bare",
        pointer: "/streams",
        closed_by: "US-311",
        row: "7",
        why: "the reference prints the `vibe mcp` help and exits 0 where this port answers every unrecognized shape with a one-line usage string on stderr",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-help-short",
        pointer: "/exit",
        closed_by: "US-311",
        row: "7",
        why: "the reference answers `-h` from its own parser and exits 0, and this port declares no parser to answer it",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-help-short",
        pointer: "/streams",
        closed_by: "US-311",
        row: "7",
        why: "the reference answers `-h` from its own parser and exits 0, and this port declares no parser to answer it",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-help-long",
        pointer: "/exit",
        closed_by: "US-311",
        row: "7",
        why: "the reference answers `--help` from its own parser and exits 0, and this port declares no parser to answer it",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-help-long",
        pointer: "/streams",
        closed_by: "US-311",
        row: "7",
        why: "the reference answers `--help` from its own parser and exits 0, and this port declares no parser to answer it",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-help",
        pointer: "/exit",
        closed_by: "US-314",
        row: "7",
        why: "the reference renders the `add` sub-parser's own help and exits 0, and this port declares no `add` sub-parser",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-help",
        pointer: "/streams",
        closed_by: "US-314",
        row: "7",
        why: "the reference renders the `add` sub-parser's own help and exits 0, and this port declares no `add` sub-parser",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-http-oauth-no-login",
        pointer: "/exit",
        closed_by: "US-316",
        row: "7",
        why: "the reference persists the remote server and reports the outcome on stdout, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-http-oauth-no-login",
        pointer: "/streams",
        closed_by: "US-316",
        row: "7",
        why: "the reference persists the remote server and reports the outcome on stdout, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-streamable-http-static-auth",
        pointer: "/exit",
        closed_by: "US-316",
        row: "7",
        why: "the reference persists the remote server with its static authentication and reports the outcome on stdout, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-streamable-http-static-auth",
        pointer: "/streams",
        closed_by: "US-316",
        row: "7",
        why: "the reference persists the remote server with its static authentication and reports the outcome on stdout, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-stdio-command",
        pointer: "/exit",
        closed_by: "US-316",
        row: "7",
        why: "the reference persists the stdio server and reports the outcome on stdout, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-stdio-command",
        pointer: "/streams",
        closed_by: "US-316",
        row: "7",
        why: "the reference persists the stdio server and reports the outcome on stdout, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-stdio-again",
        pointer: "/exit",
        closed_by: "US-316",
        row: "7",
        why: "the reference recognizes the identical entry as already configured and exits 0, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-stdio-again",
        pointer: "/streams",
        closed_by: "US-316",
        row: "7",
        why: "the reference recognizes the identical entry as already configured and exits 0, and this port refuses every `add` shape",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-unknown-subcommand",
        pointer: "/exit",
        closed_by: "US-312",
        row: "7",
        why: "the reference reports an argparse choice failure and exits 2, and this port exits 1 with a bare usage string",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-remove-without-name",
        pointer: "/exit",
        closed_by: "US-312",
        row: "7",
        why: "the reference reports the missing required NAME and exits 2, and this port exits 1 with a bare usage string",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-remove-two-names",
        pointer: "/exit",
        closed_by: "US-312",
        row: "7",
        why: "the reference reports the extra argument and exits 2, and this port exits 1 with a bare usage string",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-without-name",
        pointer: "/exit",
        closed_by: "US-312",
        row: "7",
        why: "the reference reports the missing required NAME and exits 2, and this port exits 1 with a bare usage string",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-invalid-transport",
        pointer: "/exit",
        closed_by: "US-312",
        row: "7",
        why: "the reference reports the rejected `--transport` choice and exits 2, and this port exits 1 with a bare usage string",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-url-without-value",
        pointer: "/exit",
        closed_by: "US-312",
        row: "7",
        why: "the reference reports that `--url` expected one argument and exits 2, and this port exits 1 with a bare usage string",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-stdio-with-remote-flag",
        pointer: "/exit",
        closed_by: "US-315",
        row: "7",
        why: "the reference refuses the stdio transport carrying a remote-only flag and exits 2, and this port refuses every `add` shape with exit 1",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-remote-without-url",
        pointer: "/exit",
        closed_by: "US-315",
        row: "7",
        why: "the reference refuses a remote transport with no `--url` and exits 2, and this port refuses every `add` shape with exit 1",
    },
    Divergence {
        parser: "mcp",
        case: "mcp-add-duplicate-url",
        pointer: "/exit",
        closed_by: "US-316",
        row: "7",
        why: "the reference refuses a second name for a URL it already carries and exits 2, and this port refuses every `add` shape with exit 1",
    },
];
