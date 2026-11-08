//! Differential oracle for the compaction the reference still runs in Python.
//!
//! `scripts/parity/compaction.py` drives the reference's own summary parser
//! over scripted inputs and records what it answers. This module replays that
//! corpus against the port's.
//!
//! Through 2.26.0 the corpus also held the token counter, the truncation, the
//! round trimmer, the envelope and the manager's decision tree. At 3.0.0 the
//! reference moved all of them into its Rust harness
//! (`harness/core/src/core/features/compaction/`), which the capture cannot
//! drive in process, so those families left the corpus with schema version 4.
//! The port's counterparts keep their own unit tests beside them.
//!
//! The corpus is committed and replayed unconditionally: every value in it is a
//! case input or a substring of one, so no reference-authored sentence ships,
//! which is what `NOTICE` allows. Only the live recapture probe skips, and it
//! names the pin and the way back when it does.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::parity::is_pin_commit;
use crate::parity::replay::{Class, PatternDivergence, Tally, differences, entries};

use super::context::extract_summary;

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/compaction/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/compaction.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 4;
/// The scenario floor, every case the capture's table scripts, so a
/// regeneration that captured almost nothing fails instead of reporting a
/// clean but empty run.
const MINIMUM_SCENARIOS: usize = 16;

/// The differences this replay accepts. Every other case is compared exactly:
/// the parser answers with a substring of the case's own text, so there is no
/// prose to ledger.
const LEDGER: &[PatternDivergence] = &[PatternDivergence {
    scenario: "summaryExtractions/separators-*",
    pointer: "/summary",
    count: 2,
    row: "26",
    class: Class::Gap,
    reason: "the reference trims the summary with Python's `str.strip`, which counts \
             U+001C..U+001F as whitespace; this port trims with `str::trim`, which keeps them",
}];

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    summary_extractions: Vec<SummaryExtraction>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SummaryExtraction {
    case: String,
    text: String,
    summary: Option<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// The committed corpus text, which [`Tally::new`] holds to its manifest entry.
fn corpus_text() -> String {
    let path = repo_root().join(CORPUS_RELATIVE);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()))
}

fn corpus(raw: &str) -> Corpus {
    let corpus: Corpus = serde_json::from_str(raw).expect("the compaction corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert!(
        is_pin_commit(&corpus.reference.commit),
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_every_family_the_reference_answered() {
    let raw = corpus_text();
    let mut tally = Tally::new(&raw, entries(LEDGER));
    let corpus = corpus(&raw);

    let total = corpus.summary_extractions.len();
    assert!(
        total >= MINIMUM_SCENARIOS,
        "the corpus replays {total} scenarios, below the {MINIMUM_SCENARIOS} its table scripts; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
    for case in &corpus.summary_extractions {
        let reference = json!({ "summary": case.summary });
        let port = json!({ "summary": extract_summary(&case.text) });
        let found = differences(&reference, &port);
        tally.record(
            &format!("summaryExtractions/{}", case.case),
            &reference,
            &port,
            found,
        );
    }
    tally.conclude("compaction parity:");
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some(root) = crate::parity::replay::pinned_reference("compaction") else {
        return;
    };
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/compaction-recapture.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(&recaptured)
        .arg("--corpus")
        .arg(repository.join("target/compaction-corpus.json"))
        .current_dir(&repository)
        .output()
        .expect("the compaction capture script runs");
    assert!(
        output.status.success(),
        "the compaction capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(repository.join("target/compaction-corpus.json"))
        .expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}
