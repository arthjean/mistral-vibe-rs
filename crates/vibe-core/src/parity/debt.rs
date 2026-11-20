//! The measured debt of a corpus: the gaps of the row it measures that a
//! recapture revealed, kept as a generated file rather than as ledger entries.
//!
//! A ledger entry is a decision someone wrote: a divergence the scorecard
//! accepts, a text this port words its own way, or a gap of another row. A gap
//! of the row a corpus measures is not a decision but a measurement, so no
//! ledger may keep one ([`super::replay::assert_ledger`]). When a recapture
//! reveals such gaps, as a re-pin does, `python3 scripts/parity/repin.py debt
//! <corpus>` measures them and writes the corpus's debt under
//! [`DIRECTORY`]: for every replay heading, every exact scenario and every
//! pointer with its indices generalized, how many differences no ledger entry
//! covered. The file holds no reference text, only names and counts.
//!
//! **Provenance.** The manifest entry of the corpus records the debt's
//! SHA-256 and the corpus SHA-256 it was measured against, which has to be the
//! corpus's own, so a recapture re-measures the debt and a debt edited by hand
//! is refused.
//!
//! **The ratchet.** A replay consults the debt for a difference no entry
//! covers, or one an own-words entry refuses, and the figures count what the
//! debt covers as gap differences. A difference outside the debt fails the
//! replay as unexplained; a group that covers more differences than it
//! declares is a regression; a group that covers fewer fails until
//! `repin.py debt` records the progress. That command raises a debt only when
//! the corpus changed since it was measured, so the debt of an unchanged
//! corpus can only shrink.
//!
//! **The pin.** A count alone cannot see a change inside a gap: a port
//! that answers one wrong value where it answered another still differs once.
//! So a group a replay absorbs with the port's values ([`Debt::absorb_value`])
//! also pins what the port answered there, at the firmest [`Level`] its
//! measuring runs agreed on: every pointer with its value, the values alone,
//! or their shapes (a type and a length). A replay whose group holds its count
//! but answers another fingerprint fails until `repin.py debt` records the
//! change, which it may while the count does not grow. The debt is measured
//! on Linux, where the corpora are captured, and only a Linux run checks a
//! pin: another host compares the counts, since what the port answers inside
//! a gap can name a path of that host.
//!
//! **Measuring.** With [`MEASURE`] set, a replay's debt covers every
//! difference no entry covers and writes what it covered to
//! `target/parity/debt/`, which is what `repin.py debt` reads. The replay's
//! other assertions still run, so a measurement only completes over a sound
//! ledger.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the debt is test machinery: a check that fails is the calling test failing"
)]

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::live::records;
use super::manifest::{self, REPIN};
use super::replay::repository;

/// Where the debts are committed, relative to the workspace root.
pub const DIRECTORY: &str = "scripts/parity/debt";

/// The debt schema this module reads and `repin.py debt` writes.
pub const SCHEMA: u64 = 2;

/// Whether this host checks the pins of a debt: the host the debts are
/// measured on.
pub const CHECKS_PINS: bool = cfg!(target_os = "linux");

/// The variable that turns a replay's debt into a measurement.
pub const MEASURE: &str = "VIBE_PARITY_DEBT_MEASURE";

/// Where a measuring replay writes what its debt covered, under the records
/// directory.
pub const MEASUREMENTS: &str = "debt";

/// The debt file of the corpus `listed`, relative to the workspace root: its
/// path under `crates/`, each separator written `--`.
#[must_use]
pub fn path_of(listed: &str) -> String {
    let relative = listed.strip_prefix("crates/").unwrap_or(listed);
    format!("{DIRECTORY}/{}", relative.replace('/', "--"))
}

/// Whether this run measures debts instead of checking them.
#[must_use]
pub fn measuring() -> bool {
    std::env::var_os(MEASURE).is_some_and(|value| !value.is_empty())
}

/// `pointer` with every array index written `*`, which is how a debt groups
/// the differences of one scenario.
#[must_use]
pub fn generalize(pointer: &str) -> String {
    pointer
        .split('/')
        .map(|segment| {
            if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) {
                "*"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// One scenario and one generalized pointer.
type Group = (String, String);

/// How firmly a debt group pins what the port answers inside it, firmest
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Every difference's pointer, indices included, with the port's value.
    Exact,
    /// The port's values, wherever in the group they sit.
    Values,
    /// The type and the length of each value the port answered.
    Shape,
}

impl Level {
    /// Every level, firmest first, as a measurement records them.
    pub const ALL: [Self; 3] = [Self::Exact, Self::Values, Self::Shape];

    /// The name a debt file writes.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Values => "values",
            Self::Shape => "shape",
        }
    }

    /// The level a debt file names.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.name() == name)
    }
}

/// What a group pins: a fingerprint of the port's answers at one level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub level: Level,
    pub fingerprint: String,
}

/// A group as a debt declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    pub count: usize,
    /// [`None`] for a group whose answers moved between measuring runs, or
    /// that a replay absorbs without the port's values.
    pub pin: Option<Pin>,
}

/// What one run absorbed into a group.
#[derive(Debug, Default)]
struct Absorbed {
    count: usize,
    /// Every difference's pointer and the port's value there (`None` where
    /// the port has none), while every absorption carried one.
    answers: Option<Vec<(String, Option<Value>)>>,
}

/// The canonical text of `value`: keys sorted at every depth, so two equal
/// values read alike whatever order a capture wrote them in.
fn canonical(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let ordered: BTreeMap<&String, Value> = object
                    .iter()
                    .map(|(key, child)| (key, sorted(child)))
                    .collect();
                let mut map = serde_json::Map::new();
                for (key, child) in ordered {
                    map.insert(key.clone(), child);
                }
                Value::Object(map)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

/// The type and the length of a port answer, which [`Level::Shape`] pins.
fn shape(value: Option<&Value>) -> String {
    match value {
        None => "absent".to_owned(),
        Some(value) if super::replay::is_digest(value) => {
            format!("prose:{}", value["prose"])
        }
        Some(Value::Null) => "null".to_owned(),
        Some(Value::Bool(_)) => "bool".to_owned(),
        Some(Value::Number(number)) if number.is_f64() => "float".to_owned(),
        Some(Value::Number(_)) => "integer".to_owned(),
        Some(Value::String(text)) => format!("string:{}", text.chars().count()),
        Some(Value::Array(items)) => format!("array:{}", items.len()),
        Some(Value::Object(object)) => format!("object:{}", object.len()),
    }
}

/// The fingerprint of `answers` at `level`: the first 16 hexadecimal digits of
/// the SHA-256 of their sorted lines, so the order the differences were found
/// in does not count.
#[must_use]
pub fn fingerprint(level: Level, answers: &[(String, Option<Value>)]) -> String {
    let value = |answer: &Option<Value>| {
        answer
            .as_ref()
            .map_or_else(|| "<absent>".to_owned(), canonical)
    };
    let mut lines: Vec<String> = answers
        .iter()
        .map(|(pointer, answer)| match level {
            Level::Exact => format!("{pointer}\t{}", value(answer)),
            Level::Values => value(answer),
            Level::Shape => shape(answer.as_ref()),
        })
        .collect();
    lines.sort();
    crate::text::sha256_hex(lines.join("\n").as_bytes())[..16].to_owned()
}

/// The debt one replay heading keeps over one corpus.
#[derive(Debug)]
pub struct Debt {
    listed: String,
    heading: String,
    declared: BTreeMap<Group, Declared>,
    measured: BTreeMap<Group, Absorbed>,
    measuring: bool,
}

impl Debt {
    /// The debt `heading` keeps over the corpus `listed` (a workspace-relative
    /// path the manifest lists), empty when the corpus keeps none.
    ///
    /// # Panics
    ///
    /// When the corpus is unlisted, or its debt file is missing, edited by
    /// hand, or measured against another corpus than the committed one.
    #[must_use]
    pub fn load(listed: &str, heading: &str) -> Self {
        let measuring = measuring();
        let declared = if measuring {
            BTreeMap::new()
        } else {
            declared(listed, Some(heading))
        };
        Self {
            listed: listed.to_owned(),
            heading: heading.to_owned(),
            declared,
            measured: BTreeMap::new(),
            measuring,
        }
    }

    /// A debt over `declared` groups, none of them pinned, for a test of the
    /// ratchet.
    #[must_use]
    pub fn of(listed: &str, heading: &str, declared: BTreeMap<Group, usize>) -> Self {
        Self::pinned(
            listed,
            heading,
            declared
                .into_iter()
                .map(|(group, count)| (group, Declared { count, pin: None }))
                .collect(),
        )
    }

    /// A debt over `declared` groups, for a test of the ratchet and its pins.
    #[must_use]
    pub fn pinned(listed: &str, heading: &str, declared: BTreeMap<Group, Declared>) -> Self {
        Self {
            listed: listed.to_owned(),
            heading: heading.to_owned(),
            declared,
            measured: BTreeMap::new(),
            measuring: false,
        }
    }

    /// Keeps only the groups of the scenarios `compared` says this run
    /// compared: a replay that measures a subset of its corpus on a host, such
    /// as a surface that host cannot drive, owes nothing for the rest. A
    /// scenario the corpus stops recording changes the corpus, which the debt
    /// is measured against, so no group is dropped unseen.
    pub fn retain_compared(&mut self, compared: impl Fn(&str) -> bool) {
        self.declared.retain(|(scenario, _), _| compared(scenario));
    }

    /// Covers the difference at `pointer` in `scenario` when the debt holds
    /// its group, or when this run measures, and says whether it did. Its
    /// group counts it without pinning what the port answered: a replay that
    /// has the port's value calls [`Debt::absorb_value`].
    pub fn absorb(&mut self, scenario: &str, pointer: &str) -> bool {
        self.take(scenario, pointer, None)
    }

    /// [`Debt::absorb`], also recording `port`, what the port answered at
    /// `pointer` (`None` where it has nothing), which the group's pin
    /// fingerprints.
    pub fn absorb_value(&mut self, scenario: &str, pointer: &str, port: Option<&Value>) -> bool {
        self.take(scenario, pointer, Some(port))
    }

    fn take(&mut self, scenario: &str, pointer: &str, port: Option<Option<&Value>>) -> bool {
        let group = (scenario.to_owned(), generalize(pointer));
        if !self.measuring && !self.declared.contains_key(&group) {
            return false;
        }
        let first = !self.measured.contains_key(&group);
        let absorbed = self.measured.entry(group).or_default();
        absorbed.count += 1;
        match port {
            Some(port) if first || absorbed.answers.is_some() => absorbed
                .answers
                .get_or_insert_with(Vec::new)
                .push((pointer.to_owned(), port.cloned())),
            _ => absorbed.answers = None,
        }
        true
    }

    /// The fingerprint at the level the group of `scenario` at the
    /// generalized `pointer` pins, of `answers`, or [`None`] when this host
    /// checks no pin or the group pins nothing.
    #[must_use]
    pub fn pin_of(
        &self,
        scenario: &str,
        pointer: &str,
        answers: &[(String, Option<Value>)],
    ) -> Option<String> {
        if !CHECKS_PINS {
            return None;
        }
        let pin = self
            .declared
            .get(&(scenario.to_owned(), pointer.to_owned()))?
            .pin
            .as_ref()?;
        Some(fingerprint(pin.level, answers))
    }

    /// Whether the debt holds the group of the difference at `pointer` in
    /// `scenario`, without covering it.
    #[must_use]
    pub fn holds(&self, scenario: &str, pointer: &str) -> bool {
        self.measuring
            || self
                .declared
                .contains_key(&(scenario.to_owned(), generalize(pointer)))
    }

    /// How many differences the debt has covered.
    #[must_use]
    pub fn covered(&self) -> usize {
        self.measured.values().map(|absorbed| absorbed.count).sum()
    }

    /// How many groups the debt declares.
    #[must_use]
    pub fn groups(&self) -> usize {
        self.declared.len()
    }

    /// What the ratchet refuses: every group that covered more differences
    /// than it declares (a regression) or fewer (progress to record). A
    /// measuring run writes what it covered instead and refuses nothing.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        if self.measuring {
            self.write_measurement();
            return Vec::new();
        }
        let command = format!("{REPIN} debt {}", self.listed);
        let mut problems = Vec::new();
        for ((scenario, pointer), declared) in &self.declared {
            let absorbed = self.measured.get(&(scenario.clone(), pointer.clone()));
            let covered = absorbed.map_or(0, |absorbed| absorbed.count);
            let expected = declared.count;
            if covered > expected {
                problems.push(format!(
                    "{scenario} {pointer} differs {covered} times where the debt measured \
                     {expected}: the port regressed inside its debt"
                ));
            } else if covered < expected {
                problems.push(format!(
                    "{scenario} {pointer} differs {covered} times where the debt measured \
                     {expected}: record the progress with `{command}`"
                ));
            } else if let (true, Some(pin)) = (CHECKS_PINS, &declared.pin) {
                let answered = absorbed
                    .and_then(|absorbed| absorbed.answers.as_deref())
                    .map(|answers| fingerprint(pin.level, answers));
                if answered.as_deref() != Some(pin.fingerprint.as_str()) {
                    problems.push(format!(
                        "{scenario} {pointer} still differs {covered} times, but the port answers \
                         something else there than when the debt was measured (its {} \
                         fingerprint is {}, not {}): a change inside a known gap; fix it, or \
                         record it with `{command}`",
                        pin.level.name(),
                        answered.as_deref().unwrap_or("unrecorded"),
                        pin.fingerprint
                    ));
                }
            }
        }
        problems
    }

    /// Asserts [`Debt::problems`] is empty.
    ///
    /// # Panics
    ///
    /// When a group of the debt moved.
    pub fn assert_settled(&self) {
        let problems = self.problems();
        assert!(
            problems.is_empty(),
            "the measured debt of `{}` over {} ({}) moved:\n  {}",
            self.heading,
            self.listed,
            path_of(&self.listed),
            problems.join("\n  ")
        );
    }

    fn write_measurement(&self) {
        let mut groups: BTreeMap<&str, BTreeMap<&str, Value>> = BTreeMap::new();
        for ((scenario, pointer), absorbed) in &self.measured {
            // A group absorbed with every answer records its fingerprint at
            // every level; `repin.py debt` keeps the firmest its runs agree on.
            let measured = absorbed.answers.as_deref().map_or_else(
                || json!(absorbed.count),
                |answers| {
                    let fingerprints: serde_json::Map<String, Value> = Level::ALL
                        .into_iter()
                        .map(|level| (level.name().to_owned(), json!(fingerprint(level, answers))))
                        .collect();
                    json!({"count": absorbed.count, "fingerprints": fingerprints})
                },
            );
            groups
                .entry(scenario.as_str())
                .or_default()
                .insert(pointer.as_str(), measured);
        }
        let name = crate::text::sha256_hex(format!("{}\n{}", self.listed, self.heading).as_bytes());
        let directory = records().join(MEASUREMENTS);
        let written = std::fs::create_dir_all(&directory).and_then(|()| {
            std::fs::write(
                directory.join(format!("{}.json", &name[..16])),
                format!(
                    "{}\n",
                    json!({
                        "corpus": self.listed,
                        "heading": self.heading,
                        "groups": groups,
                    })
                ),
            )
        });
        written.unwrap_or_else(|error| {
            panic!(
                "cannot write the debt measurement of `{}` under {}: {error}",
                self.heading,
                directory.display()
            )
        });
    }
}

/// Why part of a debt differs: what someone learned about the gap, kept
/// beside the measurement. A note covers nothing and declares no count, so it
/// cannot absorb a difference; it has to explain at least one group of the
/// committed debt ([`assert_notes`]), so it goes when the gap it explains
/// closes.
#[derive(Debug)]
pub struct Note {
    /// The scenarios it explains, each as
    /// [`super::replay::scenario_matches`] reads a pattern.
    pub scenarios: &'static [&'static str],
    /// The pointers it explains, each as [`super::replay::pointer_matches`]
    /// reads a pattern, array indices written `*` as a debt groups them.
    pub pointers: &'static [&'static str],
    pub reason: &'static str,
}

impl Note {
    /// Whether the note explains the debt group of `scenario` at the
    /// generalized `pointer`.
    #[must_use]
    pub fn explains(&self, scenario: &str, pointer: &str) -> bool {
        self.scenarios
            .iter()
            .any(|pattern| super::replay::scenario_matches(pattern, scenario))
            && self
                .pointers
                .iter()
                .any(|pattern| super::replay::pointer_matches(pattern, pointer))
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.scenarios.is_empty() || self.pointers.is_empty() || self.reason.is_empty() {
            problems.push(format!(
                "the debt note on {:?} needs scenarios, pointers and a reason",
                self.pointers
            ));
        }
        if let Some(pattern) = self
            .pointers
            .iter()
            .find(|pattern| !pattern.starts_with('/') || generalize(pattern) != **pattern)
        {
            problems.push(format!(
                "the debt note pattern {pattern} is not a pointer with its indices written `*`"
            ));
        }
        problems
    }
}

/// Asserts every note of `notes` is declared soundly and explains at least
/// one group of the measured debt of `listed`, under any heading, as
/// committed.
///
/// # Panics
///
/// When a note is malformed or explains no measured difference.
pub fn assert_notes(listed: &str, notes: &[Note]) {
    // A measuring run is rewriting the debt the notes are checked against.
    if measuring() {
        return;
    }
    let groups = declared(listed, None);
    let mut problems: Vec<String> = notes.iter().flat_map(Note::problems).collect();
    for note in notes {
        if !groups
            .keys()
            .any(|(scenario, pointer)| note.explains(scenario, pointer))
        {
            problems.push(format!(
                "the debt note on {:?} in {:?} explains no group of the measured debt of \
                 {listed}; the gap closed, so remove the note",
                note.pointers, note.scenarios
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "the debt notes of {listed} are unsound:\n  {}",
        problems.join("\n  ")
    );
}

/// The groups the committed debt of `listed` declares for `heading`, or
/// under every heading when it is `None`.
fn declared(listed: &str, heading: Option<&str>) -> BTreeMap<Group, Declared> {
    let manifest = manifest::load();
    let entry = manifest
        .entry(listed)
        .unwrap_or_else(|| panic!("{listed} is not listed in {}", manifest::MANIFEST));
    let Some(record) = &entry.debt else {
        return BTreeMap::new();
    };
    let command = format!("{REPIN} debt {listed}");
    if entry.sha256.as_deref() != Some(record.measured_against.as_str()) {
        panic!(
            "{listed} was recaptured since its debt was measured against {}; measure it again \
             with `{command}`",
            record.measured_against
        );
    }
    let path = path_of(listed);
    let text = std::fs::read_to_string(repository().join(&path))
        .unwrap_or_else(|error| panic!("{path} is unreadable ({error}); `{command}` writes it"));
    if crate::text::sha256_hex(text.as_bytes()) != record.sha256 {
        panic!(
            "{path} changed outside `{command}`, which is the only sanctioned way to change a \
             debt: its SHA-256 is not the one {} records",
            manifest::MANIFEST
        );
    }
    let document: Value =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path} is not JSON: {error}"));
    let problems = document_problems(&document, listed, &record.measured_against);
    assert!(
        problems.is_empty(),
        "{path} is not the debt of {listed}; measure it again with `{command}`:\n  {}",
        problems.join("\n  ")
    );
    match heading {
        Some(heading) => groups(&document, heading),
        None => document["tallies"]
            .as_object()
            .into_iter()
            .flatten()
            .flat_map(|(heading, _)| groups(&document, heading))
            .collect(),
    }
}

/// Every rule the debt of `entry` breaks: a file the manifest records no
/// debt for, a debt edited by hand, measured against another corpus than the
/// committed one, malformed, or kept for a heading no replay reads. A heading
/// is read when a line of the figures table carries it (`headings`) or, for a
/// family whose replay asserts no figures, when a Rust source of the
/// workspace names it as a literal ([`named_in_sources`]).
#[must_use]
pub fn entry_problems(
    entry: &manifest::ManifestEntry,
    headings: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let path = path_of(&entry.corpus);
    let file = repository().join(&path);
    let Some(record) = &entry.debt else {
        return if file.exists() {
            vec![format!(
                "{path} is the debt of {}, which the manifest records no debt for",
                entry.corpus
            )]
        } else {
            Vec::new()
        };
    };
    let command = format!("`{REPIN} debt {}`", entry.corpus);
    let mut problems = Vec::new();
    if entry.sha256.as_deref() != Some(record.measured_against.as_str()) {
        problems.push(format!(
            "{} was recaptured since its debt was measured; measure it again with {command}",
            entry.corpus
        ));
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) => {
            problems.push(format!(
                "{path} is unreadable ({error}); {command} writes it"
            ));
            return problems;
        }
    };
    if crate::text::sha256_hex(text.as_bytes()) != record.sha256 {
        problems.push(format!(
            "{path} changed outside {command}, the only sanctioned way to change a debt"
        ));
    }
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        problems.push(format!("{path} is not JSON"));
        return problems;
    };
    problems.extend(
        document_problems(&document, &entry.corpus, &record.measured_against)
            .into_iter()
            .map(|problem| format!("{path}: {problem}")),
    );
    for heading in document["tallies"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(heading, _)| heading)
    {
        if !headings.contains(heading) && !named_in_sources(heading) {
            problems.push(format!(
                "{path} keeps a debt for `{heading}`, which neither a line of the figures table \
                 nor a Rust source of the workspace names"
            ));
        }
    }
    problems
}

/// Whether a Rust source under `crates/` names `heading` as a string
/// literal, which is how a replay that asserts no figures line passes its
/// heading to [`Debt::load`].
#[must_use]
pub fn named_in_sources(heading: &str) -> bool {
    static SOURCES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let literal = format!("\"{heading}\"");
    SOURCES
        .get_or_init(|| {
            let mut sources = Vec::new();
            let mut pending = vec![repository().join("crates")];
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(&directory)
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let path = entry.path();
                    if path.is_dir() {
                        pending.push(path);
                    } else if path.extension().is_some_and(|extension| extension == "rs") {
                        sources.extend(std::fs::read_to_string(&path));
                    }
                }
            }
            sources
        })
        .iter()
        .any(|source| source.contains(&literal))
}

/// The committed debt files no corpus of `listed` owns.
#[must_use]
pub fn orphans(listed: &[&str]) -> Vec<String> {
    let owned: std::collections::BTreeSet<String> =
        listed.iter().map(|corpus| path_of(corpus)).collect();
    let Ok(directory) = std::fs::read_dir(repository().join(DIRECTORY)) else {
        return Vec::new();
    };
    let mut found: Vec<String> = directory
        .flatten()
        .filter_map(|file| file.file_name().into_string().ok())
        .filter(|name| name.ends_with(".json"))
        .map(|name| format!("{DIRECTORY}/{name}"))
        .filter(|path| !owned.contains(path))
        .map(|path| format!("{path} belongs to no corpus the manifest lists; remove it"))
        .collect();
    found.sort();
    found
}

/// What makes `document` something other than the debt of `listed` measured
/// against the corpus SHA-256 `measured_against`.
#[must_use]
pub fn document_problems(document: &Value, listed: &str, measured_against: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if document["schemaVersion"] != SCHEMA {
        problems.push(format!("its schemaVersion is not {SCHEMA}"));
    }
    if document["corpus"] != listed {
        problems.push(format!("it names the corpus {}", document["corpus"]));
    }
    if document["measuredAgainst"] != measured_against {
        problems.push(format!(
            "it was measured against {}",
            document["measuredAgainst"]
        ));
    }
    let Some(tallies) = document["tallies"].as_object() else {
        problems.push("it holds no tallies".to_owned());
        return problems;
    };
    if tallies.is_empty() {
        problems.push("it holds no tallies; a corpus without debt keeps no file".to_owned());
    }
    for (heading, scenarios) in tallies {
        let Some(scenarios) = scenarios.as_object().filter(|map| !map.is_empty()) else {
            problems.push(format!("`{heading}` holds no scenarios"));
            continue;
        };
        for (scenario, pointers) in scenarios {
            let Some(pointers) = pointers.as_object().filter(|map| !map.is_empty()) else {
                problems.push(format!("`{heading}` {scenario} holds no pointers"));
                continue;
            };
            for (pointer, group) in pointers {
                if !pointer.starts_with('/') || generalize(pointer) != *pointer {
                    problems.push(format!(
                        "`{heading}` {scenario} {pointer} is not a pointer with its indices \
                         generalized"
                    ));
                }
                if declared_group(group).is_none() {
                    problems.push(format!(
                        "`{heading}` {scenario} {pointer} declares no positive count, or a pin \
                         that is not a level and a 16-digit fingerprint"
                    ));
                }
            }
        }
    }
    problems
}

/// A group as a debt file writes it: a positive count, or a positive count
/// with the level and the fingerprint it pins.
#[must_use]
pub fn declared_group(group: &Value) -> Option<Declared> {
    let positive = |count: &Value| {
        count
            .as_u64()
            .filter(|count| *count > 0)
            .and_then(|count| usize::try_from(count).ok())
    };
    if let Some(count) = positive(group) {
        return Some(Declared { count, pin: None });
    }
    let object = group.as_object()?;
    if object.len() != 3 {
        return None;
    }
    let count = positive(object.get("count")?)?;
    let level = Level::named(object.get("level")?.as_str()?)?;
    let fingerprint = object.get("fingerprint")?.as_str()?;
    let sound = fingerprint.len() == 16
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    sound.then(|| Declared {
        count,
        pin: Some(Pin {
            level,
            fingerprint: fingerprint.to_owned(),
        }),
    })
}

/// The groups `document` declares for `heading`.
#[must_use]
pub fn groups(document: &Value, heading: &str) -> BTreeMap<Group, Declared> {
    let mut found = BTreeMap::new();
    for (scenario, pointers) in document["tallies"][heading]
        .as_object()
        .into_iter()
        .flatten()
    {
        for (pointer, group) in pointers.as_object().into_iter().flatten() {
            let declared = declared_group(group).unwrap_or(Declared {
                count: 0,
                pin: None,
            });
            found.insert((scenario.clone(), pointer.clone()), declared);
        }
    }
    found
}

#[cfg(test)]
mod debt_tests;
