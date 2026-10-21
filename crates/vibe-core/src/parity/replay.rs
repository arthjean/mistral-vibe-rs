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
//! test's ledger, and every entry has to still reproduce, so the summary line
//! a [`Tally`] prints is what a row of `docs/parity.md` reads.

#![allow(
    clippy::expect_used,
    reason = "the replay is test machinery: an expectation that fails is the calling test failing"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

use super::{RESTORE_COMMAND, off_pin_reason, reference_root};

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

/// How a replay compares the prose both sides reduce to a length and a
/// SHA-256.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prose {
    /// Two digests are compared like any other value, so a sentence this port
    /// words its own way is a difference a ledger entry has to name.
    Compared,
    /// Two `{prose, sha256}` digests count as equal wherever both sides hold
    /// one: this port writes its own prose on purpose (`NOTICE`).
    OwnWords,
}

/// Every JSON pointer at which `port` departs from `reference`. A key or an
/// element only one side holds is a difference at its own pointer.
#[must_use]
pub fn differences(reference: &Value, port: &Value, prose: Prose) -> Vec<String> {
    let mut found = Vec::new();
    collect_differences(reference, port, prose, "", &mut found);
    found
}

fn collect_differences(
    reference: &Value,
    port: &Value,
    prose: Prose,
    pointer: &str,
    found: &mut Vec<String>,
) {
    if prose == Prose::OwnWords && is_prose(reference) && is_prose(port) {
        return;
    }
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => {
                        collect_differences(left, right, prose, &child, found);
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
                        collect_differences(left, right, prose, &child, found);
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

fn is_prose(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("prose") && object.contains_key("sha256")
    })
}

/// Whether `pointer` matches `pattern` segment by segment, where a `*`
/// segment matches any one segment and a final `**` one or more.
#[must_use]
pub fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let mut pattern = pattern.split('/').peekable();
    let mut pointer = pointer.split('/');
    while let Some(expected) = pattern.next() {
        if expected == "**" && pattern.peek().is_none() {
            return pointer.next().is_some();
        }
        match pointer.next() {
            Some(segment) if expected == "*" || expected == segment => {}
            _ => return false,
        }
    }
    pointer.next().is_none()
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

/// The checkout the live probe named `probe` recaptures, or [`None`] after
/// saying why this machine cannot act as the oracle. The committed corpus
/// replays regardless, which is what keeps a missing checkout from failing
/// `cargo test`.
#[must_use]
pub fn pinned_reference(probe: &str) -> Option<PathBuf> {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, probe) {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return None;
    }
    Some(root)
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
    /// The command that regenerates the corpus.
    fn command(&self) -> String {
        std::iter::once(self.script)
            .chain(self.family.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
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
    pub fn probe(&self, probe: &str) {
        if let Some(root) = pinned_reference(probe) {
            self.check(&root, &[]);
        }
    }
}

/// A difference a replay's ledger accepts.
pub trait Entry {
    /// Whether the entry accepts the difference at `pointer` in `scenario`.
    fn covers(&self, scenario: &str, pointer: &str) -> bool;

    /// How a report names the entry.
    fn label(&self) -> String;

    /// The scenarios that each have to reproduce the entry, so one that stops
    /// reproducing in one of them is reported for that one; empty when one
    /// reproduction anywhere keeps the entry.
    fn scenarios(&self) -> &[&str] {
        &[]
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
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pub pointer: &'static str,
    pub row: &'static str,
    pub reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenarios.contains(&scenario) && pointer.starts_with(self.pointer)
    }

    fn label(&self) -> String {
        self.pointer.to_owned()
    }

    fn scenarios(&self) -> &[&str] {
        self.scenarios
    }
}

impl Divergence {
    /// Asserts every entry of `ledger` names a row `scorecard` carries,
    /// scenarios `recorded` holds, a pointer prefix and a reason. `measured`
    /// is the row the corpus measures: an entry naming it has to stand on the
    /// divergence the scorecard accepts under the name `accepted`, and with no
    /// such name it is a gap rather than an entry.
    pub fn assert_ledger(
        ledger: &[Self],
        scorecard: &str,
        recorded: &BTreeMap<String, &Value>,
        measured: &str,
        accepted: Option<&str>,
    ) {
        let rows = scorecard_rows(scorecard);
        for entry in ledger {
            assert!(
                rows.contains(entry.row),
                "the ledger entry for {} names row {}, which docs/parity.md does not carry",
                entry.pointer,
                entry.row
            );
            match accepted {
                Some(accepted) => assert!(
                    entry.row != measured || scorecard.contains(&format!("| {accepted} |")),
                    "the ledger entry for {} keeps a row {measured} difference that no accepted \
                     divergence decides",
                    entry.pointer
                ),
                None => assert_ne!(
                    entry.row, measured,
                    "row {measured} is what this corpus measures, so a difference it keeps is a \
                     gap, not a ledger entry: {}",
                    entry.pointer
                ),
            }
            assert!(
                !entry.scenarios.is_empty()
                    && entry
                        .scenarios
                        .iter()
                        .all(|scenario| recorded.contains_key(*scenario))
                    && entry.pointer.starts_with('/')
                    && !entry.reason.is_empty(),
                "the ledger entry for {} needs recorded scenarios, a pointer prefix and a reason",
                entry.pointer
            );
        }
    }
}

/// A field this port answers differently wherever it appears, in any
/// scenario, and the row of `docs/parity.md` that answers for it.
#[derive(Debug)]
pub struct FieldDivergence {
    /// Every difference whose JSON pointer ends with this suffix is covered.
    pub suffix: &'static str,
    pub row: &'static str,
    pub reason: &'static str,
}

impl Entry for FieldDivergence {
    fn covers(&self, _scenario: &str, pointer: &str) -> bool {
        pointer.ends_with(self.suffix)
    }

    fn label(&self) -> String {
        self.suffix.to_owned()
    }
}

impl FieldDivergence {
    /// Asserts every entry of `ledger` names a pointer suffix, a reason and a
    /// row `scorecard` carries other than `measured`, the row the corpus
    /// measures, where a difference it keeps is a gap rather than an entry.
    pub fn assert_ledger(ledger: &[Self], scorecard: &str, measured: &str) {
        let rows = scorecard_rows(scorecard);
        for entry in ledger {
            assert!(
                rows.contains(entry.row),
                "the ledger entry for {} names row {}, which docs/parity.md does not carry",
                entry.suffix,
                entry.row
            );
            assert_ne!(
                entry.row, measured,
                "row {measured} is what this corpus measures, so a difference it keeps is a gap, \
                 not a ledger entry: {}",
                entry.suffix
            );
            assert!(
                entry.suffix.starts_with('/') && !entry.reason.is_empty(),
                "the ledger entry for {} needs a pointer suffix and a reason",
                entry.suffix
            );
        }
    }
}

/// A difference a replay accepts wherever its scenario and pointer patterns
/// match, and why.
#[derive(Debug)]
pub struct PatternDivergence {
    /// The scenarios it applies to, as [`scenario_matches`] reads a pattern.
    pub scenario: &'static str,
    /// A JSON pointer inside the scenario's observation, as
    /// [`pointer_matches`] reads a pattern.
    pub pointer: &'static str,
    pub reason: &'static str,
}

impl Entry for PatternDivergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        scenario_matches(self.scenario, scenario) && pointer_matches(self.pointer, pointer)
    }

    fn label(&self) -> String {
        self.pointer.to_owned()
    }
}

impl PatternDivergence {
    /// Asserts every entry of `ledger` names a scenario, a pointer and a
    /// reason.
    pub fn assert_ledger(ledger: &[Self]) {
        for entry in ledger {
            assert!(
                entry.pointer.starts_with('/')
                    && !entry.reason.is_empty()
                    && !entry.scenario.is_empty(),
                "the ledger entry for {} needs a scenario, a pointer and a reason",
                entry.pointer
            );
        }
    }
}

/// What one replay found, scenario by scenario, against its ledger.
pub struct Tally<'l> {
    entries: Vec<&'l dyn Entry>,
    /// For every entry, how many differences it covered in each scenario.
    reproduced: Vec<BTreeMap<String, usize>>,
    unexplained: Vec<String>,
    scenarios: usize,
    conformant: usize,
}

impl<'l> Tally<'l> {
    /// A tally over `entries`, the first entry that covers a difference
    /// accounting for it.
    pub fn new(entries: impl IntoIterator<Item = &'l dyn Entry>) -> Self {
        let entries: Vec<&dyn Entry> = entries.into_iter().collect();
        Self {
            reproduced: vec![BTreeMap::new(); entries.len()],
            entries,
            unexplained: Vec::new(),
            scenarios: 0,
            conformant: 0,
        }
    }

    /// Accounts for `found`, the differences between the two observations of
    /// `scenario`, which an unexplained difference quotes.
    pub fn record(&mut self, scenario: &str, reference: &Value, port: &Value, found: Vec<String>) {
        self.scenarios += 1;
        if found.is_empty() {
            self.conformant += 1;
        }
        for pointer in found {
            match self
                .entries
                .iter()
                .position(|entry| entry.covers(scenario, &pointer))
            {
                Some(index) => {
                    *self.reproduced[index]
                        .entry(scenario.to_owned())
                        .or_default() += 1;
                }
                None => self.unexplained.push(format!(
                    "{scenario} {pointer}\n  reference: {}\n  port:      {}",
                    quote(reference, &pointer),
                    quote(port, &pointer)
                )),
            }
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
    /// then asserts every difference fell under an entry and every entry still
    /// reproduces.
    pub fn conclude(self, heading: &str) {
        let mut stale = Vec::new();
        for (entry, reproduced) in self.entries.iter().zip(&self.reproduced) {
            let scenarios = entry.scenarios();
            if scenarios.is_empty() {
                if reproduced.is_empty() {
                    stale.push(entry.label());
                }
            } else {
                stale.extend(
                    scenarios
                        .iter()
                        .filter(|scenario| !reproduced.contains_key(**scenario))
                        .map(|scenario| format!("{scenario} {}", entry.label())),
                );
            }
        }
        println!(
            "{heading} {}/{} scenarios conformant, {} ledgered differences across {} entries",
            self.conformant,
            self.scenarios,
            self.reproduced
                .iter()
                .flat_map(BTreeMap::values)
                .sum::<usize>(),
            self.entries.len()
        );
        assert!(
            self.unexplained.is_empty(),
            "the port departs from the corpus where no ledger entry says it may:\n{}",
            self.unexplained.join("\n")
        );
        assert!(
            stale.is_empty(),
            "these ledger entries no longer reproduce and should be removed: {stale:?}"
        );
    }
}

fn quote(observation: &Value, pointer: &str) -> String {
    observation
        .pointer(pointer)
        .map_or_else(|| "<absent>".to_owned(), Value::to_string)
}

/// A committed corpus replayed against this port through its capture script.
pub struct Replay<'a> {
    /// What the summary line opens with, such as `review parity:`.
    pub heading: &'a str,
    /// The committed corpus.
    pub corpus: &'a str,
    /// The scenarios the corpus may not fall below, so a recapture that lost
    /// coverage fails here and not only on the machine that made it.
    pub floor: usize,
    pub capture: &'a Capture,
    /// What hands the script this port's binary, such as `--server` and the
    /// stdio fixture.
    pub port: &'a [&'a str],
    /// Where the port's capture is written.
    pub output: &'a Path,
    pub prose: Prose,
}

impl Replay<'_> {
    /// Captures this port, compares every scenario with the corpus under
    /// `entries`, and concludes the tally.
    pub fn run<'l>(&self, entries: impl IntoIterator<Item = &'l dyn Entry>) {
        let corpus: Value = serde_json::from_str(self.corpus).expect("the corpus parses");
        let recorded = scenarios(&corpus);
        assert!(
            recorded.len() >= self.floor,
            "the corpus holds {} scenarios, below the floor of {}",
            recorded.len(),
            self.floor
        );
        let replayed = self.capture.port(self.port, self.output);
        let mut tally = Tally::new(entries);
        tally.compare(
            self.capture,
            &recorded,
            &scenarios(&replayed),
            |reference, port| differences(reference, port, self.prose),
        );
        tally.conclude(self.heading);
    }
}
