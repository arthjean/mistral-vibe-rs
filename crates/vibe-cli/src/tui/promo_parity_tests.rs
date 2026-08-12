//! Differential oracle for the VS Code extension promo.
//!
//! `scripts/parity/experiments.py` drives the reference's own promo predicate
//! and filesystem repository over inputs the script authors, and records four
//! families into `crates/vibe-cli/tests/promo/corpus.json`. This module replays
//! that corpus against this build unconditionally: only the recapture probe at
//! the bottom skips when the checkout is absent or off-pin.
//!
//! The two reference sentences are recorded as a byte length and a SHA-256,
//! never as text: `NOTICE` forbids shipping reference-authored prose, so the
//! corpus measures them without carrying them and
//! [`this_ports_prose_never_matches_a_reference_digest`] holds this port's own
//! sentences permanently unequal to both.
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
use vibe_core::updates::UpdateCacheStore;

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/promo/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/experiments.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the capture
/// script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 60;

/// Keys the corpus carries that are not families: the pin, the layout and the
/// prose-free note.
const METADATA: [&str; 3] = ["schemaVersion", "reference", "note"];

/// Every family the corpus declares, with the case fields that are *inputs* the
/// capture authored and the case fields that are *answers* both sides give.
const FAMILIES: &[Family] = &[
    Family {
        name: "promoConstants",
        inputs: &[],
        answers: &["value"],
    },
    Family {
        name: "promoPredicate",
        inputs: &["shownCount", "instant"],
        answers: &["show"],
    },
    Family {
        name: "promoState",
        inputs: &["document"],
        answers: &["shownCount", "documentAfterWrite"],
    },
    Family {
        name: "promoProse",
        inputs: &[],
        answers: &["length", "digest"],
    },
];

/// Cases where this build answers something other than the reference, each with
/// the reason and the story that closes it.
///
/// This epic builds the instrument and nothing else: the promo has no
/// counterpart here at all, so every family answers [`None`] but the one
/// constant the shared cache file already fixes. The staleness check forces each
/// row out as EP-006 lands its surface.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "promoConstants/value/maxShownCount",
        "OPEN: US-014 ports the ceiling",
    ),
    (
        "promoConstants/value/promoStart",
        "OPEN: US-014 ports the start instant",
    ),
    (
        "promoConstants/value/promoStartEpochSeconds",
        "OPEN: as above",
    ),
    (
        "promoConstants/value/cacheSection",
        "OPEN: US-014 names the `cache.toml` section the counter lives in",
    ),
    (
        "promoConstants/value/extensionUri",
        "OPEN: US-015 ships the link; the URI and its label are identifiers rather than prose, so \
         this port reproduces them",
    ),
    ("promoConstants/value/linkLabel", "OPEN: as above"),
    (
        "promoPredicate/*",
        "OPEN: US-014 ports the three-branch predicate",
    ),
    (
        "promoState/*",
        "OPEN: US-014 ports the repository, its non-integer rejection and its sibling preservation",
    ),
    (
        "promoProse/*",
        "OPEN: US-015 writes this port's own sentences. The reference's are measured as a length \
         and a SHA-256 because `NOTICE` forbids reproducing them, so this port answers nothing \
         until it has prose of its own; \
         `this_ports_prose_never_matches_a_reference_digest` is what keeps the two apart \
         afterward",
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
    let parsed: Value = serde_json::from_str(&raw).expect("the promo corpus parses");
    let corpus = parsed
        .as_object()
        .expect("the promo corpus is an object")
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
        "promo: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// This build's answers
// --------------------------------------------------------------------------

/// The sentences this port ships in place of the reference's two.
///
/// Empty until US-015 writes them. Whatever lands here is held permanently
/// unequal to both reference digests by
/// [`this_ports_prose_never_matches_a_reference_digest`], which is how the
/// licensing boundary stays measurable without any reference text entering this
/// repository.
fn port_promo_sentences() -> &'static [&'static str] {
    &[]
}

/// What this build answers for one `family/field/case`, or [`None`] where the
/// surface the case measures does not exist here yet.
fn port_answer(family: &str, field: &str, case: &str) -> Option<Value> {
    match (family, field, case) {
        // The promo counter shares `cache.toml` with the update cache, and this
        // port already reads and writes a named section of that exact file.
        ("promoConstants", "value", "cacheFile") => {
            // Naming the file is all this reads, and the store resolves it by
            // joining rather than by touching a disk.
            let store = UpdateCacheStore::new(Path::new("vibe-home"));
            Some(Value::String(
                store.path().file_name()?.to_str()?.to_owned(),
            ))
        }
        _ => None,
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

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

/// `NOTICE` forbids shipping a reference-authored sentence, and a digest is what
/// makes that enforceable without carrying the text: every sentence this port
/// ships is compared against both reference digests and has to differ from each.
/// An empty sentence would trivially differ, so it fails too.
#[test]
fn this_ports_prose_never_matches_a_reference_digest() {
    let corpus = corpus();
    let reference = cases(&corpus, "promoProse")
        .iter()
        .map(|case| {
            (
                case_id(case).to_owned(),
                case.get("digest")
                    .and_then(Value::as_str)
                    .expect("every prose case carries a digest")
                    .to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert!(
        !reference.is_empty(),
        "the corpus records no reference prose to stay unequal to"
    );
    for sentence in port_promo_sentences() {
        assert!(
            !sentence.trim().is_empty(),
            "this port's promo prose is empty, which is not a sentence of its own"
        );
        let digest: String = Sha256::digest(sentence.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        for (name, expected) in &reference {
            assert_ne!(
                &digest, expected,
                "this port's promo prose reproduces the reference's {name} sentence, which \
                 `NOTICE` forbids"
            );
        }
    }
}

/// The two failure modes the replay exists for, proven on a report the test
/// builds rather than on the corpus: a divergence the ledger does not name has
/// to be reported with its family, its case and both values, and a ledger entry
/// whose divergence stopped reproducing has to be reported as stale.
#[test]
fn the_ledger_reports_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger = [("promoPredicate/show/named", "recorded")];
    let mut report = Report::default();
    report.check(
        "promoPredicate",
        "show",
        "unnamed",
        &Value::Bool(true),
        Some(&Value::Bool(false)),
    );
    let (unrecorded, stale) = audit(&report, "promoPredicate", &ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    let reported = &unrecorded[0];
    assert!(
        reported.starts_with("promoPredicate/show/unnamed:"),
        "{reported}"
    );
    assert!(reported.contains("reference true"), "{reported}");
    assert!(reported.contains("port false"), "{reported}");
    assert_eq!(
        stale,
        vec!["promoPredicate/show/named".to_owned()],
        "an entry whose case stopped diverging is stale"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!("promo: divergence ledger ({} entries)", DIVERGENCES.len());
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut comparisons = 0;
    for family in FAMILIES {
        let mut report = Report::default();
        run_family(&corpus, family, &mut report);
        comparisons += settle(&report, family.name);
    }
    println!(
        "promo: {comparisons} comparisons across {} families replayed at {}",
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
    if let Some(reason) = off_pin_reason(&root, "promo") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/promo-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/promo-full.json"))
        .arg("--corpus")
        .arg(repository.join("target/promo-engine-corpus.json"))
        .arg("--promo-corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the experiments capture script runs");
    assert!(
        output.status.success(),
        "the promo capture failed: {}",
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
