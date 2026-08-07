//! Differential oracle for the checkpoint engine.
//!
//! A region identity is a producing edit's sequence number paired with that
//! region's position in the edit's opcode list, and a client sends the pair
//! back across turns. So the opcode list is a contract: a diff of equal quality
//! but different shape would resolve a client's target to a different change,
//! silently. This module replays what the pinned reference answered for a set
//! of fixtures, so a divergence is a test failure naming the fixture rather
//! than a client bug reported months later.
//!
//! Two families are replayed, both captured by
//! `scripts/parity/checkpoint_opcodes.py` into
//! `crates/vibe-core/tests/checkpoints/opcodes.json`:
//!
//! - `lineFixtures` records what `_decode_lines` answered for one input, which
//!   is where the eight non-newline boundary characters and the binary and
//!   absent states are decided.
//! - `opcodeFixtures` records what `SequenceMatcher(autojunk=False)` answered
//!   over two of those line sequences, with both sides' splits pinned beside
//!   the opcodes so a divergence says which of the two moved.
//!
//! The replay is unconditional. The corpus carries inputs this repository
//! authored, digests of the line sequences the reference produced from them,
//! and the opcode tuples, so it ships no reference prose and needs no checkout
//! to answer. Only the probe that recaptures from the pinned reference skips
//! when the checkout is absent or off-pin.
//!
//! There is no ledger here, and there is not meant to be one: an opcode
//! divergence is not a tolerable gap, it is a different protocol.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::lines::{FileState, decode_lines};
use super::matcher::SequenceMatcher;
use crate::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

const CAPTURE_SCRIPT: &str = "scripts/parity/checkpoint_opcodes.py";
const CORPUS_RELATIVE: &str = "tests/checkpoints/opcodes.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The opcode-fixture floor this epic commits to, mirroring
/// `MINIMUM_OPCODE_FIXTURES` in the capture script, so a regeneration that
/// captured almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_OPCODE_FIXTURES: usize = 30;
/// The line-fixture floor: one per boundary character on both positions, plus
/// the absent, empty, binary and line-ending cases.
const MINIMUM_LINE_FIXTURES: usize = 24;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference_commit: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    line_fixtures: Vec<LineFixture>,
    opcode_fixtures: Vec<OpcodeFixture>,
}

/// One input and the lines the reference split it into.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LineFixture {
    name: String,
    /// The input, or [`None`] for a file that does not exist.
    text: Option<String>,
    /// Whether the reference refused to split it into lines.
    binary: bool,
    /// The split, or [`None`] when `binary` is true.
    lines: Option<LineSummary>,
}

/// A line sequence recorded without a second copy of its text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct LineSummary {
    count: usize,
    /// Each line's length in characters, which locates a divergent boundary.
    lengths: Vec<usize>,
    /// The sequence's identity, boundaries included.
    digest: String,
}

/// Two inputs and the opcodes the reference produced between them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpcodeFixture {
    name: String,
    a: Side,
    b: Side,
    opcodes: Vec<RecordedOpcode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Side {
    text: String,
    lines: LineSummary,
}

/// One captured `(tag, i1, i2, j1, j2)` tuple, in the shape the reference
/// emits it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RecordedOpcode(String, usize, usize, usize, usize);

impl std::fmt::Display for RecordedOpcode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "({}, {}, {}, {}, {})",
            self.0, self.1, self.2, self.3, self.4
        )
    }
}

fn render(opcodes: &[RecordedOpcode]) -> String {
    opcodes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let raw = fs::read_to_string(corpus_path()).expect("the corpus is committed");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `{CAPTURE_SCRIPT}`"
    );
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    assert!(
        corpus.line_fixtures.len() >= MINIMUM_LINE_FIXTURES,
        "the corpus shrank to {} line fixtures",
        corpus.line_fixtures.len()
    );
    assert!(
        corpus.opcode_fixtures.len() >= MINIMUM_OPCODE_FIXTURES,
        "the corpus shrank to {} opcode fixtures",
        corpus.opcode_fixtures.len()
    );
    corpus
}

/// A line sequence's identity, mirroring `digest` in the capture script.
///
/// Digesting the concatenation would not do: joining keepends lines rebuilds
/// the input exactly, so two different splits of the same text would digest
/// alike. Prefixing each line with its length puts the boundaries in the hash.
fn digest(lines: &[String]) -> String {
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.chars().count().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(line.as_bytes());
    }
    let hash = hasher.finalize();
    let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
        use std::fmt::Write;
        let _ = write!(accumulator, "{byte:02x}");
        accumulator
    });
    format!("sha256:{}", &hex[..32])
}

fn summarize(lines: &[String]) -> LineSummary {
    LineSummary {
        count: lines.len(),
        lengths: lines.iter().map(|line| line.chars().count()).collect(),
        digest: digest(lines),
    }
}

/// This port's answer for one recorded input, or [`None`] when it calls the
/// input binary.
fn observed_lines(text: Option<&str>) -> Option<Vec<String>> {
    let state = text.map_or_else(FileState::absent, FileState::from_text);
    decode_lines(&state)
}

/// Every way this port's split differs from the recorded one, named.
fn line_divergences(fixture: &LineFixture) -> Vec<String> {
    let name = &fixture.name;
    let observed = observed_lines(fixture.text.as_deref());
    match (&fixture.lines, observed) {
        (None, None) => Vec::new(),
        (Some(expected), Some(lines)) => {
            let summary = summarize(&lines);
            if &summary == expected {
                return Vec::new();
            }
            let mut found = Vec::new();
            if summary.count != expected.count {
                found.push(format!(
                    "{name}: the reference split it into {} lines, this port into {}",
                    expected.count, summary.count
                ));
            }
            if summary.lengths != expected.lengths {
                found.push(format!(
                    "{name}: the reference produced line lengths {:?}, this port {:?}",
                    expected.lengths, summary.lengths
                ));
            }
            if summary.digest != expected.digest {
                found.push(format!(
                    "{name}: the reference produced the line sequence {}, this port {}",
                    expected.digest, summary.digest
                ));
            }
            found
        }
        (Some(_), None) => vec![format!(
            "{name}: the reference split it into lines, this port called it binary"
        )],
        (None, Some(lines)) => vec![format!(
            "{name}: the reference called it binary, this port split it into {} lines",
            lines.len()
        )],
    }
}

/// Every way this port's opcodes differ from the recorded ones, named.
///
/// The two sides' splits are checked first, because an opcode divergence
/// downstream of a split divergence would report the wrong cause.
fn opcode_divergences(fixture: &OpcodeFixture) -> Vec<String> {
    let name = &fixture.name;
    let mut found = Vec::new();
    let mut sides = Vec::new();
    for (label, side) in [("a", &fixture.a), ("b", &fixture.b)] {
        let Some(lines) = observed_lines(Some(&side.text)) else {
            found.push(format!(
                "{name}: this port called side {label} binary, so it produced no opcodes"
            ));
            continue;
        };
        let summary = summarize(&lines);
        if summary != side.lines {
            found.push(format!(
                "{name}: side {label} split into {} lines with sequence {}, the reference into {} \
                 with {}",
                summary.count, summary.digest, side.lines.count, side.lines.digest
            ));
        }
        sides.push(lines);
    }
    if sides.len() != 2 {
        return found;
    }

    let produced: Vec<RecordedOpcode> = SequenceMatcher::new(&sides[0], &sides[1])
        .opcodes()
        .into_iter()
        .map(|opcode| {
            RecordedOpcode(
                opcode.tag.as_str().to_owned(),
                opcode.i1,
                opcode.i2,
                opcode.j1,
                opcode.j2,
            )
        })
        .collect();
    if produced != fixture.opcodes {
        found.push(format!(
            "{name}: the reference produced [{}], this port produced [{}]",
            render(&fixture.opcodes),
            render(&produced)
        ));
    }
    found
}

#[test]
fn line_splitting_matches_the_reference() {
    let corpus = corpus();
    let mut conforming = 0;
    let mut divergent = Vec::new();
    for fixture in &corpus.line_fixtures {
        let found = line_divergences(fixture);
        if found.is_empty() {
            conforming += 1;
        } else {
            divergent.extend(found);
        }
    }
    println!(
        "checkpoint lines: {conforming}/{} fixtures match the reference at {}, {} divergent",
        corpus.line_fixtures.len(),
        &corpus.reference_commit[..12],
        corpus.line_fixtures.len() - conforming
    );
    assert!(
        divergent.is_empty(),
        "line divergences:\n  {}",
        divergent.join("\n  ")
    );
}

#[test]
fn opcodes_match_the_reference() {
    let corpus = corpus();
    let mut conforming = 0;
    let mut divergent = Vec::new();
    for fixture in &corpus.opcode_fixtures {
        let found = opcode_divergences(fixture);
        if found.is_empty() {
            conforming += 1;
        } else {
            divergent.extend(found);
        }
    }
    println!(
        "checkpoint opcodes: {conforming}/{} fixtures match the reference at {}, {} divergent",
        corpus.opcode_fixtures.len(),
        &corpus.reference_commit[..12],
        corpus.opcode_fixtures.len() - conforming
    );
    assert!(
        divergent.is_empty(),
        "opcode divergences:\n  {}",
        divergent.join("\n  ")
    );
}

#[test]
fn the_corpus_covers_the_cases_the_algorithm_turns_on() {
    let corpus = corpus();
    let opcode_names: Vec<&str> = corpus
        .opcode_fixtures
        .iter()
        .map(|fixture| fixture.name.as_str())
        .collect();
    for required in [
        "identical-multi-line",
        "both-empty",
        "empty-into-content",
        "content-into-empty",
        "disjoint-lines",
        "shared-prefix",
        "shared-suffix",
        "repeated-line-twice",
        "above-the-threshold-identical",
        "above-the-threshold-with-a-repeated-line",
        "above-the-threshold-popular-line-is-the-only-match",
    ] {
        assert!(
            opcode_names.contains(&required),
            "the corpus no longer covers {required}"
        );
    }

    // The heuristic case has to be a real one: a sequence CPython would have
    // filtered, whose only match is the element it would have dropped.
    let heuristic = corpus
        .opcode_fixtures
        .iter()
        .find(|fixture| fixture.name == "above-the-threshold-popular-line-is-the-only-match")
        .expect("the heuristic fixture is committed");
    assert!(
        heuristic.b.lines.count >= 200,
        "below the 200-element threshold, so the heuristic would not have triggered"
    );
    assert!(
        heuristic.opcodes.iter().any(|opcode| opcode.0 == "equal"),
        "the reference found no match, so this fixture proves nothing about the heuristic"
    );

    let line_names: Vec<&str> = corpus
        .line_fixtures
        .iter()
        .map(|fixture| fixture.name.as_str())
        .collect();
    for required in [
        "absent",
        "empty",
        "vertical-tab",
        "form-feed",
        "file-separator",
        "group-separator",
        "record-separator",
        "next-line",
        "line-separator",
        "paragraph-separator",
        "binary-with-an-embedded-nul",
    ] {
        assert!(
            line_names.contains(&required),
            "the corpus no longer covers {required}"
        );
    }
    assert!(
        corpus.line_fixtures.iter().any(|fixture| fixture.binary),
        "no fixture records the binary answer"
    );
}

#[test]
fn a_divergent_fixture_is_reported_by_name_with_both_opcode_lists() {
    let corpus = corpus();
    let mut fixture = corpus
        .opcode_fixtures
        .iter()
        .find(|fixture| fixture.name == "shared-prefix-and-suffix")
        .expect("the fixture is committed")
        .clone();
    assert!(opcode_divergences(&fixture).is_empty());

    // What a real divergence looks like: the same inputs, one opcode moved.
    fixture.opcodes = vec![RecordedOpcode("replace".to_owned(), 0, 3, 0, 3)];
    let found = opcode_divergences(&fixture);
    assert_eq!(found.len(), 1, "{found:?}");
    let report = &found[0];
    assert!(report.contains("shared-prefix-and-suffix"), "{report}");
    assert!(report.contains("(replace, 0, 3, 0, 3)"), "{report}");
    assert!(report.contains("(equal, 0, 1, 0, 1)"), "{report}");
}

#[test]
fn a_divergent_split_is_reported_before_the_opcodes_it_would_move() {
    let corpus = corpus();
    let mut fixture = corpus
        .opcode_fixtures
        .iter()
        .find(|fixture| fixture.name == "shared-prefix")
        .expect("the fixture is committed")
        .clone();
    fixture.a.lines.count += 1;
    fixture.a.lines.digest = "sha256:0000000000000000000000000000000".to_owned();
    let found = opcode_divergences(&fixture);
    assert!(
        found.iter().any(|report| report.contains("side a")),
        "{found:?}"
    );
}

#[test]
fn a_divergent_line_fixture_is_reported_by_name() {
    let corpus = corpus();
    let mut fixture = corpus
        .line_fixtures
        .iter()
        .find(|fixture| fixture.name == "form-feed")
        .expect("the fixture is committed")
        .clone();
    assert!(line_divergences(&fixture).is_empty());

    // What a lost boundary character looks like: one line instead of two.
    fixture.lines = Some(LineSummary {
        count: 1,
        lengths: vec![9],
        digest: "sha256:0000000000000000000000000000000".to_owned(),
    });
    let found = line_divergences(&fixture);
    assert!(!found.is_empty());
    assert!(
        found.iter().all(|report| report.starts_with("form-feed:")),
        "{found:?}"
    );
}

#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "checkpoint opcode oracle") {
        eprintln!("{reason}");
        return;
    }
    let Some(interpreter) = pinned_interpreter(&root) else {
        eprintln!(
            "skipping the live checkpoint opcode probe: no interpreter that can import the \
             reference"
        );
        return;
    };
    let workspace = repo_root();
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("opcodes.json");
    let output = Command::new(&interpreter)
        .arg(workspace.join(CAPTURE_SCRIPT))
        .arg("--reference")
        .arg(&root)
        .arg("--output")
        .arg(&captured)
        .current_dir(&workspace)
        .output()
        .expect("the capture script runs");
    assert!(
        output.status.success(),
        "{CAPTURE_SCRIPT} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recaptured = fs::read_to_string(&captured).expect("captured corpus reads");
    let committed = fs::read_to_string(corpus_path()).expect("committed corpus reads");
    assert_eq!(
        recaptured, committed,
        "the committed corpus no longer matches the pinned reference; regenerate it with \
         `{CAPTURE_SCRIPT}`"
    );
}
