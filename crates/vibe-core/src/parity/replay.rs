//! The corpus replay every parity integration test drives.
//!
//! An oracle under `scripts/parity/` records a corpus from the pinned
//! reference and, pointed at one of this port's binaries, the same
//! observations from the port. The tests that replay those corpora used to
//! carry their own copy of the comparison, the ledger accounting and the live
//! probe, nineteen of them nearly identical. This module is the one copy. The
//! replays are integration tests of four crates, and a `#[cfg(test)]` item is
//! not visible across a crate boundary, so the other crates reach it through
//! the `parity-replay` feature their dev-dependency on this crate enables: no
//! binary compiles it.
//!
//! A replay compares the two observations of every scenario as JSON pointers
//! ([`differences`]). Every difference has to fall under an [`Entry`] of the
//! test's ledger or under the corpus's measured debt ([`super::debt`]), and
//! every entry and every debt group has to still reproduce, so the summary
//! line a [`Tally`] prints is what a row of `docs/parity.md` reads.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the replay is test machinery: an expectation that fails is the calling test failing"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

#[cfg(test)]
mod replay_tests;

/// The workspace root, which the capture scripts run from.
#[must_use]
pub fn repository() -> PathBuf {
    crate::platform::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .expect("the workspace root resolves")
}

/// Every scenario `corpus` records, by name.
#[must_use]
pub fn scenarios(corpus: &Value) -> BTreeMap<String, &Value> {
    corpus["scenarios"]
        .as_array()
        .expect("the corpus holds a scenario list")
        .iter()
        .map(|entry| {
            (
                entry["name"]
                    .as_str()
                    .expect("every scenario is named")
                    .to_owned(),
                entry,
            )
        })
        .collect()
}

/// Every JSON pointer at which `port` departs from `reference`. A key or an
/// element only one side holds is a difference at its own pointer.
///
/// A prose digest (`{"prose": <length>, "sha256": <hex>}`, what a capture
/// records for authored text) is one value: two digests that differ are one
/// difference at the digest's pointer, never one per field. No digest is
/// exempt here: a replay accepts a sentence this port words its own way only
/// through an [`OwnProse`] entry naming that pointer.
#[must_use]
pub fn differences(reference: &Value, port: &Value) -> Vec<String> {
    let mut found = Vec::new();
    collect_differences(reference, port, "", &mut found);
    found
}

fn collect_differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    if is_digest(reference) || is_digest(port) {
        if reference != port {
            found.push(pointer.to_owned());
        }
        return;
    }
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => {
                        collect_differences(left, right, &child, found);
                    }
                    _ => found.push(child),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let child = format!("{pointer}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => {
                        collect_differences(left, right, &child, found);
                    }
                    _ => found.push(child),
                }
            }
        }
        _ if reference != port => found.push(pointer.to_owned()),
        _ => {}
    }
}

/// One place two documents disagree, with what each side holds there.
#[derive(Debug, Clone, PartialEq)]
pub struct ValuedDifference {
    pub pointer: String,
    pub expected: Value,
    pub actual: Value,
}

/// Every place `actual` departs from `expected`, all of them rather than the
/// first, because a ledger is matched per pointer: reporting only the earliest
/// would leave an entry for a later field unexercised, and the staleness check
/// would then delete a gap that is still open.
///
/// A key only one side holds is a difference at its own pointer, the other
/// side reading `"absent"`. Two arrays of different lengths are one difference
/// at the array, since every element past the shorter one would otherwise be
/// reported as shifted.
#[must_use]
pub fn valued_differences(expected: &Value, actual: &Value) -> Vec<ValuedDifference> {
    let mut found = Vec::new();
    collect_valued(expected, actual, "", &mut found);
    found
}

fn collect_valued(
    expected: &Value,
    actual: &Value,
    pointer: &str,
    found: &mut Vec<ValuedDifference>,
) {
    let absent = || Value::String("absent".to_owned());
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => collect_valued(left, right, &child, found),
                    (left, right) => found.push(ValuedDifference {
                        pointer: child,
                        expected: left.cloned().unwrap_or_else(absent),
                        actual: right.cloned().unwrap_or_else(absent),
                    }),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                collect_valued(left, right, &format!("{pointer}/{index}"), found);
            }
        }
        _ if expected == actual => {}
        _ => found.push(ValuedDifference {
            pointer: pointer.to_owned(),
            expected: expected.clone(),
            actual: actual.clone(),
        }),
    }
}

/// Whether `value` is a prose digest: exactly a `prose` length and a
/// `sha256`.
#[must_use]
pub fn is_digest(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("prose") && object.contains_key("sha256")
    })
}

/// Whether `pointer` matches `pattern` segment by segment, where a `*`
/// segment matches any one segment and a `**` segment one or more.
#[must_use]
pub fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let pointer: Vec<&str> = pointer.split('/').collect();
    segments_match(&pattern, &pointer)
}

fn segments_match(pattern: &[&str], pointer: &[&str]) -> bool {
    match pattern.split_first() {
        None => pointer.is_empty(),
        Some((&"**", rest)) => {
            (1..=pointer.len()).any(|taken| segments_match(rest, &pointer[taken..]))
        }
        Some((expected, rest)) => pointer.split_first().is_some_and(|(segment, remaining)| {
            (*expected == "*" || expected == segment) && segments_match(rest, remaining)
        }),
    }
}

/// Whether `pointer` is `prefix` or lies under it, on a segment boundary:
/// `/display/summary` holds `/display/summary/0` and not
/// `/display/summaryDetail`. A trailing `/` on `prefix` is ignored.
#[must_use]
pub fn pointer_within(prefix: &str, pointer: &str) -> bool {
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    pointer
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `suffix` ends `pointer` on a segment boundary.
#[must_use]
pub fn pointer_ends_with(suffix: &str, pointer: &str) -> bool {
    let suffix = suffix.strip_suffix('/').unwrap_or(suffix);
    pointer
        .strip_suffix(suffix)
        .is_some_and(|rest| suffix.starts_with('/') || rest.is_empty() || rest.ends_with('/'))
}

/// Whether `pointer` names a top-level observation key and nothing below it,
/// such as `/requests/`: an entry that broad would absorb whatever changes
/// anywhere in that observation.
#[must_use]
pub fn is_top_level(pointer: &str) -> bool {
    let pointer = pointer.strip_suffix('/').unwrap_or(pointer);
    pointer
        .strip_prefix('/')
        .is_some_and(|key| !key.is_empty() && !key.contains('/'))
}

/// Whether the pattern `pattern` (as [`pointer_matches`] reads one) matches
/// a whole top-level observation: a key and `**`, such as `/requests/**`,
/// or `/**` alone. A pattern without `**` matches only pointers of its own
/// length, so `/stdout` names one value and absorbs nothing below it.
#[must_use]
pub fn pattern_is_top_level(pattern: &str) -> bool {
    pattern
        .strip_suffix("/**")
        .is_some_and(|key| key.is_empty() || is_top_level(key))
}

/// Whether the scenario `name` matches `pattern`: a name, or a prefix ending
/// in `*`, so `*` alone matches every scenario.
#[must_use]
pub fn scenario_matches(pattern: &str, name: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == name, |prefix| name.starts_with(prefix))
}

/// The row numbers `scorecard`, the text of `docs/parity.md`, carries.
#[must_use]
pub fn scorecard_rows(scorecard: &str) -> BTreeSet<&str> {
    scorecard
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|line| line.split_once(" |"))
        .map(|(number, _)| number.trim())
        .filter(|number| number.parse::<u32>().is_ok())
        .collect()
}

/// A text as a capture records the prose a program authored: its length in
/// code points and the hexadecimal SHA-256 of its UTF-8 bytes.
#[must_use]
pub fn digest(text: &str) -> Value {
    json!({
        "length": text.chars().count(),
        "sha256": crate::text::sha256_hex(text.as_bytes()),
    })
}

/// The attested reference environment the live probe named `probe`
/// recaptures, or [`None`] after recording why this machine cannot act as the
/// oracle ([`super::live::gate`]). The committed corpus replays regardless,
/// which is what keeps a missing environment from failing `cargo test` unless
/// `VIBE_PARITY_REQUIRE_LIVE=1` asks for live measurement.
#[must_use]
#[track_caller]
pub fn pinned_reference(probe: &str) -> Option<PathBuf> {
    super::live::gate(probe)
}

/// [`pinned_reference`] for a probe whose oracle is the host's own platform
/// ([`super::live::gate_native`]).
#[must_use]
#[track_caller]
pub fn pinned_reference_native(probe: &str) -> Option<PathBuf> {
    super::live::gate_native(probe)
}

/// A capture script under `scripts/parity/` and how a replay runs it.
#[derive(Clone, Copy, Debug)]
pub struct Capture {
    /// The script, relative to the workspace root.
    pub script: &'static str,
    /// What selects the corpus the script records, such as
    /// `--harness unified`, which the command that regenerates it repeats.
    pub family: &'static [&'static str],
    /// What every run passes besides: how many scenarios run side by side, or
    /// a fixture binary the scenarios use.
    pub arguments: &'static [&'static str],
}

impl Capture {
    /// The command that regenerates the corpus: `repin.py capture` on the
    /// manifest entry this script and family write, which stages the output
    /// and records its digest, or the bare driver when no entry names it.
    fn command(&self) -> String {
        let driver: Vec<&str> = std::iter::once(self.script)
            .chain(self.family.iter().copied())
            .collect();
        super::manifest::load()
            .entries
            .iter()
            .find(|entry| {
                entry.driver.as_ref().is_some_and(|listed| {
                    listed.iter().map(String::as_str).eq(driver.iter().copied())
                })
            })
            .map_or_else(
                || driver.join(" "),
                |entry| format!("{} capture {}", super::manifest::REPIN, entry.corpus),
            )
    }

    /// Runs the script under `python3` from the workspace root, `arguments`
    /// after its own. `FORCE_COLOR` is removed: the reference would answer
    /// with escapes no corpus holds.
    #[must_use]
    pub fn run(&self, arguments: &[&OsStr]) -> Output {
        let root = repository();
        Command::new("python3")
            .arg(root.join(self.script))
            .args(self.family)
            .args(self.arguments)
            .args(arguments)
            .current_dir(&root)
            .env_remove("FORCE_COLOR")
            .output()
            .expect("python3 runs the capture script")
    }

    /// Captures this port, `arguments` naming the binary the script drives,
    /// and returns what the script wrote to `output`.
    #[must_use]
    pub fn port(&self, arguments: &[&str], output: &Path) -> Value {
        let mut arguments: Vec<&OsStr> = arguments.iter().map(OsStr::new).collect();
        arguments.extend([OsStr::new("--output"), output.as_os_str()]);
        let capture = self.run(&arguments);
        assert!(
            capture.status.success(),
            "the port capture failed: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
        serde_json::from_str(
            &std::fs::read_to_string(output).expect("the port capture is readable"),
        )
        .expect("the port capture parses")
    }

    /// Recaptures the pinned reference at `root`, `arguments` after the
    /// script's own, and asserts the committed corpus is still what it
    /// answers.
    pub fn check(&self, root: &Path, arguments: &[&str]) {
        let mut arguments: Vec<&OsStr> = arguments.iter().map(OsStr::new).collect();
        arguments.extend([
            OsStr::new("--check"),
            OsStr::new("--reference"),
            root.as_os_str(),
        ]);
        let output = self.run(&arguments);
        assert!(
            output.status.success(),
            "the pinned reference no longer answers what the corpus records; regenerate it with \
             `{}`: {}",
            self.command(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Recaptures the pinned reference at `root` with a script that has no
    /// `--check`: `arguments` name the files it writes, `recaptured` among
    /// them, and the corpus there has to be the one committed at `committed`.
    pub fn recapture(
        &self,
        root: &Path,
        arguments: &[&OsStr],
        recaptured: &Path,
        committed: &Path,
    ) {
        let mut arguments = arguments.to_vec();
        arguments.extend([OsStr::new("--reference"), root.as_os_str()]);
        let output = self.run(&arguments);
        assert!(
            output.status.success(),
            "the reference capture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let read = |path: &Path| -> Value {
            serde_json::from_str(&std::fs::read_to_string(path).expect("the corpus is readable"))
                .expect("the corpus parses")
        };
        assert_eq!(
            read(recaptured),
            read(committed),
            "the pinned reference no longer answers what the committed corpus records; \
             regenerate it with `{}`",
            self.command()
        );
    }

    /// The live probe named `probe`: recaptures the pinned reference when
    /// this machine has it, as [`Capture::check`] does.
    #[track_caller]
    pub fn probe(&self, probe: &str) {
        if let Some(root) = pinned_reference(probe) {
            self.check(&root, &[]);
        }
    }
}

/// What a ledgered difference stands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A decided divergence: the name of the row of `docs/parity.md`
    /// "Accepted divergences" that decides it.
    Accepted(&'static str),
    /// An open gap of the row the entry names, which that row still owes.
    Gap,
}

/// A difference a replay's ledger accepts.
pub trait Entry {
    /// Whether the entry accepts the difference at `pointer` in `scenario`.
    fn covers(&self, scenario: &str, pointer: &str) -> bool;

    /// How a report names the entry.
    fn label(&self) -> String;

    /// The scenarios that each have to reproduce the entry, so one that stops
    /// reproducing in one of them is reported for that one; empty when the
    /// entry applies wherever it matches.
    fn scenarios(&self) -> &[&str] {
        &[]
    }

    /// How many differences the entry covers: in each of
    /// [`Entry::scenarios`] when it names some, in each scenario it covers
    /// when [`Entry::counts_per_scenario`] says so, across the whole replay
    /// otherwise. A replay fails when the count grows or shrinks, so a
    /// regression inside a ledgered area is never absorbed.
    fn count(&self) -> usize;

    /// Whether [`Entry::count`] holds in every scenario the entry covers
    /// rather than across the replay, for an entry that names no scenario
    /// but differs by the same amount wherever it applies.
    fn counts_per_scenario(&self) -> bool {
        false
    }

    /// The row of `docs/parity.md` that answers for the difference.
    fn row(&self) -> &str;

    /// What the difference stands on.
    fn class(&self) -> Class;

    /// Whether the entry is an own-words prose entry, counted apart in the
    /// figures.
    fn is_prose(&self) -> bool {
        false
    }

    /// The commit the entry's text was audited against, for an own-words
    /// entry whose body covers reference directives.
    fn audited(&self) -> Option<&str> {
        None
    }

    /// Whether a difference the entry covers is one it admits, given what
    /// each side holds at its pointer; the error names why not.
    ///
    /// # Errors
    ///
    /// When the values at the pointer fall outside what the entry declares.
    fn admit(&self, _reference: Option<&Value>, _port: Option<&Value>) -> Result<(), String> {
        Ok(())
    }

    /// What is wrong with the entry's own declaration.
    fn problems(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The entries of `ledger`, as a [`Tally`] reads them.
pub fn entries<E: Entry>(ledger: &[E]) -> impl Iterator<Item = &dyn Entry> {
    ledger.iter().map(|entry| entry as &dyn Entry)
}

/// A difference this port keeps in the scenarios named, and the row of
/// `docs/parity.md` that answers for it.
#[derive(Debug)]
pub struct Divergence {
    pub scenarios: &'static [&'static str],
    /// Every difference at this JSON pointer or below it, on a segment
    /// boundary, is covered.
    pub pointer: &'static str,
    /// Patterns relative to `pointer` (as [`pointer_matches`] reads them,
    /// each starting with `/`) that narrow what it covers. Required when
    /// `pointer` is a top-level observation key alone, such as `/requests`.
    pub within: &'static [&'static str],
    /// How many differences it covers in each of `scenarios`.
    pub count: usize,
    pub row: &'static str,
    pub class: Class,
    pub reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenarios.contains(&scenario)
            && pointer_within(self.pointer, pointer)
            && (self.within.is_empty()
                || self.within.iter().any(|pattern| {
                    let base = self.pointer.strip_suffix('/').unwrap_or(self.pointer);
                    pointer_matches(&format!("{base}{pattern}"), pointer)
                }))
    }

    fn label(&self) -> String {
        self.pointer.to_owned()
    }

    fn scenarios(&self) -> &[&str] {
        self.scenarios
    }

    fn count(&self) -> usize {
        self.count
    }

    fn row(&self) -> &str {
        self.row
    }

    fn class(&self) -> Class {
        self.class
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.scenarios.is_empty() || !self.pointer.starts_with('/') || self.reason.is_empty() {
            problems.push(format!(
                "the ledger entry for {} needs scenarios, a pointer and a reason",
                self.pointer
            ));
        }
        if is_top_level(self.pointer) && self.within.is_empty() {
            problems.push(format!(
                "the ledger entry for {} covers a whole top-level observation; list the \
                 sub-pointers it may cover in `within`",
                self.pointer
            ));
        }
        if self.within.iter().any(|pattern| !pattern.starts_with('/')) {
            problems.push(format!(
                "the ledger entry for {} has a `within` pattern that does not start with `/`",
                self.pointer
            ));
        }
        problems
    }
}

/// A field this port answers differently wherever it appears, in any
/// scenario, and the row of `docs/parity.md` that answers for it.
#[derive(Debug)]
pub struct FieldDivergence {
    /// Every difference whose JSON pointer ends with this suffix, on a
    /// segment boundary, is covered.
    pub suffix: &'static str,
    /// How many differences it covers in each scenario it covers.
    pub count: usize,
    pub row: &'static str,
    pub class: Class,
    pub reason: &'static str,
}

impl Entry for FieldDivergence {
    fn covers(&self, _scenario: &str, pointer: &str) -> bool {
        pointer_ends_with(self.suffix, pointer)
    }

    fn label(&self) -> String {
        self.suffix.to_owned()
    }

    fn count(&self) -> usize {
        self.count
    }

    fn counts_per_scenario(&self) -> bool {
        true
    }

    fn row(&self) -> &str {
        self.row
    }

    fn class(&self) -> Class {
        self.class
    }

    fn problems(&self) -> Vec<String> {
        if self.suffix.starts_with('/') && !self.reason.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "the ledger entry for {} needs a pointer suffix and a reason",
                self.suffix
            )]
        }
    }
}

/// A difference a replay accepts wherever its scenario and pointer patterns
/// match, the row of `docs/parity.md` that answers for it, and why.
#[derive(Debug)]
pub struct PatternDivergence {
    /// The scenarios it applies to, as [`scenario_matches`] reads a pattern.
    pub scenario: &'static str,
    /// A JSON pointer inside the scenario's observation, as
    /// [`pointer_matches`] reads a pattern.
    pub pointer: &'static str,
    /// How many differences it covers across the replay.
    pub count: usize,
    pub row: &'static str,
    pub class: Class,
    pub reason: &'static str,
}

impl Entry for PatternDivergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        scenario_matches(self.scenario, scenario) && pointer_matches(self.pointer, pointer)
    }

    fn label(&self) -> String {
        format!("{} {}", self.scenario, self.pointer)
    }

    fn count(&self) -> usize {
        self.count
    }

    fn row(&self) -> &str {
        self.row
    }

    fn class(&self) -> Class {
        self.class
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.pointer.starts_with('/') || self.reason.is_empty() || self.scenario.is_empty() {
            problems.push(format!(
                "the ledger entry for {} needs a scenario, a pointer and a reason",
                self.pointer
            ));
        }
        if pattern_is_top_level(self.pointer) {
            problems.push(format!(
                "the ledger entry for {} covers a whole top-level observation; name the \
                 sub-pointers it may cover",
                self.pointer
            ));
        }
        problems
    }
}

/// How far this port's wording of a text may stray in length from the
/// reference's, as multiples of the reference's length in code points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub low: f64,
    pub high: f64,
}

impl Band {
    /// Half to twice the reference's length: a rewording, not a different
    /// text and not a missing one.
    pub const DEFAULT: Self = Self {
        low: 0.5,
        high: 2.0,
    };

    /// Whether `port` code points are within the band around `reference`.
    #[must_use]
    pub fn holds(self, reference: u64, port: u64) -> bool {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a prose length is far below 2^52 code points"
        )]
        let (reference, port) = (reference as f64, port as f64);
        port >= self.low * reference && port <= self.high * reference
    }
}

/// A text this port writes in its own words on purpose (`NOTICE`): the two
/// digests at the pointers it names may differ, provided both sides hold
/// prose there and the port's length stays within `band` of the reference's.
#[derive(Debug)]
pub struct OwnProse {
    /// The scenarios it applies to, each as [`scenario_matches`] reads a
    /// pattern.
    pub scenarios: &'static [&'static str],
    /// The digest's pointer, as [`pointer_matches`] reads a pattern.
    pub pointer: &'static str,
    /// How many digests it covers across the replay.
    pub count: usize,
    pub band: Band,
    /// For a body that restates reference directives in this port's words,
    /// the reference commit its directives were last audited against, in the
    /// 12-character abbreviation `.parity/reference-<sha12>/` names (the full
    /// hash is written only where `parity_tests` allows it); the replay fails
    /// once the corpus is captured from another, so a re-pin re-audits the
    /// text.
    pub directives: Option<&'static str>,
    pub row: &'static str,
    /// The accepted divergence of `docs/parity.md` that keeps the text out.
    pub accepted: &'static str,
    pub reason: &'static str,
}

impl Entry for OwnProse {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenarios
            .iter()
            .any(|pattern| scenario_matches(pattern, scenario))
            && pointer_matches(self.pointer, pointer)
    }

    fn label(&self) -> String {
        format!("own words {} {}", self.scenarios.join(","), self.pointer)
    }

    fn count(&self) -> usize {
        self.count
    }

    fn row(&self) -> &str {
        self.row
    }

    fn class(&self) -> Class {
        Class::Accepted(self.accepted)
    }

    fn is_prose(&self) -> bool {
        true
    }

    fn audited(&self) -> Option<&str> {
        self.directives
    }

    fn admit(&self, reference: Option<&Value>, port: Option<&Value>) -> Result<(), String> {
        let length = |value: Option<&Value>| {
            value
                .filter(|value| is_digest(value))
                .and_then(|value| value["prose"].as_u64())
        };
        let (Some(reference), Some(port)) = (length(reference), length(port)) else {
            return Err(
                "an own-words entry covers it, but both sides do not hold prose there; \
                        the difference is structural"
                    .to_owned(),
            );
        };
        if self.band.holds(reference, port) {
            Ok(())
        } else {
            Err(format!(
                "the port's prose is {port} code points, outside the band of {} to {} times the \
                 reference's {reference}",
                self.band.low, self.band.high
            ))
        }
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.pointer.starts_with('/')
            || self.scenarios.is_empty()
            || self.scenarios.iter().any(|pattern| pattern.is_empty())
            || self.reason.is_empty()
        {
            problems.push(format!(
                "the own-words entry for {} needs scenarios, a pointer and a reason",
                self.pointer
            ));
        }
        if pattern_is_top_level(self.pointer) {
            problems.push(format!(
                "the own-words entry for {} covers a whole top-level observation; name the \
                 sub-pointers its digests sit at",
                self.pointer
            ));
        }
        if !(self.band.low > 0.0 && self.band.low <= 1.0 && self.band.high >= 1.0) {
            problems.push(format!(
                "the own-words entry for {} declares a band that excludes the reference's length",
                self.pointer
            ));
        }
        problems
    }
}

/// How many hex digits an audited commit is written with: the abbreviation
/// `.parity/reference-<sha12>/` names, short of the full hash that
/// `parity_tests` allows in two places only.
pub const AUDIT_ABBREVIATION: usize = 12;

/// Asserts every entry of a ledger over the corpus `listed` (a
/// workspace-relative path the manifest lists, as [`manifest::assert_replayed`]
/// returns it), whose scenarios are `recorded`, is declared soundly: its own
/// shape, a row `docs/parity.md` carries, a decided divergence the scorecard
/// accepts when its class names one, recorded scenarios, an audit commit that
/// is the corpus's, and no gap of the row the corpus measures: that gap is a
/// measurement, kept in the corpus's measured debt ([`super::debt`]), never a
/// decision a ledger writes.
///
/// [`Tally::conclude`] runs this; a test may also run it on its own, which
/// checks a ledger on a host where its replay is ignored.
///
/// [`manifest::assert_replayed`]: super::manifest::assert_replayed
pub fn assert_ledger<'l>(
    listed: &str,
    recorded: &BTreeMap<String, &Value>,
    ledger: impl IntoIterator<Item = &'l dyn Entry>,
) {
    let manifest = super::manifest::load();
    let listing = manifest
        .entry(listed)
        .unwrap_or_else(|| panic!("{listed} is not listed in {}", super::manifest::MANIFEST));
    let commit = listing.commit.as_deref().unwrap_or_default();
    let scorecard = super::figures::scorecard();
    let rows = scorecard_rows(&scorecard);
    let accepted = super::figures::accepted_divergences(&scorecard);
    let mut problems = Vec::new();
    for entry in ledger {
        let label = entry.label();
        problems.extend(entry.problems());
        if !rows.contains(entry.row()) {
            problems.push(format!(
                "the ledger entry for {label} names row {}, which docs/parity.md does not carry",
                entry.row()
            ));
        }
        match entry.class() {
            Class::Accepted(name) if !accepted.contains(name) => problems.push(format!(
                "the ledger entry for {label} stands on `{name}`, which docs/parity.md \
                 \"Accepted divergences\" does not carry"
            )),
            Class::Gap if entry.row() == listing.row => {
                problems.push(format!(
                    "row {} is what this corpus measures, so a difference it keeps is a gap to \
                     close or measured debt (`{} debt {listed}`), not a ledger entry: {label}",
                    listing.row,
                    super::manifest::REPIN
                ));
            }
            _ => {}
        }
        if let Some(missing) = entry
            .scenarios()
            .iter()
            .find(|scenario| !recorded.contains_key(**scenario))
        {
            problems.push(format!(
                "the ledger entry for {label} names scenario {missing}, which the corpus does not \
                 record"
            ));
        }
        if let Some(audited) = entry.audited()
            && (audited.len() != AUDIT_ABBREVIATION || !commit.starts_with(audited))
        {
            problems.push(format!(
                "the own-words entry for {label} was audited against {audited}, and the corpus is \
                 captured from {commit}: re-read the reference's directives at {commit} and \
                 restate the entry"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "the ledger of {listed} is unsound:\n  {}",
        problems.join("\n  ")
    );
}

/// What one replay found, scenario by scenario, against its ledger.
pub struct Tally<'l> {
    /// The corpus as the manifest lists it.
    listed: String,
    recorded: BTreeMap<String, Value>,
    entries: Vec<&'l dyn Entry>,
    /// For every entry, how many differences it covered in each scenario.
    reproduced: Vec<BTreeMap<String, usize>>,
    /// For every entry, the pointers it covered, numbers generalized to `*`,
    /// which is what narrowing a broad entry starts from.
    covered: Vec<BTreeSet<String>>,
    /// The differences no entry admitted, which the corpus's measured debt
    /// covers or the replay reports.
    uncovered: Vec<Uncovered>,
    /// The scenarios this run compared, whose debt it owes.
    compared: BTreeSet<String>,
    figures: super::figures::Figures,
}

/// A difference no ledger entry admitted.
struct Uncovered {
    scenario: String,
    pointer: String,
    /// How a report quotes it, with what each side holds.
    report: String,
}

impl<'l> Tally<'l> {
    /// A tally over `entries` for `corpus`, the committed corpus text, the
    /// first entry that covers a difference accounting for it. The corpus is
    /// asserted against its manifest entry first, so nothing is compared
    /// against a corpus whose provenance fails.
    ///
    /// # Panics
    ///
    /// When the corpus breaks its provenance or holds no scenario.
    pub fn new(corpus: &str, entries: impl IntoIterator<Item = &'l dyn Entry>) -> Self {
        let listed = super::manifest::assert_replayed(corpus);
        let document: Value = serde_json::from_str(corpus).expect("the corpus parses");
        let recorded: BTreeMap<String, Value> = document
            .get("scenarios")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| Some((entry["name"].as_str()?.to_owned(), entry.clone())))
            .collect();
        let entries: Vec<&dyn Entry> = entries.into_iter().collect();
        Self {
            listed,
            recorded,
            reproduced: vec![BTreeMap::new(); entries.len()],
            covered: vec![BTreeSet::new(); entries.len()],
            entries,
            uncovered: Vec::new(),
            compared: BTreeSet::new(),
            figures: super::figures::Figures::default(),
        }
    }

    /// The corpus as the manifest lists it.
    #[must_use]
    pub fn listed(&self) -> &str {
        &self.listed
    }

    /// Accounts for `found`, the differences between the two observations of
    /// `scenario`, which an unexplained difference quotes.
    pub fn record(&mut self, scenario: &str, reference: &Value, port: &Value, found: Vec<String>) {
        self.figures.total += 1;
        self.compared.insert(scenario.to_owned());
        if found.is_empty() {
            self.figures.conformant += 1;
            return;
        }
        let mut decided = true;
        for pointer in found {
            let left = reference.pointer(&pointer);
            let right = port.pointer(&pointer);
            let covering = self
                .entries
                .iter()
                .position(|entry| entry.covers(scenario, &pointer));
            let admitted = covering.map(|index| (index, self.entries[index].admit(left, right)));
            match admitted {
                Some((index, Ok(()))) => {
                    let entry = self.entries[index];
                    *self.reproduced[index]
                        .entry(scenario.to_owned())
                        .or_default() += 1;
                    self.covered[index].insert(super::debt::generalize(&pointer));
                    match (entry.is_prose(), entry.class()) {
                        (true, _) => self.figures.prose_differences += 1,
                        (false, Class::Accepted(_)) => self.figures.accepted_differences += 1,
                        (false, Class::Gap) => {
                            self.figures.gap_differences += 1;
                            decided = false;
                        }
                    }
                }
                Some((index, Err(why))) => {
                    decided = false;
                    self.uncovered.push(Uncovered {
                        report: format!(
                            "{scenario} {pointer} ({}: {why})\n  reference: {}\n  port:      {}",
                            self.entries[index].label(),
                            quote(left),
                            quote(right)
                        ),
                        scenario: scenario.to_owned(),
                        pointer,
                    });
                }
                None => {
                    decided = false;
                    self.uncovered.push(Uncovered {
                        report: format!(
                            "{scenario} {pointer}\n  reference: {}\n  port:      {}",
                            quote(left),
                            quote(right)
                        ),
                        scenario: scenario.to_owned(),
                        pointer,
                    });
                }
            }
        }
        if decided {
            self.figures.accepted += 1;
        }
    }

    /// Compares every scenario `recorded` holds with the one `replayed` holds
    /// under its name, `differ` finding the differences between their
    /// observations. The two have to hold the same scenarios, captured from
    /// the same specification, or `capture` changed since the corpus was
    /// recorded.
    pub fn compare(
        &mut self,
        capture: &Capture,
        recorded: &BTreeMap<String, &Value>,
        replayed: &BTreeMap<String, &Value>,
        differ: impl Fn(&Value, &Value) -> Vec<String>,
    ) {
        let command = capture.command();
        assert_eq!(
            recorded.keys().collect::<Vec<_>>(),
            replayed.keys().collect::<Vec<_>>(),
            "the capture script declares other scenarios than the corpus records; recapture the \
             reference with `{command}`"
        );
        for (name, reference) in recorded {
            let port = replayed[name];
            assert_eq!(
                reference["scenario"], port["scenario"],
                "the capture script changed scenario {name} since the corpus was recorded; \
                 recapture the reference with `{command}`"
            );
            let found = differ(&reference["observed"], &port["observed"]);
            self.record(name, &reference["observed"], &port["observed"], found);
        }
    }

    /// Prints the summary line a scorecard row reads, opening with `heading`,
    /// then asserts the ledger is sound, every difference fell under an
    /// entry or the corpus's measured debt, every entry and every debt group
    /// covered exactly the differences it declares, and the counts equal the
    /// line of `docs/parity.md` "Measured figures" that `heading` keys.
    pub fn conclude(mut self, heading: &str) {
        let mut debt = super::debt::Debt::load(&self.listed, heading);
        debt.retain_compared(|scenario| self.compared.contains(scenario));
        let mut unexplained = Vec::new();
        for difference in std::mem::take(&mut self.uncovered) {
            if debt.absorb(&difference.scenario, &difference.pointer) {
                self.figures.gap_differences += 1;
            } else {
                unexplained.push(difference.report);
            }
        }
        let (stale, miscounted) = count_problems(&self.entries, &self.reproduced);
        let detail: Vec<Value> = self
            .entries
            .iter()
            .zip(self.reproduced.iter().zip(&self.covered))
            .map(|(entry, (reproduced, pointers))| {
                json!({
                    "label": entry.label(),
                    "row": entry.row(),
                    "declared": entry.count(),
                    "covered": reproduced,
                    "pointers": pointers,
                })
            })
            .collect();
        let figures = self.figures;
        println!(
            "{heading} {}/{} scenarios conformant, {} more at parity, {} ledgered differences \
             across {} entries and {} debt groups ({} accepted, {} gaps, {} own words; {} of \
             the gaps measured debt)",
            figures.conformant,
            figures.total,
            figures.accepted,
            figures.accepted_differences + figures.gap_differences + figures.prose_differences,
            self.entries.len(),
            debt.groups(),
            figures.accepted_differences,
            figures.gap_differences,
            figures.prose_differences,
            debt.covered(),
        );
        let recorded: BTreeMap<String, &Value> = self
            .recorded
            .iter()
            .map(|(name, value)| (name.clone(), value))
            .collect();
        let figures_detail = json!({
            "entries": detail,
            "debt": {"groups": debt.groups(), "covered": debt.covered()},
            "unexplained": unexplained,
            "stale": stale,
            "miscounted": miscounted,
        });
        // Recorded before any assertion, so a failing run still leaves every
        // count to restate.
        let measured = super::figures::record(heading, &self.listed, figures, figures_detail);
        assert_ledger(&self.listed, &recorded, self.entries.iter().copied());
        assert!(
            unexplained.is_empty(),
            "the port departs from the corpus where no ledger entry or measured debt says it \
             may:\n{}",
            unexplained.join("\n")
        );
        assert!(
            stale.is_empty(),
            "these ledger entries no longer reproduce and should be removed: {stale:?}"
        );
        assert!(
            miscounted.is_empty(),
            "these ledger entries cover another number of differences than they declare; a \
             count that grew is a regression inside a ledgered area:\n  {}",
            miscounted.join("\n  ")
        );
        debt.assert_settled();
        super::figures::check(&measured);
    }
}

/// The entries that covered nothing (stale) and those that covered another
/// number of differences than they declare (miscounted), given what each
/// covered scenario by scenario.
fn count_problems(
    entries: &[&dyn Entry],
    reproduced: &[BTreeMap<String, usize>],
) -> (Vec<String>, Vec<String>) {
    let mut stale = Vec::new();
    let mut miscounted = Vec::new();
    for (entry, reproduced) in entries.iter().zip(reproduced) {
        let declared = entry.count();
        let scenarios = entry.scenarios();
        if scenarios.is_empty() && entry.counts_per_scenario() {
            if reproduced.values().all(|covered| *covered == 0) {
                stale.push(entry.label());
            }
            for (scenario, covered) in reproduced {
                if *covered != 0 && *covered != declared {
                    miscounted.push(format!(
                        "{} declares {declared} differences in {scenario} and covered {covered}",
                        entry.label()
                    ));
                }
            }
        } else if scenarios.is_empty() {
            let covered: usize = reproduced.values().sum();
            if covered == 0 {
                stale.push(entry.label());
            } else if covered != declared {
                miscounted.push(format!(
                    "{} declares {declared} differences across the replay and covered {covered}",
                    entry.label()
                ));
            }
        } else {
            for scenario in scenarios {
                match reproduced.get(*scenario).copied().unwrap_or(0) {
                    0 => stale.push(format!("{scenario} {}", entry.label())),
                    covered if covered != declared => miscounted.push(format!(
                        "{} declares {declared} differences in {scenario} and covered {covered}",
                        entry.label()
                    )),
                    _ => {}
                }
            }
        }
    }
    (stale, miscounted)
}

fn quote(value: Option<&Value>) -> String {
    value.map_or_else(|| "<absent>".to_owned(), Value::to_string)
}

/// A committed corpus replayed against this port through its capture script.
pub struct Replay<'a> {
    /// What the summary line opens with, such as `review parity:`, and what
    /// keys the replay's line of `docs/parity.md` "Measured figures".
    pub heading: &'a str,
    /// The committed corpus, as `include_str!` embeds it. Its manifest entry
    /// supplies the commits it may record and the floor its scenarios may
    /// not fall below, so a recapture that lost coverage fails here and not
    /// only on the machine that made it.
    pub corpus: &'a str,
    pub capture: &'a Capture,
    /// What hands the script this port's binary, such as `--server` and the
    /// stdio fixture.
    pub port: &'a [&'a str],
    /// Where the port's capture is written.
    pub output: &'a Path,
}

impl Replay<'_> {
    /// Asserts the corpus is the one its manifest entry records, at an
    /// accepted commit and above its floors, then captures this port,
    /// compares every scenario with the corpus under `entries` (its own-words
    /// prose entries among them), and concludes the tally.
    pub fn run<'l>(&self, entries: impl IntoIterator<Item = &'l dyn Entry>) {
        let mut tally = Tally::new(self.corpus, entries);
        let corpus: Value = serde_json::from_str(self.corpus).expect("the corpus parses");
        let recorded = scenarios(&corpus);
        assert!(
            !recorded.is_empty(),
            "{} holds no scenarios, so the replay would compare nothing",
            tally.listed()
        );
        let replayed = self.capture.port(self.port, self.output);
        tally.compare(self.capture, &recorded, &scenarios(&replayed), differences);
        tally.conclude(self.heading);
    }
}
