//! Differential oracle for the experiments engine, its configuration layer and
//! its session gates.
//!
//! `scripts/parity/experiments.py` drives the reference's own
//! `RemoteEvalClient`, `ExperimentManager`, `GrowthbookLayer` and session
//! helpers over inputs the script authors, and records thirteen families into
//! `crates/vibe-core/tests/experiments/corpus.json`. This module replays that
//! corpus against this build unconditionally: only the recapture probe at the
//! bottom skips when the checkout is absent or off-pin.
//!
//! The capture never reaches a network. In remote evaluation mode the GrowthBook
//! proxy performs the bucketing and rewrites every feature as a pre-resolved
//! `force` rule carrying the exposure metadata in `tracks`, so a client has no
//! assignment logic of its own to get wrong. What a client implementation *can*
//! get wrong is entirely local: which URL it builds, what payload it posts, how
//! it fails open, how it resolves a value, which features it keeps, and which
//! variants it lets reach configuration versus telemetry. All of that is
//! measured here by feeding both sides the same synthetic eval response.
//!
//! A key is `family/field/case`, and a trailing `*` covers every key that starts
//! with the prefix. A divergence no entry names fails the replay; an entry whose
//! divergence stopped reproducing fails as stale, which is what forces a row out
//! once the behavior conforms.
//!
//! Every family is compared field by field. Where this build has no counterpart
//! at all the port answer is [`None`], which is what makes an entry go stale the
//! moment the surface starts answering: EP-002 through EP-005 remove these rows
//! by filling [`port_answer`], not by editing the ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};

use crate::config::registry::default_document;
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/experiments/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/experiments.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the capture
/// script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 480;

/// Keys the corpus carries that are not families: the pin, the layout and the
/// prose-free note.
const METADATA: [&str; 3] = ["schemaVersion", "reference", "note"];

/// Every family the corpus declares, with the case fields that are *inputs* the
/// capture authored and the case fields that are *answers* both sides give.
///
/// A family the capture adds without an entry here fails the replay by name
/// rather than passing unread, and so does a field: the runner asserts that
/// inputs and answers together account for every key a case carries.
const FAMILIES: &[Family] = &[
    Family {
        name: "constants",
        inputs: &[],
        answers: &["value"],
    },
    Family {
        name: "bucketingKey",
        inputs: &["variable"],
        answers: &["bucketingKey", "length", "stable", "hexadecimal"],
    },
    Family {
        name: "evalUrl",
        inputs: &["apiHost", "clientKey"],
        answers: &["url"],
    },
    Family {
        name: "evalRequest",
        inputs: &[],
        answers: &[
            "method",
            "url",
            "headerNames",
            "payloadKeys",
            "attributeKeys",
            "attributes",
            "forcedVariations",
            "forcedFeatures",
            "urlField",
            "credentialVariable",
            "returnedState",
            "requests",
        ],
    },
    Family {
        name: "evalFailures",
        inputs: &[],
        answers: &[
            "state",
            "requests",
            "variants",
            "variantsAreDefaults",
            "assignments",
            "configVariants",
            "logs",
        ],
    },
    Family {
        name: "featureResolution",
        inputs: &["definition"],
        answers: &["resolved"],
    },
    Family {
        name: "variantResolution",
        inputs: &["response"],
        answers: &["knownFeatures", "variants", "variantsOrNone"],
    },
    Family {
        name: "configVariants",
        inputs: &["response"],
        answers: &["assignments", "configVariants"],
    },
    Family {
        name: "variantLabels",
        inputs: &["definition"],
        answers: &["assignments", "reported"],
    },
    Family {
        name: "configMapping",
        inputs: &["variants"],
        answers: &["data", "hasFingerprint"],
    },
    Family {
        name: "layerPrecedence",
        inputs: &["user", "project", "environment", "overrides", "variants"],
        answers: &["effective"],
    },
    Family {
        name: "sessionGates",
        inputs: &["configuration", "helper"],
        answers: &[
            "returned",
            "evalRequests",
            "identityRequests",
            "identityTimeout",
            "persisted",
            "organizationId",
        ],
    },
    Family {
        name: "attributes",
        inputs: &["document", "context"],
        answers: &["attributes", "payloadKeys", "credentialVariable"],
    },
];

/// Cases where this build answers something other than the reference, each with
/// the reason and the story that closes it.
///
/// Every entry here is open work rather than an accepted divergence: this epic
/// builds the instrument and nothing else, so the engine, the layer and the
/// session lifecycle are all absent and answer [`None`]. The staleness check is
/// what forces a row out once its behavior conforms, so the ledger cannot
/// outlive the gap it records.
const DIVERGENCES: &[(&str, &str)] = &[
    // -- EP-003: the experiments engine -------------------------------------
    (
        "constants/value/experimentNames",
        "OPEN: US-006 declares the three reference experiment names and their client-side defaults",
    ),
    ("constants/value/defaultVariants", "OPEN: as above"),
    (
        "constants/value/everyNameHasADefault",
        "OPEN: as above; the reference ties the two together with a module-level assertion and \
         US-006 owes this port the same guard",
    ),
    (
        "constants/value/evalPathTemplate",
        "OPEN: US-007 ports the eval URL construction and its timeout",
    ),
    ("constants/value/evalTimeoutSeconds", "OPEN: as above"),
    (
        "constants/value/payloadKeys",
        "OPEN: as above; the four payload keys are the client's own request shape",
    ),
    (
        "constants/value/bucketingKeyLength",
        "OPEN: US-008 derives the anonymous bucketing key",
    ),
    (
        "bucketingKey/*",
        "OPEN: US-008 derives the bucketing key as a truncated SHA-256 of the API key",
    ),
    (
        "evalUrl/*",
        "OPEN: US-007 ports the URL construction, including the strip, the trailing-slash removal \
         and the empty-input null",
    ),
    (
        "evalRequest/*",
        "OPEN: US-007 ports the eval request, its payload and its lazy client",
    ),
    (
        "evalFailures/*",
        "OPEN: US-007 ports the fail-open contract across the five failure branches",
    ),
    (
        "featureResolution/*",
        "OPEN: US-006 ports the tolerant eval models and `resolved_value`",
    ),
    (
        "variantResolution/*",
        "OPEN: US-008 ports the manager's two variant readers and its known-key filter",
    ),
    (
        "configVariants/*",
        "OPEN: US-008 splits confirmed exposures from forced assignments",
    ),
    (
        "variantLabels/*",
        "OPEN: US-008 ports the four-level label fallback and its empty terminal",
    ),
    // -- EP-005: the session lifecycle --------------------------------------
    (
        "constants/value/identityPath",
        "OPEN: US-011 ports the identity gateway the organization attribute depends on",
    ),
    ("constants/value/identityTimeoutSeconds", "OPEN: as above"),
    (
        "sessionGates/*",
        "OPEN: US-012 ports the two session helpers, their gates and their persistence",
    ),
    (
        "attributes/*",
        "OPEN: US-012 builds the nine attributes from a launch context",
    ),
    // -- EP-004: the GrowthBook configuration layer -------------------------
    (
        "constants/value/layerName",
        "OPEN: US-009 names the layer that turns a variant into a configuration value",
    ),
    (
        "constants/value/configuredFields",
        "OPEN: US-009 maps the three experiments onto the four configuration fields",
    ),
    (
        "configMapping/*",
        "OPEN: US-009 ports the three mappers, the empty-snapshot branches and the read-only \
         refusal",
    ),
    (
        "layerPrecedence/*",
        "OPEN: US-009 fills the layer and US-010 reseats it below the selected TOML, where the \
         reference puts it; the layer socket exists here but nothing maps a variant into it",
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
    let parsed: Value = serde_json::from_str(&raw).expect("the experiments corpus parses");
    let corpus = parsed
        .as_object()
        .expect("the experiments corpus is an object")
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
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

/// Every case of one family, each as its object.
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
    /// One comparison against a surface this build may not have yet. The port
    /// answer is [`None`] where the surface is absent, which is what makes the
    /// ledger entry go stale the moment it starts answering.
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

/// What the ledger has to say about one family's report: the divergences it does
/// not name, and the entries whose divergence no longer reproduces.
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

/// Fails on any divergence the ledger does not name, and on any ledger entry
/// whose divergence no longer reproduces, then reports the family's count.
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
        "experiments: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// This build's answers
// --------------------------------------------------------------------------

/// What this build answers for one `family/field/case`, or [`None`] where the
/// surface the case measures does not exist here yet.
///
/// This is the single seam every later story widens. An entry the ledger names
/// goes stale as soon as a match here starts answering, so a surface cannot land
/// without the row that recorded its absence coming out in the same change.
fn port_answer(family: &str, field: &str, case: &str) -> Option<Value> {
    match (family, field, case) {
        // The `[experiments]` table is the socket the engine plugs into, and
        // this port already publishes it with the reference's own schema and
        // defaults, down to the committed client key.
        ("constants", "value", "experimentsConfigDefaults") => {
            Some(published_experiments_defaults())
        }
        _ => None,
    }
}

/// The `[experiments]` defaults this build's published schema carries, read the
/// way a running binary reads them.
fn published_experiments_defaults() -> Value {
    let table = default_document();
    let experiments = table
        .get("experiments")
        .and_then(toml::Value::as_table)
        .expect("the published document declares the experiments table");
    let mut answered = Map::new();
    for key in ["enable", "api_host", "client_key"] {
        let value = experiments
            .get(key)
            .unwrap_or_else(|| panic!("the experiments table declares {key}"));
        answered.insert(
            key.to_owned(),
            serde_json::to_value(value).expect("a TOML scalar converts to JSON"),
        );
    }
    Value::Object(answered)
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

/// Replays one family, comparing every answer field of every case.
fn run_family(corpus: &Map<String, Value>, family: &Family, report: &mut Report) {
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
            let actual = port_answer(family.name, field, identifier);
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

/// The two failure modes the replay exists for, proven on a report the test
/// builds rather than on the corpus: a divergence the ledger does not name has
/// to be reported with its family, its case and both values, and a ledger entry
/// whose divergence stopped reproducing has to be reported as stale.
#[test]
fn the_ledger_reports_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger = [("evalUrl/url/named", "recorded")];
    let mut report = Report::default();
    report.check(
        "evalUrl",
        "url",
        "unnamed",
        &Value::String("https://reference.example.test".to_owned()),
        Some(&Value::String("https://port.example.test".to_owned())),
    );
    let (unrecorded, stale) = audit(&report, "evalUrl", &ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    let reported = &unrecorded[0];
    assert!(reported.starts_with("evalUrl/url/unnamed:"), "{reported}");
    assert!(
        reported.contains("https://reference.example.test"),
        "{reported}"
    );
    assert!(reported.contains("https://port.example.test"), "{reported}");
    assert_eq!(
        stale,
        vec!["evalUrl/url/named".to_owned()],
        "an entry whose case stopped diverging is stale"
    );

    // An absent surface reads as a divergence too, which is what keeps a
    // ledgered gap from passing quietly once it closes.
    let mut absent = Report::default();
    absent.check("evalUrl", "url", "named", &Value::Null, None);
    let (unrecorded, stale) = audit(&absent, "evalUrl", &ledger);
    assert!(unrecorded.is_empty(), "the ledger names it: {unrecorded:?}");
    assert!(stale.is_empty(), "it still diverges: {stale:?}");
    assert!(absent.divergences[0].ends_with("port absent"));
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!(
        "experiments: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut comparisons = 0;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for family in FAMILIES {
        let mut report = Report::default();
        run_family(&corpus, family, &mut report);
        let total = settle(&report, family.name);
        counts.insert(family.name, total);
        comparisons += total;
    }
    println!(
        "experiments: {comparisons} comparisons across {} families replayed at {}",
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
    if let Some(reason) = off_pin_reason(&root, "experiments") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/experiments-corpus.json");
    let promo = repository.join("target/experiments-promo-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/experiments-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .arg("--promo-corpus")
        .arg(&promo)
        .current_dir(&repository)
        .output()
        .expect("the experiments capture script runs");
    assert!(
        output.status.success(),
        "the experiments capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`"
    );
}
