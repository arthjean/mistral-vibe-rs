//! Differential oracle for the command registry.
//!
//! `scripts/parity/commands.py` drives the pinned reference's own
//! `CommandRegistry` over inputs the script authors, and records eight families
//! into `crates/vibe-cli/tests/commands/corpus.json`. This module replays that
//! corpus against this build unconditionally: only the recapture probe at the
//! bottom skips when the checkout is absent or off-pin.
//!
//! Row 2 of `docs/parity.md` used to be measured by diffing two lists of names,
//! which cannot see what an alias resolves to, which commands a context leaves
//! standing, or what `/help` prints. Those are what the corpus records and what
//! this module compares.
//!
//! The reference's help lines are authored prose `NOTICE` forbids reproducing,
//! so the corpus measures each one as a byte length and a SHA-256 rather than
//! carrying it. [`this_ports_help_prose_never_matches_a_reference_digest`] holds
//! this port's own lines permanently unequal to every one of those digests, and
//! [`the_corpus_carries_no_reference_help_line_in_cleartext`] fails if a
//! reference line is ever pasted back into the corpus it was reduced from.
//!
//! A key is `family/field/case`, and a trailing `*` covers every key that starts
//! with the prefix. A divergence no entry names fails the replay; an entry whose
//! divergence stopped reproducing fails as stale, which is what forces a row out
//! once the behavior conforms.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::commands::{COMMANDS, CommandContext, command_available_in, parse_command_in};

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/commands/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/commands.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the capture
/// script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, read off the first real capture
/// rather than estimated, so a regeneration that captured almost nothing fails
/// instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 395;

/// Keys the corpus carries that are not families: the pin, the layout and the
/// prose-free note.
const METADATA: [&str; 3] = ["schemaVersion", "reference", "note"];

/// Every family the corpus declares, with the case fields that are *inputs* the
/// capture authored and the case fields that are *answers* both sides give.
const FAMILIES: &[Family] = &[
    Family {
        name: "counts",
        inputs: &[],
        answers: &["count"],
    },
    Family {
        name: "inventory",
        inputs: &[],
        answers: &["aliases"],
    },
    Family {
        name: "availability",
        inputs: &["vibeCodeEnabled", "clipboardSupported", "excluded"],
        answers: &["keys", "count"],
    },
    Family {
        name: "parse",
        inputs: &["context", "input"],
        answers: &["key", "alias", "arguments"],
    },
    Family {
        name: "helpDocument",
        inputs: &[],
        answers: &["count"],
    },
    Family {
        name: "helpSections",
        inputs: &[],
        answers: &["index", "headingLine", "level", "lineCount"],
    },
    Family {
        name: "helpCommands",
        inputs: &[],
        answers: &["index", "line", "aliases"],
    },
    Family {
        name: "helpProse",
        inputs: &[],
        answers: &["length", "digest"],
    },
];

/// Cases where this build answers something other than the reference, each with
/// the reason and the story that closes it.
///
/// EP-069 builds the instrument. The four `help*` families measure a document
/// this port does not publish yet: `/help` opens a modal picker here, so there
/// is no line to compare, no section to position and no prose of this port's own
/// to hold unequal. EP-070 writes that document, and the staleness check is what
/// forces each row out when it does.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "helpDocument/*",
        "OPEN: US-231 replaces the modal help with a Markdown transcript message; until it lands \
         this port publishes no help document to count lines in",
    ),
    (
        "helpSections/*",
        "OPEN: US-231 gives the document its three sections and US-232 reconciles the shortcut \
         section against the chords this binary actually binds",
    ),
    (
        "helpCommands/*",
        "OPEN: US-231 sorts the command section by registry key and lists every alias of each \
         command, canonical `/name` first",
    ),
    (
        "helpProse/*",
        "OPEN: US-231 writes this port's own lines. The reference's are measured as a length and \
         a SHA-256 because `NOTICE` forbids reproducing them, so this port answers nothing until \
         it has prose of its own; \
         `this_ports_help_prose_never_matches_a_reference_digest` is what keeps the two apart \
         afterward, and the split is a permanent row in the scorecard's divergence table",
    ),
];

/// One family's shape: which case fields the capture authored and which ones
/// both sides answer.
struct Family {
    name: &'static str,
    inputs: &'static [&'static str],
    answers: &'static [&'static str],
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus() -> Map<String, Value> {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
    let parsed: Value = serde_json::from_str(&raw).expect("the registry corpus parses");
    let corpus = parsed
        .as_object()
        .expect("the registry corpus is an object")
        .clone();
    assert_eq!(
        corpus.get("schemaVersion").and_then(Value::as_u64),
        Some(u64::from(CORPUS_SCHEMA_VERSION)),
        "the corpus layout moved; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus
            .get("reference")
            .and_then(|reference| reference.get("commit"))
            .and_then(Value::as_str),
        Some(REFERENCE_COMMIT),
        "the corpus was captured from an unpinned reference; the replay compares one revision or \
         none"
    );
    corpus
}

fn cases<'a>(corpus: &'a Map<String, Value>, family: &str) -> &'a Vec<Value> {
    corpus
        .get(family)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the corpus carries the {family} family as an array"))
}

fn case_id(case: &Value) -> &str {
    case.get("id")
        .and_then(Value::as_str)
        .expect("every corpus case carries an id")
}

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// Whether a ledger entry covers a divergence key: an exact match, or a
/// `prefix*` entry the key starts with.
fn covers(entry: &str, key: &str) -> bool {
    entry
        .strip_suffix('*')
        .map_or(entry == key, |prefix| key.starts_with(prefix))
}

/// Records one comparison, so a family reports a count and a divergence names
/// itself instead of stopping at the first one.
#[derive(Default)]
struct Report {
    conformant: usize,
    total: usize,
    divergences: Vec<String>,
    observed: Vec<String>,
}

impl Report {
    fn check(
        &mut self,
        family: &str,
        field: &str,
        case: &str,
        expected: &Value,
        actual: Option<&Value>,
    ) {
        self.total = self.total.saturating_add(1);
        if actual == Some(expected) {
            self.conformant = self.conformant.saturating_add(1);
            return;
        }
        self.observed.push(format!("{family}/{field}/{case}"));
        self.divergences.push(format!(
            "{family}/{field}/{case}: reference {expected}, port {}",
            actual.map_or_else(|| "absent".to_owned(), Value::to_string)
        ));
    }
}

fn audit(report: &Report, family: &str, ledger: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !ledger.iter().any(|(entry, _)| covers(entry, key))
        })
        .cloned()
        .collect::<Vec<_>>();
    let family_prefix = format!("{family}/");
    let stale = ledger
        .iter()
        .map(|(entry, _)| (*entry).to_owned())
        .filter(|entry| entry.starts_with(&family_prefix))
        .filter(|entry| !report.observed.iter().any(|key| covers(entry, key)))
        .collect::<Vec<_>>();
    (unrecorded, stale)
}

fn settle(report: &Report, family: &str) -> usize {
    let (unrecorded, stale) = audit(report, family, DIVERGENCES);
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    let ledgered = report.total.saturating_sub(report.conformant);
    println!(
        "commands: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// This build's answers
// --------------------------------------------------------------------------

/// The help lines this port *authors*: the section headings, the shortcut lines
/// and the feature lines.
///
/// Empty until US-231 writes the transcript document. Whatever lands here is
/// held permanently unequal to every reference digest by
/// [`this_ports_help_prose_never_matches_a_reference_digest`], which is how the
/// licensing boundary stays measurable without a reference line entering this
/// repository.
///
/// The twenty-eight command lines are deliberately not among them. A command
/// line is `- <aliases>: <description>`, and both halves are the observable
/// contract rather than prose: the aliases come from the registry and the
/// descriptions are already byte-identical to the reference's, which the four
/// `commands-*` popup traces assert and which no story of this PRD may rewrite.
/// Measured against the committed corpus, all twenty-eight lines rebuilt from
/// `COMMANDS` hash to a digest `helpProse` records, so routing them through this
/// function would make US-231 unsatisfiable: it would forbid the very lines its
/// own criteria require. `helpCommands` is what compares them, on their order
/// and their alias list, which is the part a port can get wrong.
fn port_help_lines() -> Vec<String> {
    Vec::new()
}

/// The availability contexts the corpus declares, rebuilt as this port's own
/// [`CommandContext`].
///
/// The definitions are read out of the `availability` family rather than
/// restated here, so the parse family resolves under exactly the contexts the
/// capture recorded and a new context is added in one place.
fn port_contexts(corpus: &Map<String, Value>) -> BTreeMap<String, CommandContext> {
    cases(corpus, "availability")
        .iter()
        .map(|case| {
            let excluded = case
                .get("excluded")
                .and_then(Value::as_array)
                .expect("every availability case declares an excluded list")
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .expect("an excluded entry is a command name")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            let context = CommandContext::new(
                case.get("vibeCodeEnabled")
                    .and_then(Value::as_bool)
                    .expect("every availability case declares vibeCodeEnabled"),
            )
            .with_clipboard_image_supported(
                case.get("clipboardSupported")
                    .and_then(Value::as_bool)
                    .expect("every availability case declares clipboardSupported"),
            )
            .with_excluded(excluded.iter().map(String::as_str));
            (case_id(case).to_owned(), context)
        })
        .collect()
}

fn available_keys(context: &CommandContext) -> Vec<&'static str> {
    let mut keys = COMMANDS
        .iter()
        .filter(|command| command_available_in(command, context))
        .map(|command| command.name)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn strings(values: impl IntoIterator<Item = impl Into<String>>) -> Value {
    Value::Array(
        values
            .into_iter()
            .map(|value| Value::String(value.into()))
            .collect(),
    )
}

fn count(value: usize) -> Value {
    Value::Number(value.into())
}

/// What this build answers for one case of `family`, or [`None`] where the
/// surface the case measures does not exist here yet.
fn port_answer(
    contexts: &BTreeMap<String, CommandContext>,
    family: &str,
    field: &str,
    case: &Map<String, Value>,
) -> Option<Value> {
    let id = case.get("id")?.as_str()?;
    match family {
        "counts" => {
            let aliases = COMMANDS
                .iter()
                .flat_map(|command| command.aliases.iter().copied())
                .collect::<Vec<_>>();
            let value = match id {
                "keys" => COMMANDS.len(),
                "aliases" => aliases.len(),
                "slashAliases" => aliases.iter().filter(|a| a.starts_with('/')).count(),
                "bareAliases" => aliases.iter().filter(|a| !a.starts_with('/')).count(),
                _ => return None,
            };
            Some(count(value))
        }
        "inventory" => {
            let command = COMMANDS.iter().find(|command| command.name == id)?;
            let mut aliases = command.aliases.to_vec();
            aliases.sort_unstable();
            Some(strings(aliases))
        }
        "availability" => {
            let context = contexts.get(id)?;
            let keys = available_keys(context);
            match field {
                "keys" => Some(strings(keys)),
                "count" => Some(count(keys.len())),
                _ => None,
            }
        }
        "parse" => {
            let context = contexts.get(case.get("context")?.as_str()?)?;
            let input = case.get("input")?.as_str()?;
            let Some(parsed) = parse_command_in(input, context) else {
                // The reference records a refusal as three nulls, so this port
                // answers the same shape rather than an absent value: a
                // refusal both sides agree on is a conformance, not a hole.
                return Some(Value::Null);
            };
            let command = COMMANDS.iter().find(|command| command.id == parsed.id)?;
            match field {
                "key" => Some(Value::String(command.name.to_owned())),
                // The reference records the alias-map entry the head word was
                // looked up under, which is the declared alias itself because
                // every declared alias is lowercase. Deriving it here rather
                // than reading a field off `ParsedCommand` is deliberate: it
                // fails when this port resolves through a fold the reference
                // does not perform.
                "alias" => {
                    let lowered = parsed.alias.to_lowercase();
                    command
                        .aliases
                        .iter()
                        .find(|alias| **alias == lowered.as_str())
                        .map(|alias| Value::String((*alias).to_owned()))
                }
                "arguments" => Some(Value::String(parsed.arguments.to_owned())),
                _ => None,
            }
        }
        // The help document is EP-070's surface; this port opens a modal picker
        // instead, so every help family is answered by the ledger until US-231
        // lands.
        _ => None,
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

fn run_family(
    corpus: &Map<String, Value>,
    contexts: &BTreeMap<String, CommandContext>,
    family: &Family,
    report: &mut Report,
) {
    let declared = family
        .inputs
        .iter()
        .chain(family.answers.iter())
        .copied()
        .collect::<BTreeSet<_>>();
    for case in cases(corpus, family.name) {
        let object = case
            .as_object()
            .unwrap_or_else(|| panic!("{} cases are objects", family.name));
        let carried = object
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "id")
            .collect::<BTreeSet<_>>();
        assert!(
            carried.iter().all(|key| declared.contains(key)),
            "the {} case {} carries fields this replay does not read: {:?}; declare them as an \
             input or as an answer rather than leaving them unread",
            family.name,
            case_id(case),
            carried.difference(&declared).collect::<Vec<_>>()
        );
        let identifier = case_id(case);
        for field in family.answers {
            let Some(expected) = object.get(*field) else {
                continue;
            };
            let actual = port_answer(contexts, family.name, field, object);
            report.check(family.name, field, identifier, expected, actual.as_ref());
        }
    }
}

#[test]
fn every_corpus_key_is_a_family_this_replay_reads() {
    let corpus = corpus();
    let declared = FAMILIES
        .iter()
        .map(|family| family.name)
        .chain(METADATA)
        .collect::<BTreeSet<_>>();
    let carried = corpus.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        carried, declared,
        "the corpus and this replay disagree on which families exist; regenerate with \
         {CAPTURE_SCRIPT} or declare the family here"
    );
}

#[test]
fn every_ledger_entry_names_a_declared_family() {
    let declared = FAMILIES
        .iter()
        .map(|family| family.name)
        .collect::<BTreeSet<_>>();
    let orphans = DIVERGENCES
        .iter()
        .map(|(entry, _)| *entry)
        .filter(|entry| {
            entry
                .split('/')
                .next()
                .is_none_or(|family| !declared.contains(family))
        })
        .collect::<Vec<_>>();
    assert!(
        orphans.is_empty(),
        "these ledger entries name a family the corpus does not carry: {orphans:?}"
    );
}

fn hex_digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reference_help_digests(corpus: &Map<String, Value>) -> BTreeMap<String, String> {
    let digests = cases(corpus, "helpProse")
        .iter()
        .map(|case| {
            (
                case_id(case).to_owned(),
                case.get("digest")
                    .and_then(Value::as_str)
                    .expect("every help prose case carries a digest")
                    .to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert!(
        !digests.is_empty(),
        "the corpus records no reference help prose to stay unequal to"
    );
    digests
}

/// `NOTICE` forbids shipping a reference-authored line, and a digest is what
/// makes that enforceable without carrying the text: every help line this port
/// ships is compared against every reference digest and has to differ from each.
/// A blank line would trivially differ, so it fails too.
#[test]
fn this_ports_help_prose_never_matches_a_reference_digest() {
    let corpus = corpus();
    let reference = reference_help_digests(&corpus);
    for line in port_help_lines() {
        assert!(
            !line.trim().is_empty(),
            "this port's help carries a blank line as prose, which is not a line of its own"
        );
        let digest = hex_digest(&line);
        for (name, expected) in &reference {
            assert_ne!(
                &digest, expected,
                "this port's help reproduces the reference's {name}, which `NOTICE` forbids"
            );
        }
    }
}

/// The other half of the same boundary: the corpus reduced the reference's lines
/// to digests, and nothing may put one back. Every string the corpus carries is
/// hashed and held unequal to every digest it records, so a line pasted into a
/// note, an input or an identifier fails the suite rather than shipping.
#[test]
fn the_corpus_carries_no_reference_help_line_in_cleartext() {
    let corpus = corpus();
    let reference = reference_help_digests(&corpus)
        .into_values()
        .collect::<BTreeSet<_>>();
    let mut offenders = Vec::new();
    collect_strings(&Value::Object(corpus.clone()), &mut |text| {
        if reference.contains(&hex_digest(text)) {
            offenders.push(text.to_owned());
        }
    });
    assert!(
        offenders.is_empty(),
        "{CORPUS_RELATIVE} carries {} reference-authored line(s) in cleartext; the corpus records \
         them as a length and a digest and never as text",
        offenders.len()
    );
}

fn collect_strings(value: &Value, visit: &mut impl FnMut(&str)) {
    match value {
        Value::String(text) => visit(text),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, visit);
            }
        }
        Value::Object(entries) => {
            for item in entries.values() {
                collect_strings(item, visit);
            }
        }
        _ => {}
    }
}

/// The two failure modes the replay exists for, proven on a report the test
/// builds rather than on the corpus: a divergence the ledger does not name has
/// to be reported with its family, its case and both values, and a ledger entry
/// whose divergence stopped reproducing has to be reported as stale.
#[test]
fn the_ledger_reports_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger = [("parse/key/named", "recorded")];
    let mut report = Report::default();
    report.check(
        "parse",
        "key",
        "unnamed",
        &Value::String("exit".to_owned()),
        Some(&Value::Null),
    );
    let (unrecorded, stale) = audit(&report, "parse", &ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    let reported = &unrecorded[0];
    assert!(reported.starts_with("parse/key/unnamed:"), "{reported}");
    assert!(reported.contains("reference \"exit\""), "{reported}");
    assert!(reported.contains("port null"), "{reported}");
    assert_eq!(
        stale,
        vec!["parse/key/named".to_owned()],
        "an entry whose case stopped diverging is stale"
    );
}

/// The parse family is only an oracle for the branches it reaches, and the
/// criteria that motivated it name them one by one. This holds the corpus to
/// carrying each rather than to a probe count alone.
#[test]
fn the_parse_family_reaches_every_branch_it_was_built_for() {
    let corpus = corpus();
    let probes = cases(&corpus, "parse");
    assert!(
        probes.len() >= 40,
        "the parse family carries {} probes, below the 40 the corpus commits to",
        probes.len()
    );
    let inputs = probes
        .iter()
        .map(|case| {
            case.get("input")
                .and_then(Value::as_str)
                .expect("every parse case carries its input")
        })
        .collect::<Vec<_>>();
    let has = |predicate: &dyn Fn(&str) -> bool| inputs.iter().any(|input| predicate(input));
    assert!(has(&|input| input.starts_with(' ')), "leading whitespace");
    assert!(has(&|input| input.ends_with(' ')), "trailing whitespace");
    assert!(
        has(&|input| input.contains("  ") && input.trim().contains("  ")),
        "an interior whitespace run"
    );
    assert!(has(&|input| input == "exit"), "a bare alias alone");
    assert!(
        has(&|input| input.starts_with("exit ")),
        "a bare alias followed by arguments"
    );
    assert!(
        has(&|input| input.starts_with("/mcp ")),
        "a slash alias followed by arguments"
    );
    assert!(has(&|input| input.is_empty()), "empty input");
    assert!(
        has(&|input| !input.is_empty() && input.trim().is_empty()),
        "whitespace-only input"
    );
    assert!(has(&|input| input == "/nope"), "an unknown alias");
    assert!(has(&|input| input == "/HELP"), "an alias in uppercase");
    assert!(
        has(&|input| !input.is_ascii() && input.to_lowercase().is_ascii()),
        "an alias spelled with a non-ASCII character whose Unicode lowercase is ASCII"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    let contexts = port_contexts(&corpus);
    println!(
        "commands: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut comparisons = 0;
    for family in FAMILIES {
        let mut report = Report::default();
        run_family(&corpus, &contexts, family, &mut report);
        comparisons += settle(&report, family.name);
    }
    println!(
        "commands: {comparisons} comparisons across {} families replayed at {}",
        FAMILIES.len(),
        &REFERENCE_COMMIT[..12],
    );
    assert!(
        comparisons >= MINIMUM_COMPARISONS,
        "the corpus replays {comparisons} comparisons, below the {MINIMUM_COMPARISONS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "commands") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/commands-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the commands capture script runs");
    assert!(
        output.status.success(),
        "the commands capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`"
    );
}
