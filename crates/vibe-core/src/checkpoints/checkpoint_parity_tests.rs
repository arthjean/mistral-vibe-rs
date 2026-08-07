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

// ---------------------------------------------------------------------------
// The engine corpus
// ---------------------------------------------------------------------------
//
// The opcode families above pin the diff a region identity is numbered by. The
// families below pin everything built on it: which regions an edit produces,
// what each was built on, what decision is in force after dragging, what the
// file looks like with those decisions applied, where a pending change sits in
// a rendered diff, and what a truncation would restore. Each scenario is a list
// of steps `scripts/parity/checkpoints.py` drove through the reference; this
// module drives the same steps through this port and compares family by family.

use std::collections::BTreeMap;

use super::checkpointer::Checkpointer;
use super::models::{
    Change, CheckpointError, Decision, HunkAnchor, HunkSide, OpaqueReason, Owner, RegionId,
};

const ENGINE_CAPTURE_SCRIPT: &str = "scripts/parity/checkpoints.py";
const ENGINE_CORPUS_RELATIVE: &str = "tests/checkpoints/engine.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const ENGINE_SCHEMA_VERSION: u32 = 1;
/// The scenario floor this epic commits to, mirroring `MINIMUM_SCENARIOS` in
/// the capture script.
const MINIMUM_SCENARIOS: usize = 40;

/// A divergence this port accepts, with what holds it in place.
///
/// An entry that no longer matches a real divergence fails the replay, because
/// a ledger nobody prunes is a list of things that used to be true. Nothing is
/// listed today: every family below is expected to match exactly, and a
/// divergence in any of them is a different protocol rather than a tolerable
/// gap.
struct LedgerEntry {
    scenario: &'static str,
    family: &'static str,
    #[expect(dead_code, reason = "the reason documents the entry for its readers")]
    reason: &'static str,
}

const LEDGER: &[LedgerEntry] = &[];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EngineCorpus {
    schema_version: u32,
    reference_commit: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    /// The turn identifier no scenario opens, which pins what a restore plan
    /// answers for a turn the log does not carry.
    absent_turn: u64,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    name: String,
    /// The behavior the scenario was authored for, which the coverage test
    /// reads.
    covers: String,
    steps: Vec<Step>,
    /// What the reference did with each step, in order.
    outcomes: Vec<String>,
    has_open_turn: bool,
    tracked_paths: Vec<String>,
    last_turn_paths: Vec<String>,
    scopes: Vec<Owner>,
    files: Vec<FileObservation>,
    restore_plans: Vec<RestorePlanObservation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Step {
    BeginTurn {
        turn_id: u64,
    },
    PreEdit {
        path: String,
        text: Option<String>,
    },
    PostEdit {
        path: String,
        text: Option<String>,
    },
    SealTurn,
    Reconcile {
        path: String,
        text: Option<String>,
    },
    DecideRegion {
        path: String,
        version_index: u64,
        ordinal: usize,
        decision: Decision,
    },
    DecideScope {
        path: String,
        owner: Owner,
        decision: Decision,
    },
    DecideFile {
        path: String,
        decision: Decision,
    },
    DropTurnsFrom {
        turn_id: u64,
    },
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileObservation {
    path: String,
    regions: Vec<RegionObservation>,
    content: String,
    accepted_baseline: String,
    original: String,
    fully_reviewed: bool,
    anchors: Vec<AnchorObservation>,
    scopes: Vec<ScopeObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegionObservation {
    version_index: u64,
    ordinal: usize,
    owner: Owner,
    decision: Decision,
    depends_on: Vec<(u64, usize)>,
    change: ChangeObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ChangeObservation {
    /// One non-equal opcode, with both sides' spans and the lines they hold.
    Text {
        baseline_start: usize,
        baseline_line_count: usize,
        baseline_lines: String,
        current_start: usize,
        current_line_count: usize,
        current_lines: String,
    },
    /// One whole-file unit, with both sides' states.
    Opaque {
        reason: OpaqueReason,
        baseline: String,
        current: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnchorObservation {
    side: HunkSide,
    line: usize,
    regions: Vec<(u64, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeObservation {
    owner: Owner,
    /// The scope's own pending diff, or nothing when it has none left.
    diff: Option<DiffObservation>,
    anchors: Vec<AnchorObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffObservation {
    baseline: String,
    current: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestorePlanObservation {
    turn_id: u64,
    has_turn: bool,
    plan: Vec<PlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanEntry {
    path: String,
    state: String,
}

fn engine_corpus_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(ENGINE_CORPUS_RELATIVE)
}

fn engine_corpus() -> EngineCorpus {
    let raw = fs::read_to_string(engine_corpus_path()).expect("the corpus is committed");
    let corpus: EngineCorpus = serde_json::from_str(&raw).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, ENGINE_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `{ENGINE_CAPTURE_SCRIPT}`"
    );
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    assert!(
        corpus.scenarios.len() >= MINIMUM_SCENARIOS,
        "the corpus shrank to {} scenarios",
        corpus.scenarios.len()
    );
    corpus
}

/// A file state's identity, with absence answering apart from emptiness.
fn state_digest(state: &FileState) -> String {
    match state.data() {
        None => "absent".to_owned(),
        Some(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            let hash = hasher.finalize();
            let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
                use std::fmt::Write;
                let _ = write!(accumulator, "{byte:02x}");
                accumulator
            });
            format!("sha256:{}", &hex[..32])
        }
    }
}

fn state_of(text: Option<&str>) -> FileState {
    text.map_or_else(FileState::absent, FileState::from_text)
}

/// The family the reference would have refused this step with.
fn outcome_of(result: Result<(), CheckpointError>) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(
            CheckpointError::TurnAlreadyOpen
            | CheckpointError::NoOpenTurn { .. }
            | CheckpointError::DecisionDuringTurn,
        ) => "turnState",
        Err(CheckpointError::UnknownRegion { .. } | CheckpointError::PendingDecision) => {
            "fileState"
        }
        // No scenario reaches the retention ceiling, which the reference does
        // not have; a corpus step that did would be a divergence, not a family.
        Err(CheckpointError::RetentionExhausted { .. }) => "retention",
    }
}

/// One step against the log, answering what this port did with it.
fn apply_step(log: &mut Checkpointer, step: &Step) -> &'static str {
    match step {
        Step::BeginTurn { turn_id } => outcome_of(log.begin_turn(*turn_id)),
        Step::PreEdit { path, text } => {
            outcome_of(log.record_pre_edit(path, state_of(text.as_deref())))
        }
        Step::PostEdit { path, text } => {
            outcome_of(log.record_post_edit(path, state_of(text.as_deref())))
        }
        Step::SealTurn => {
            log.seal_turn();
            "ok"
        }
        Step::Reconcile { path, text } => {
            outcome_of(log.reconcile(path, state_of(text.as_deref())));
            "ok"
        }
        Step::DecideRegion {
            path,
            version_index,
            ordinal,
            decision,
        } => {
            outcome_of(log.decide_region(path, RegionId::new(*version_index, *ordinal), *decision))
        }
        Step::DecideScope {
            path,
            owner,
            decision,
        } => outcome_of(log.decide_scope(path, *owner, *decision)),
        Step::DecideFile { path, decision } => outcome_of(log.decide_file(path, *decision)),
        Step::DropTurnsFrom { turn_id } => {
            log.drop_turns_from(*turn_id);
            "ok"
        }
        Step::Clear => {
            log.clear();
            "ok"
        }
    }
}

fn anchor_observations(anchors: &[HunkAnchor]) -> Vec<AnchorObservation> {
    anchors
        .iter()
        .map(|anchor| AnchorObservation {
            side: anchor.side,
            line: anchor.line,
            regions: anchor
                .regions
                .iter()
                .map(|region| (region.version_index, region.ordinal))
                .collect(),
        })
        .collect()
}

/// Everything this port answers about one file, in the corpus's shape.
fn observe_file(log: &Checkpointer, path: &str) -> FileObservation {
    let history = log.history();
    let regions = history.regions(path);
    let mut owners: Vec<Owner> = Vec::new();
    for region in &regions {
        if !owners.contains(&region.owner) {
            owners.push(region.owner);
        }
    }
    FileObservation {
        path: path.to_owned(),
        regions: regions
            .iter()
            .map(|region| RegionObservation {
                version_index: region.region_id.version_index,
                ordinal: region.region_id.ordinal,
                owner: region.owner,
                decision: region.decision,
                depends_on: region
                    .depends_on
                    .iter()
                    .map(|dep| (dep.version_index, dep.ordinal))
                    .collect(),
                change: match &region.change {
                    Change::Text(text) => ChangeObservation::Text {
                        baseline_start: text.baseline_start,
                        baseline_line_count: text.baseline_lines.len(),
                        baseline_lines: digest(&text.baseline_lines),
                        current_start: text.current_start,
                        current_line_count: text.current_lines.len(),
                        current_lines: digest(&text.current_lines),
                    },
                    Change::Opaque(opaque) => ChangeObservation::Opaque {
                        reason: opaque.reason,
                        baseline: state_digest(&opaque.baseline),
                        current: state_digest(&opaque.current),
                    },
                },
            })
            .collect(),
        content: state_digest(&history.content(path)),
        accepted_baseline: state_digest(&history.accepted_baseline(path)),
        original: state_digest(&history.original(path)),
        fully_reviewed: history.is_fully_reviewed(path),
        anchors: anchor_observations(&history.pending_hunks(path, None)),
        scopes: owners
            .into_iter()
            .map(|owner| ScopeObservation {
                owner,
                diff: history
                    .scope_pending_diff(path, owner)
                    .map(|(baseline, current)| DiffObservation {
                        baseline: state_digest(&baseline),
                        current: state_digest(&current),
                    }),
                anchors: anchor_observations(&history.pending_hunks(path, Some(owner))),
            })
            .collect(),
    }
}

fn observe_restore_plan(log: &Checkpointer, turn_id: u64) -> RestorePlanObservation {
    let history = log.history();
    RestorePlanObservation {
        turn_id,
        has_turn: history.has_turn(turn_id),
        plan: history
            .restore_plan_to_turn(turn_id)
            .iter()
            .map(|(path, state)| PlanEntry {
                path: path.clone(),
                state: state_digest(state),
            })
            .collect(),
    }
}

/// One divergence, named by the scenario and the family it belongs to.
#[derive(Debug)]
struct Divergence {
    scenario: String,
    family: &'static str,
    detail: String,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} [{}]: {}",
            self.scenario, self.family, self.detail
        )
    }
}

/// Replays one scenario and reports every family that answered differently.
fn scenario_divergences(scenario: &Scenario) -> Vec<Divergence> {
    let mut found = Vec::new();
    let mut report = |family: &'static str, detail: String| {
        found.push(Divergence {
            scenario: scenario.name.clone(),
            family,
            detail,
        });
    };

    let mut log = Checkpointer::new();
    let outcomes: Vec<&str> = scenario
        .steps
        .iter()
        .map(|step| apply_step(&mut log, step))
        .collect();
    if outcomes != scenario.outcomes {
        report(
            "steps",
            format!(
                "the reference answered {:?}, this port {:?}",
                scenario.outcomes, outcomes
            ),
        );
    }

    let history = log.history();
    if history.tracked_paths() != scenario.tracked_paths {
        report(
            "logShape",
            format!(
                "the reference tracked {:?}, this port {:?}",
                scenario.tracked_paths,
                history.tracked_paths()
            ),
        );
    }
    if history.last_turn_paths() != scenario.last_turn_paths {
        report(
            "logShape",
            format!(
                "the reference's last turn tracked {:?}, this port's {:?}",
                scenario.last_turn_paths,
                history.last_turn_paths()
            ),
        );
    }
    if history.scopes() != scenario.scopes {
        report(
            "logShape",
            format!(
                "the reference held the slots {:?}, this port {:?}",
                scenario.scopes,
                history.scopes()
            ),
        );
    }
    if log.has_open_turn() != scenario.has_open_turn {
        report(
            "logShape",
            format!(
                "the reference left the turn {}, this port {}",
                if scenario.has_open_turn {
                    "open"
                } else {
                    "closed"
                },
                if log.has_open_turn() {
                    "open"
                } else {
                    "closed"
                }
            ),
        );
    }
    drop(history);

    for expected in &scenario.files {
        let observed = observe_file(&log, &expected.path);
        if observed.regions != expected.regions {
            report(
                "regions",
                format!(
                    "{}: the reference held {:?}, this port {:?}",
                    expected.path, expected.regions, observed.regions
                ),
            );
        }
        if (
            &observed.content,
            &observed.accepted_baseline,
            &observed.original,
            observed.fully_reviewed,
        ) != (
            &expected.content,
            &expected.accepted_baseline,
            &expected.original,
            expected.fully_reviewed,
        ) {
            report(
                "projections",
                format!(
                    "{}: the reference projected content {} baseline {} original {} reviewed {}, \
                     this port {} {} {} {}",
                    expected.path,
                    expected.content,
                    expected.accepted_baseline,
                    expected.original,
                    expected.fully_reviewed,
                    observed.content,
                    observed.accepted_baseline,
                    observed.original,
                    observed.fully_reviewed
                ),
            );
        }
        if observed.anchors != expected.anchors {
            report(
                "anchors",
                format!(
                    "{}: the reference anchored {:?}, this port {:?}",
                    expected.path, expected.anchors, observed.anchors
                ),
            );
        }
        if observed.scopes != expected.scopes {
            report(
                "scopes",
                format!(
                    "{}: the reference scoped {:?}, this port {:?}",
                    expected.path, expected.scopes, observed.scopes
                ),
            );
        }
    }
    let observed_paths: Vec<String> = scenario
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    if observed_paths != scenario.tracked_paths {
        report(
            "logShape",
            format!(
                "the corpus observed {observed_paths:?} but recorded {:?} as tracked",
                scenario.tracked_paths
            ),
        );
    }

    for expected in &scenario.restore_plans {
        let observed = observe_restore_plan(&log, expected.turn_id);
        if &observed != expected {
            report(
                "restorePlans",
                format!(
                    "turn {}: the reference planned {:?} (has_turn {}), this port {:?} (has_turn \
                     {})",
                    expected.turn_id,
                    expected.plan,
                    expected.has_turn,
                    observed.plan,
                    observed.has_turn
                ),
            );
        }
    }
    found
}

/// The divergences left after the ledger has taken the ones it accounts for,
/// with the ledger entries nothing matched.
fn against_ledger(
    divergences: Vec<Divergence>,
    ledger: &[LedgerEntry],
) -> (Vec<Divergence>, Vec<String>) {
    let mut unaccounted = Vec::new();
    let mut matched = vec![false; ledger.len()];
    for divergence in divergences {
        match ledger.iter().position(|entry| {
            entry.scenario == divergence.scenario && entry.family == divergence.family
        }) {
            Some(index) => matched[index] = true,
            None => unaccounted.push(divergence),
        }
    }
    let stale = ledger
        .iter()
        .zip(matched)
        .filter(|(_entry, hit)| !hit)
        .map(|(entry, _hit)| format!("{} [{}]", entry.scenario, entry.family))
        .collect();
    (unaccounted, stale)
}

#[test]
fn the_engine_matches_the_reference_over_every_scenario() {
    let corpus = engine_corpus();
    let mut per_family: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut all = Vec::new();
    for scenario in &corpus.scenarios {
        let divergences = scenario_divergences(scenario);
        let diverged: Vec<&'static str> = divergences
            .iter()
            .map(|divergence| divergence.family)
            .collect();
        for family in [
            "steps",
            "logShape",
            "regions",
            "projections",
            "anchors",
            "scopes",
            "restorePlans",
        ] {
            let counts = per_family.entry(family).or_default();
            if diverged.contains(&family) {
                counts.1 += 1;
            } else {
                counts.0 += 1;
            }
        }
        all.extend(divergences);
    }
    let (unaccounted, stale) = against_ledger(all, LEDGER);

    println!(
        "checkpoint engine: {} scenarios replayed against the reference at {}",
        corpus.scenarios.len(),
        &corpus.reference_commit[..12]
    );
    for (family, (conforming, divergent)) in &per_family {
        println!("  {family}: {conforming} conforming, {divergent} divergent");
    }
    assert!(
        stale.is_empty(),
        "the ledger names divergences that no longer happen, so it is stale:\n  {}",
        stale.join("\n  ")
    );
    assert!(
        unaccounted.is_empty(),
        "engine divergences outside the ledger:\n  {}",
        unaccounted
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

#[test]
fn the_engine_corpus_covers_the_behaviors_the_epic_names() {
    let corpus = engine_corpus();
    let covered: Vec<&str> = corpus
        .scenarios
        .iter()
        .map(|scenario| scenario.covers.as_str())
        .collect();
    for required in [
        "attribution",
        "per-turn-revert",
        "per-region-revert",
        "incremental-revert",
        "dependency-cascade",
        "opaque",
        "turn-gate",
        "manual-edit",
        "manual-edit-dependency",
        "ratchet",
        "scope-diff",
        "bulk-decisions",
        "truncation",
    ] {
        assert!(
            covered.contains(&required),
            "the corpus no longer covers {required}"
        );
    }

    // The opaque family has to carry all three of its causes, or the reason
    // field would be pinned by one case.
    let reasons: Vec<OpaqueReason> = corpus
        .scenarios
        .iter()
        .flat_map(|scenario| &scenario.files)
        .flat_map(|file| &file.regions)
        .filter_map(|region| match region.change {
            ChangeObservation::Opaque { reason, .. } => Some(reason),
            ChangeObservation::Text { .. } => None,
        })
        .collect();
    assert!(reasons.contains(&OpaqueReason::Missing));
    assert!(reasons.contains(&OpaqueReason::BinaryOrUndecodable));

    // A turn refusal and a target refusal are different families, and both have
    // to be exercised or the outcome comparison would be a formality.
    let outcomes: Vec<&str> = corpus
        .scenarios
        .iter()
        .flat_map(|scenario| &scenario.outcomes)
        .map(String::as_str)
        .collect();
    assert!(outcomes.contains(&"turnState"));
    assert!(outcomes.contains(&"fileState"));

    // A hand edit has to appear as an owner somewhere, and a dependency edge
    // has to be non-empty somewhere.
    assert!(
        corpus
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.scopes)
            .any(|owner| matches!(owner, Owner::Manual { .. })),
        "no scenario produced a hand edit"
    );
    assert!(
        corpus
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.files)
            .flat_map(|file| &file.regions)
            .any(|region| !region.depends_on.is_empty()),
        "no scenario produced a dependency edge"
    );
    assert!(
        corpus.scenarios.iter().any(|scenario| scenario
            .restore_plans
            .iter()
            .any(|plan| plan.turn_id == corpus.absent_turn && !plan.has_turn)),
        "no scenario asks what a turn the log does not carry restores"
    );
}

#[test]
fn a_divergent_scenario_is_reported_by_name_and_family() {
    let corpus = engine_corpus();
    let mut scenario = corpus
        .scenarios
        .iter()
        .find(|scenario| scenario.name == "per-region-revert-keeps-the-sibling")
        .expect("the scenario is committed")
        .clone();
    assert!(scenario_divergences(&scenario).is_empty());

    // What a real divergence looks like: the same steps, one decision moved.
    scenario.files[0].regions[0].decision = Decision::Keep;
    let found = scenario_divergences(&scenario);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].family, "regions");
    assert_eq!(found[0].scenario, "per-region-revert-keeps-the-sibling");
}

#[test]
fn a_ledger_entry_nothing_diverges_on_is_reported_as_stale() {
    let ledger = &[LedgerEntry {
        scenario: "one-turn-two-regions",
        family: "regions",
        reason: "a divergence this port does not actually have",
    }];

    let (unaccounted, stale) = against_ledger(Vec::new(), ledger);

    assert!(unaccounted.is_empty());
    assert_eq!(stale, vec!["one-turn-two-regions [regions]"]);

    // And an entry that does match takes its divergence out of the report.
    let divergence = Divergence {
        scenario: "one-turn-two-regions".to_owned(),
        family: "regions",
        detail: "for the sake of the argument".to_owned(),
    };
    let (unaccounted, stale) = against_ledger(vec![divergence], ledger);
    assert!(unaccounted.is_empty());
    assert!(stale.is_empty());
}

#[test]
fn the_committed_engine_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "checkpoint engine oracle") {
        eprintln!("{reason}");
        return;
    }
    let Some(interpreter) = pinned_interpreter(&root) else {
        eprintln!(
            "skipping the live checkpoint engine probe: no interpreter that can import the \
             reference"
        );
        return;
    };
    let workspace = repo_root();
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("engine.json");
    let output = Command::new(&interpreter)
        .arg(workspace.join(ENGINE_CAPTURE_SCRIPT))
        .arg("--reference")
        .arg(&root)
        .arg("--output")
        .arg(&captured)
        .current_dir(&workspace)
        .output()
        .expect("the capture script runs");
    assert!(
        output.status.success(),
        "{ENGINE_CAPTURE_SCRIPT} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recaptured = fs::read_to_string(&captured).expect("captured corpus reads");
    let committed = fs::read_to_string(engine_corpus_path()).expect("committed corpus reads");
    assert_eq!(
        recaptured, committed,
        "the committed corpus no longer matches the pinned reference; regenerate it with \
         `{ENGINE_CAPTURE_SCRIPT}`"
    );
}
