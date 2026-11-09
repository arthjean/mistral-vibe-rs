//! The parity manifest: where every committed corpus came from.
//!
//! `scripts/parity/manifest.json` lists every corpus a replay reads, with the
//! driver command that regenerates it, the test that replays it, the scorecard
//! row it answers for, the reference commit it was captured from, its SHA-256,
//! and a floor per family of observations. `scripts/parity/repin.py` is the
//! only sanctioned writer of the manifest and of the corpora it lists;
//! `scripts/parity/provenance.py` states the rules in Python, and this module
//! states the same rules for the replays and the meta-test, so verifying the
//! manifest never needs Python.
//!
//! A replay that reads a corpus asks [`assert_corpus`] (or, through
//! [`super::replay::Replay`], [`assert_replayed`]) before it compares anything,
//! so a corpus that is unlisted, edited outside the capture tool, captured
//! from a commit no pin accepts, or shrunk below its floor fails its own
//! replay rather than passing on whatever it holds.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the manifest is test machinery: a check that fails is the calling test failing"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::replay::repository;
use super::{PREVIOUS_COMMIT, REFERENCE_COMMIT};

/// The manifest, relative to the workspace root.
pub const MANIFEST: &str = "scripts/parity/manifest.json";

/// The manifest schema this module reads.
pub const MANIFEST_SCHEMA: u32 = 1;

/// The command a report names for the only sanctioned way to change a corpus.
pub const REPIN: &str = "python3 scripts/parity/repin.py";

/// Top-level keys that describe a corpus rather than hold its observations, as
/// `provenance.METADATA_KEYS` lists them. `unavailable` lists what the host
/// could not capture: it shrinking is progress, not lost coverage.
pub const METADATA_KEYS: [&str; 8] = [
    "commit",
    "note",
    "platform",
    "reference",
    "referenceCommit",
    "schemaVersion",
    "unavailable",
    "version",
];

/// The share of a family, in percent, a recapture may keep before its floor
/// stops it.
pub const FLOOR_PERCENT: usize = 95;

/// The whole manifest.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(default)]
    pub note: Option<String>,
    /// The corpora a rolling re-pin has not recaptured yet, which may still
    /// record [`PREVIOUS_COMMIT`]; empty outside a transition.
    #[serde(default)]
    pub transition: Vec<String>,
    pub entries: Vec<ManifestEntry>,
}

/// Whether an entry is captured by a driver, merged by hand from what a
/// driver measures, or owned by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Written whole by its driver through `repin.py capture`.
    Corpus,
    /// Merged by hand around what its driver prints, then recorded with
    /// `repin.py capture --hand-assembled`; checked like a corpus.
    Assembled,
    /// Owned by hand: its commit, when it records one, is checked, its
    /// content is not.
    Fixture,
}

/// One corpus or fixture.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestEntry {
    /// The file, relative to the workspace root.
    pub corpus: String,
    pub kind: Kind,
    /// The script and arguments that regenerate the corpus.
    #[serde(default)]
    pub driver: Option<Vec<String>>,
    /// The flag through which `repin.py capture` stages the driver's output;
    /// none when the driver writes in place.
    #[serde(default)]
    pub output: Option<String>,
    /// The directory `output` names when the driver writes a whole directory.
    #[serde(default)]
    pub output_root: Option<String>,
    /// `<file>::<function>`, the `#[test]` that replays it.
    pub test: String,
    /// The `docs/parity.md` row it answers for.
    pub row: String,
    #[serde(default)]
    pub commit: Option<String>,
    /// Set when `repin.py capture --commit` recorded a commit other than the
    /// pin.
    #[serde(default)]
    pub off_target: bool,
    #[serde(default)]
    pub sha256: Option<String>,
    /// How many observations the corpus held when it was captured.
    #[serde(default)]
    pub count: Option<usize>,
    /// The least each family may hold.
    #[serde(default)]
    pub floors: BTreeMap<String, usize>,
    /// What a capture of this entry needs to know.
    #[serde(default)]
    pub note: Option<String>,
    /// The measured debt of the corpus ([`super::debt`]), when it keeps one.
    #[serde(default)]
    pub debt: Option<DebtRecord>,
}

/// Where a corpus's measured debt came from: what `repin.py debt` wrote.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DebtRecord {
    /// The SHA-256 of the debt file.
    pub sha256: String,
    /// The SHA-256 of the corpus the debt was measured against, which has to
    /// be the corpus's own: a recapture re-measures the debt.
    pub measured_against: String,
}

impl Manifest {
    /// The commits the corpus at `corpus` may record: [`REFERENCE_COMMIT`],
    /// then [`PREVIOUS_COMMIT`] while the transition list names it.
    #[must_use]
    pub fn accepted_for(&self, corpus: &str) -> Vec<&'static str> {
        accepted_commits(&self.transition, PREVIOUS_COMMIT, corpus)
    }

    /// The commit the corpus at `corpus` records according to the manifest,
    /// or [`None`] when no entry lists it or the entry records none.
    #[must_use]
    pub fn commit_of(&self, corpus: &str) -> Option<&str> {
        self.entry(corpus).and_then(|entry| entry.commit.as_deref())
    }

    /// The entry listing `corpus`, a workspace-relative path.
    #[must_use]
    pub fn entry(&self, corpus: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|entry| entry.corpus == corpus)
    }
}

/// The commits a corpus may record when `transition` lists the corpora still
/// at `previous`: the pin's target, and `previous` for a corpus the list names.
#[must_use]
pub fn accepted_commits<'p>(
    transition: &[String],
    previous: &'p str,
    corpus: &str,
) -> Vec<&'p str> {
    let listed = !previous.is_empty() && transition.iter().any(|named| named == corpus);
    std::iter::once(REFERENCE_COMMIT)
        .chain(listed.then_some(previous))
        .collect()
}

/// Every rule of a rolling re-pin that `manifest` breaks while the pin leaves
/// `previous` ([`PREVIOUS_COMMIT`], empty outside a transition):
///
/// - a non-empty `previous` needs a non-empty transition list, and a
///   non-empty list needs a `previous`, so a finished transition is closed by
///   clearing [`PREVIOUS_COMMIT`] and never stays open by accident;
/// - every corpus the list names is a manifest entry, named once;
/// - the list names no corpus already at the target, which `repin.py capture`
///   drops from it.
#[must_use]
pub fn transition_problems(manifest: &Manifest, previous: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !previous.is_empty() && manifest.transition.is_empty() {
        problems.push(format!(
            "the transition list is empty but the pin still names the previous commit \
             {previous}; clear `PREVIOUS_COMMIT` in crates/vibe-core/src/parity.rs and \
             scripts/parity/pin.py to close the transition"
        ));
    }
    if previous.is_empty() && !manifest.transition.is_empty() {
        problems.push(format!(
            "the transition list names {} corpora but `PREVIOUS_COMMIT` is empty; declare the \
             commit the re-pin left in both pin sources, or empty the list",
            manifest.transition.len()
        ));
    }
    if !previous.is_empty() && previous == REFERENCE_COMMIT {
        problems.push(format!(
            "`PREVIOUS_COMMIT` is the target {REFERENCE_COMMIT}; a transition leaves another \
             commit"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for corpus in &manifest.transition {
        if !seen.insert(corpus.as_str()) {
            problems.push(format!("the transition list names {corpus} twice"));
        }
        match manifest.entry(corpus) {
            None => problems.push(format!(
                "the transition list names {corpus}, which is not a manifest entry"
            )),
            Some(entry) if entry.commit.as_deref() == Some(REFERENCE_COMMIT) => {
                problems.push(format!(
                    "the transition list names {corpus}, which is already at the target \
                     {REFERENCE_COMMIT}; drop it from the list"
                ));
            }
            Some(_) => {}
        }
    }
    problems
}

/// The committed manifest.
#[must_use]
pub fn load() -> Manifest {
    let path = repository().join(MANIFEST);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the parity manifest {} is unreadable: {error}",
            path.display()
        )
    });
    let manifest: Manifest = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the parity manifest {MANIFEST} does not parse: {error}"));
    assert_eq!(
        manifest.schema_version, MANIFEST_SCHEMA,
        "{MANIFEST} is at schema {}, not {MANIFEST_SCHEMA}",
        manifest.schema_version
    );
    manifest
}

/// The reference commit `corpus` says it was captured from: its
/// `reference.commit`, else its `referenceCommit`, else its `commit`.
#[must_use]
pub fn recorded_commit(corpus: &Value) -> Option<&str> {
    corpus
        .pointer("/reference/commit")
        .and_then(Value::as_str)
        .or_else(|| corpus.get("referenceCommit").and_then(Value::as_str))
        .or_else(|| corpus.get("commit").and_then(Value::as_str))
}

/// Whether `corpus` says it came from the reference: a `reference` key or a
/// recorded commit. Such a file under a crate's `tests/` has to be listed in
/// the manifest.
#[must_use]
pub fn names_reference(corpus: &Value) -> bool {
    corpus.get("reference").is_some() || recorded_commit(corpus).is_some()
}

/// How many observations each top-level family of `corpus` holds: the length
/// of every array or object outside [`METADATA_KEYS`].
#[must_use]
pub fn families(corpus: &Value) -> BTreeMap<String, usize> {
    corpus
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !METADATA_KEYS.contains(&key.as_str()))
        .filter_map(|(key, value)| {
            let count = match value {
                Value::Array(items) => items.len(),
                Value::Object(items) => items.len(),
                _ => return None,
            };
            Some((key.clone(), count))
        })
        .collect()
}

/// The weakest floor a family of `count` observations may carry.
#[must_use]
pub fn minimum_floor(count: usize) -> usize {
    (count * FLOOR_PERCENT).div_ceil(100)
}

/// Every provenance rule `text`, the content of the corpus `entry` lists,
/// breaks against `accepted` commits.
#[must_use]
pub fn content_problems(entry: &ManifestEntry, text: &str, accepted: &[&str]) -> Vec<String> {
    let corpus = &entry.corpus;
    let capture = format!("`{REPIN} capture {corpus}`");
    let mut problems = Vec::new();
    let Ok(document) = serde_json::from_str::<Value>(text) else {
        return vec![format!("{corpus} does not parse as JSON")];
    };
    let recorded = recorded_commit(&document);
    if entry.kind == Kind::Fixture {
        if let Some(recorded) = recorded.filter(|commit| !accepted.contains(commit)) {
            problems.push(off_commit(corpus, recorded, accepted));
        }
        return problems;
    }
    match recorded {
        None => problems.push(format!("{corpus} records no reference commit")),
        Some(recorded) if !accepted.contains(&recorded) => {
            problems.push(off_commit(corpus, recorded, accepted));
        }
        Some(recorded) if entry.commit.as_deref() != Some(recorded) => problems.push(format!(
            "{corpus} records commit {recorded} and the manifest records {}; recapture it with \
             {capture}",
            entry.commit.as_deref().unwrap_or("none")
        )),
        Some(_) => {}
    }
    let digest = crate::text::sha256_hex(text.as_bytes());
    if entry.sha256.as_deref() != Some(digest.as_str()) {
        problems.push(format!(
            "{corpus} changed outside the capture tool: its SHA-256 is {digest} and the manifest \
             records {}; {capture} is the only sanctioned way to change it",
            entry.sha256.as_deref().unwrap_or("none")
        ));
    }
    let counts = families(&document);
    if counts.values().sum::<usize>() == 0 {
        problems.push(format!(
            "{corpus} holds no observations; its case lists are empty"
        ));
    }
    for (family, floor) in &entry.floors {
        let count = counts.get(family).copied().unwrap_or(0);
        if count < *floor {
            problems.push(format!(
                "{corpus} holds {count} {family}, below the floor of {floor}; recapture what was \
                 lost, or lower the floor on purpose with `{REPIN} capture {corpus} \
                 --accept-shrink`"
            ));
        }
    }
    for (family, count) in &counts {
        if entry.floors.get(family).copied().unwrap_or(0) < minimum_floor(*count) {
            problems.push(format!(
                "{corpus}: the floor of {family} is below {FLOOR_PERCENT}% of its {count} \
                 observations; {capture} raises it"
            ));
        }
    }
    if entry.count != Some(counts.values().sum()) {
        problems.push(format!(
            "{corpus}: the manifest count disagrees with the corpus; {capture}"
        ));
    }
    problems
}

fn off_commit(corpus: &str, recorded: &str, accepted: &[&str]) -> String {
    format!(
        "{corpus} was captured from {recorded}; accepted: {}",
        accepted.join(", ")
    )
}

fn report(problems: &[String]) -> String {
    let mut message = String::from("the corpus breaks its provenance:");
    for problem in problems {
        let _ = write!(message, "\n  {problem}");
    }
    message
}

/// Asserts the corpus at `corpus`, a workspace-relative path, is listed, at
/// an accepted commit, unchanged since its capture, and above its floors.
pub fn assert_corpus(corpus: &str) {
    let manifest = load();
    let entry = manifest.entry(corpus).unwrap_or_else(|| {
        panic!(
            "{corpus} is not in {MANIFEST}; add it with `{REPIN} add {corpus} --driver \"<script> \
             <args>\" --test <file>::<function> --row <n>`"
        )
    });
    let text = std::fs::read_to_string(repository().join(corpus))
        .unwrap_or_else(|error| panic!("{corpus} is unreadable: {error}"));
    let problems = content_problems(entry, &text, &manifest.accepted_for(corpus));
    assert!(problems.is_empty(), "{}", report(&problems));
    record_replay(corpus);
}

/// Records that the corpus at `corpus` replayed, for the CI summary that
/// counts replays beside skipped probes (`target/parity/replays.jsonl`).
fn record_replay(corpus: &str) {
    super::live::record(
        "replays.jsonl",
        &serde_json::json!({"corpus": corpus, "test": super::live::test_name()}),
    );
}

/// Asserts `text`, a corpus a replay embedded, is a listed corpus at an
/// accepted commit, unchanged since its capture, and above its floors, and
/// returns the path the manifest lists it under.
///
/// The embedded text carries no path, so the entry is the one whose SHA-256
/// matches; when none does, the corpus was edited outside the capture tool,
/// and the entry whose file on disk holds the same text is the one reported.
pub fn assert_replayed(text: &str) -> String {
    let manifest = load();
    let digest = crate::text::sha256_hex(text.as_bytes());
    let entry = manifest
        .entries
        .iter()
        .find(|entry| entry.sha256.as_deref() == Some(digest.as_str()))
        .or_else(|| {
            manifest.entries.iter().find(|entry| {
                std::fs::read(repository().join(&entry.corpus))
                    .is_ok_and(|bytes| bytes == text.as_bytes())
            })
        });
    let Some(entry) = entry else {
        let commit = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|corpus| recorded_commit(&corpus).map(str::to_owned))
            .unwrap_or_else(|| "no commit".to_owned());
        panic!(
            "the replayed corpus (captured from {commit}; accepted: {}) matches no entry of \
             {MANIFEST}; list it with `{REPIN} add`",
            super::pin_commits().join(", ")
        );
    };
    let problems = content_problems(entry, text, &manifest.accepted_for(&entry.corpus));
    assert!(problems.is_empty(), "{}", report(&problems));
    record_replay(&entry.corpus);
    entry.corpus.clone()
}

/// Where the clean-room guard keeps its digest sets and third-party layers,
/// which record the commit they were measured at without being corpora.
pub const LICENSING_DATA: &str = "crates/vibe-core/tests/licensing/";

/// Whether `corpus`, a workspace-relative path, is tracked JSON under a
/// crate's `tests/` directory, outside the guard's [`LICENSING_DATA`].
#[must_use]
pub fn is_corpus_location(corpus: &str) -> bool {
    let parts: Vec<&str> = corpus.split('/').collect();
    parts.len() >= 4
        && parts[0] == "crates"
        && parts[2] == "tests"
        && !corpus.starts_with(LICENSING_DATA)
        && Path::new(corpus)
            .extension()
            .is_some_and(|extension| extension == "json")
}
