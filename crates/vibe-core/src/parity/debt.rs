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
pub const SCHEMA: u64 = 1;

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

/// The debt one replay heading keeps over one corpus.
#[derive(Debug)]
pub struct Debt {
    listed: String,
    heading: String,
    declared: BTreeMap<Group, usize>,
    measured: BTreeMap<Group, usize>,
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

    /// A debt over `declared` groups, for a test of the ratchet.
    #[must_use]
    pub fn of(listed: &str, heading: &str, declared: BTreeMap<Group, usize>) -> Self {
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
    /// its group, or when this run measures, and says whether it did.
    pub fn absorb(&mut self, scenario: &str, pointer: &str) -> bool {
        let group = (scenario.to_owned(), generalize(pointer));
        if !self.measuring && !self.declared.contains_key(&group) {
            return false;
        }
        *self.measured.entry(group).or_default() += 1;
        true
    }

    /// How many differences the debt has covered.
    #[must_use]
    pub fn covered(&self) -> usize {
        self.measured.values().sum()
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
            let covered = self
                .measured
                .get(&(scenario.clone(), pointer.clone()))
                .copied()
                .unwrap_or(0);
            if covered > *declared {
                problems.push(format!(
                    "{scenario} {pointer} differs {covered} times where the debt measured \
                     {declared}: the port regressed inside its debt"
                ));
            } else if covered < *declared {
                problems.push(format!(
                    "{scenario} {pointer} differs {covered} times where the debt measured \
                     {declared}: record the progress with `{command}`"
                ));
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
        let mut groups: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
        for ((scenario, pointer), count) in &self.measured {
            groups
                .entry(scenario.as_str())
                .or_default()
                .insert(pointer.as_str(), *count);
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
fn declared(listed: &str, heading: Option<&str>) -> BTreeMap<Group, usize> {
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
            for (pointer, count) in pointers {
                if !pointer.starts_with('/') || generalize(pointer) != *pointer {
                    problems.push(format!(
                        "`{heading}` {scenario} {pointer} is not a pointer with its indices \
                         generalized"
                    ));
                }
                if count.as_u64().is_none_or(|count| count == 0) {
                    problems.push(format!(
                        "`{heading}` {scenario} {pointer} declares no positive count"
                    ));
                }
            }
        }
    }
    problems
}

/// The groups `document` declares for `heading`.
#[must_use]
pub fn groups(document: &Value, heading: &str) -> BTreeMap<Group, usize> {
    let mut found = BTreeMap::new();
    for (scenario, pointers) in document["tallies"][heading]
        .as_object()
        .into_iter()
        .flatten()
    {
        for (pointer, count) in pointers.as_object().into_iter().flatten() {
            let count = count
                .as_u64()
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0);
            found.insert((scenario.clone(), pointer.clone()), count);
        }
    }
    found
}

#[cfg(test)]
mod debt_tests;
