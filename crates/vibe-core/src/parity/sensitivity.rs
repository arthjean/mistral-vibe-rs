//! The sensitivity pass: how much of a deliberate change a replay can see.
//!
//! A replay that passes says the port matches the corpus wherever its differ
//! looks and its ledger does not excuse. That claim is only as strong as what
//! the differ and the ledger could have reported, so every [`super::replay::Tally`]
//! ends, once its own assertions hold, by applying mutation operators to the
//! port's observations and asking whether the replay would have failed.
//!
//! **Operators.** [`Operator`] lists them: delete a key, add a key, change a
//! scalar's value, change a scalar's type, change a string's length, reorder
//! a list, drop a list element. A prose digest (`{"prose", "sha256"}`, what a
//! capture records for authored text) is a string to them: its length or its
//! hash changes, never its keys.
//!
//! **Sampling.** Every applicable site of every scenario is ranked by a hash
//! of the [`SEED`], the corpus, the heading, the operator, the scenario and
//! the pointer, and the first [`DEFAULT_SAMPLES`] sites per operator are
//! mutated, so the same tree always measures the same mutations.
//! [`SAMPLES_VARIABLE`] raises the count, or `all` mutates every site, for a
//! full matrix on demand.
//!
//! **Detection.** A mutated scenario is evaluated as the replay evaluates it:
//! the replay's differ, the first ledger entry that covers each pointer and
//! admits its values, then the measured debt. The mutation is detected when a
//! difference falls outside both, or when the number of differences any
//! entry or debt group covers changes, since each of those fails the replay
//! ([`super::replay::Tally::conclude`]). A mutation the evaluation answers
//! exactly as it answered the unmutated scenario is undetected.
//!
//! **Masked and unseen.** An undetected mutation that lands on a difference
//! the unmutated scenario already held (its pointer at, under or above one) is
//! *masked*: the ledger entry or debt group that counts that difference
//! counts it once whatever it holds, and is the entry that says why the
//! oracle cannot see the change. The pass attributes every masked mutation to
//! that entry by label (`maskedBy` in the matrix). Any other undetected
//! mutation is *unseen*: the differ or the ledger cannot see the change at
//! all.
//!
//! **The threshold.** A replay detects at least [`THRESHOLD`] of what it
//! applied, masked mutations counted as undetected. Below it, every operator
//! with an unseen mutation needs a [`Blind`] entry stating why the oracle
//! cannot see it. A blind entry for an operator with no unseen mutation is
//! stale and fails, as a ledger entry that stops reproducing does.
//!
//! **The matrix.** Every pass writes what it applied and detected per
//! operator, with the seed and the corpus's scorecard row, to
//! `target/parity/sensitivity/<name>.json` and appends it to
//! `target/parity/sensitivity/matrix.jsonl`, the operator-by-row matrix
//! `scripts/parity/live_summary.py` prints in a CI job summary.
//!
//! **Normalization.** A black-box driver normalizes what the subject wrote
//! before any replay sees it, so a change its normalization erases is
//! invisible here. A [`super::replay::Replay`] whose driver re-normalizes
//! (`scripts/parity/sensitivity.py`) captures the port with `--raw` and runs
//! the driver's `--renormalize`, which mutates the raw runs with the same
//! operators and reports what came out of normalization unchanged; the
//! matrix records that report under `normalization`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the sensitivity pass is test machinery: a check that fails is the calling test \
              failing"
)]

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value, json};

use super::live::records;

/// The seed every pass samples from, recorded in the matrix.
pub const SEED: u64 = 0x5e25_1717_ab1e_0023;

/// How many sites per operator a pass mutates by default.
pub const DEFAULT_SAMPLES: usize = 24;

/// The variable that overrides [`DEFAULT_SAMPLES`]: a count, or `all`.
pub const SAMPLES_VARIABLE: &str = "VIBE_PARITY_SENSITIVITY_SAMPLES";

/// The least share of applied mutations a replay detects without declaring
/// what it cannot see.
pub const THRESHOLD: f64 = 0.95;

/// Where the matrix is written, under the records directory.
pub const DIRECTORY: &str = "sensitivity";

/// The key [`Operator::AddKey`] inserts, which no capture records.
pub const PROBE_KEY: &str = "__sensitivityProbe";

/// One way of changing an observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operator {
    /// Removes one key of an object.
    DeleteKey,
    /// Adds [`PROBE_KEY`] to an object.
    AddKey,
    /// Changes a string, number, boolean or digest, keeping its type.
    ChangeValue,
    /// Replaces a scalar with one of another JSON type.
    ChangeType,
    /// Makes a string or a digest one character longer.
    ChangeLength,
    /// Rotates a list whose elements are not all equal.
    Reorder,
    /// Removes one element of a list.
    DropElement,
}

impl Operator {
    /// Every operator, in the order the matrix lists them.
    pub const ALL: [Self; 7] = [
        Self::DeleteKey,
        Self::AddKey,
        Self::ChangeValue,
        Self::ChangeType,
        Self::ChangeLength,
        Self::Reorder,
        Self::DropElement,
    ];

    /// The name the matrix and `--mutate` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DeleteKey => "delete-key",
            Self::AddKey => "add-key",
            Self::ChangeValue => "change-value",
            Self::ChangeType => "change-type",
            Self::ChangeLength => "change-length",
            Self::Reorder => "reorder",
            Self::DropElement => "drop-element",
        }
    }
}

impl fmt::Display for Operator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// An operator class a replay cannot see, and why: the declaration its
/// unseen mutations need below [`THRESHOLD`].
#[derive(Clone, Copy, Debug)]
pub struct Blind {
    pub operator: Operator,
    /// Why the oracle cannot report this operator, such as an own-words
    /// entry whose band admits any rewording, or a differ that compares only
    /// some fields.
    pub reason: &'static str,
}

/// Whether `value` is a prose digest, the `{prose, sha256}` every capture
/// writes ([`super::replay::digest`]).
#[must_use]
pub fn is_digest(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("sha256") && object.contains_key("prose")
    })
}

/// The pointers of `value` at which `operator` applies.
#[must_use]
pub fn sites(value: &Value, operator: Operator) -> Vec<String> {
    let mut found = Vec::new();
    collect_sites(value, "", operator, &mut found);
    found
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn collect_sites(value: &Value, pointer: &str, operator: Operator, found: &mut Vec<String>) {
    if is_digest(value) {
        if matches!(
            operator,
            Operator::ChangeValue | Operator::ChangeType | Operator::ChangeLength
        ) {
            found.push(pointer.to_owned());
        }
        return;
    }
    match value {
        Value::Object(object) => {
            if operator == Operator::AddKey && !object.contains_key(PROBE_KEY) {
                found.push(pointer.to_owned());
            }
            for (key, child) in object {
                let child_pointer = format!("{pointer}/{}", escape(key));
                if operator == Operator::DeleteKey {
                    found.push(child_pointer.clone());
                }
                collect_sites(child, &child_pointer, operator, found);
            }
        }
        Value::Array(items) => {
            if operator == Operator::Reorder && items.windows(2).any(|pair| pair[0] != pair[1]) {
                found.push(pointer.to_owned());
            }
            for (index, child) in items.iter().enumerate() {
                let child_pointer = format!("{pointer}/{index}");
                if operator == Operator::DropElement {
                    found.push(child_pointer.clone());
                }
                collect_sites(child, &child_pointer, operator, found);
            }
        }
        Value::String(_) => {
            if matches!(
                operator,
                Operator::ChangeValue | Operator::ChangeType | Operator::ChangeLength
            ) {
                found.push(pointer.to_owned());
            }
        }
        Value::Number(_) | Value::Bool(_) => {
            if matches!(operator, Operator::ChangeValue | Operator::ChangeType) {
                found.push(pointer.to_owned());
            }
        }
        Value::Null => {
            if operator == Operator::ChangeType {
                found.push(pointer.to_owned());
            }
        }
    }
}

/// `pointer` split into its parent and its last segment, unescaped.
fn split(pointer: &str) -> Option<(&str, String)> {
    let (parent, last) = pointer.rsplit_once('/')?;
    Some((parent, last.replace("~1", "/").replace("~0", "~")))
}

/// `value` with `operator` applied at `pointer`, a site [`sites`] returned,
/// or [`None`] when the site does not hold what the operator changes.
#[must_use]
pub fn mutate(value: &Value, operator: Operator, pointer: &str) -> Option<Value> {
    let mut mutated = value.clone();
    let changed = match operator {
        Operator::DeleteKey => {
            let (parent, key) = split(pointer)?;
            mutated
                .pointer_mut(parent)?
                .as_object_mut()?
                .remove(&key)
                .is_some()
        }
        Operator::DropElement => {
            let (parent, index) = split(pointer)?;
            let index: usize = index.parse().ok()?;
            let items = mutated.pointer_mut(parent)?.as_array_mut()?;
            (index < items.len()).then(|| items.remove(index)).is_some()
        }
        Operator::AddKey => {
            let object = mutated.pointer_mut(pointer)?.as_object_mut()?;
            object
                .insert(PROBE_KEY.to_owned(), Value::Bool(true))
                .is_none()
        }
        Operator::Reorder => {
            let items = mutated.pointer_mut(pointer)?.as_array_mut()?;
            items.rotate_left(1);
            true
        }
        Operator::ChangeValue | Operator::ChangeType | Operator::ChangeLength => {
            let target = mutated.pointer_mut(pointer)?;
            let replacement = changed_scalar(target, operator)?;
            *target = replacement;
            true
        }
    };
    (changed && mutated != *value).then_some(mutated)
}

fn changed_scalar(value: &Value, operator: Operator) -> Option<Value> {
    if is_digest(value) {
        let mut object = value.as_object()?.clone();
        return match operator {
            Operator::ChangeValue => {
                let hash = object.get("sha256")?.as_str()?;
                let flipped = flip_last(hash);
                object.insert("sha256".to_owned(), Value::String(flipped));
                Some(Value::Object(object))
            }
            Operator::ChangeLength => {
                let length = object.get("prose")?.as_u64()?;
                object.insert("prose".to_owned(), json!(length + 1));
                Some(Value::Object(object))
            }
            Operator::ChangeType => Some(Value::String(String::new())),
            _ => None,
        };
    }
    match (operator, value) {
        (Operator::ChangeValue, Value::String(text)) => Some(Value::String(if text.is_empty() {
            "x".to_owned()
        } else {
            flip_last(text)
        })),
        (Operator::ChangeValue, Value::Bool(flag)) => Some(Value::Bool(!flag)),
        (Operator::ChangeValue, Value::Number(number)) => Some(
            number
                .as_i64()
                .and_then(|integer| integer.checked_add(1))
                .map(Value::from)
                .or_else(|| number.as_u64().map(|integer| Value::from(integer ^ 1)))
                .or_else(|| number.as_f64().map(|float| json!(float + 1.0)))?,
        ),
        (Operator::ChangeType, Value::String(text)) => Some(json!(text.chars().count())),
        (Operator::ChangeType, Value::Number(number)) => Some(Value::String(number.to_string())),
        (Operator::ChangeType, Value::Bool(flag)) => Some(Value::String(flag.to_string())),
        (Operator::ChangeType, Value::Null) => Some(Value::Bool(false)),
        (Operator::ChangeLength, Value::String(text)) => Some(Value::String(format!("{text}x"))),
        _ => None,
    }
}

/// `text` with its last character replaced by another one, so its length in
/// characters holds.
fn flip_last(text: &str) -> String {
    let mut characters: Vec<char> = text.chars().collect();
    if let Some(last) = characters.last_mut() {
        *last = if *last == 'x' { 'y' } else { 'x' };
    }
    characters.into_iter().collect()
}

/// A 64-bit hash of `parts`, stable across runs and hosts (FNV-1a, then the
/// `SplitMix64` finalizer).
#[must_use]
pub fn rank(seed: u64, parts: &[&str]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// How many sites per operator this run mutates, [`None`] for every site.
///
/// # Panics
///
/// When [`SAMPLES_VARIABLE`] holds neither a count nor `all`.
#[must_use]
pub fn samples() -> Option<usize> {
    match std::env::var(SAMPLES_VARIABLE) {
        Ok(value) if value == "all" => None,
        Ok(value) if !value.is_empty() => Some(value.parse().unwrap_or_else(|_| {
            panic!("{SAMPLES_VARIABLE} is `{value}`, neither a count nor `all`")
        })),
        _ => Some(DEFAULT_SAMPLES),
    }
}

/// One mutation the pass applied.
#[derive(Clone, Debug)]
pub struct Mutation {
    pub scenario: String,
    pub pointer: String,
    pub detected: bool,
    /// For a mutation that overlaps a difference the unmutated scenario
    /// already held (its pointer at or under one, or one under it), the
    /// label of the ledger entry or debt group that counts that difference.
    pub masked_by: Option<String>,
}

impl Mutation {
    /// Whether the replay missed it with no entry already counting the
    /// difference it landed on.
    #[must_use]
    pub const fn unseen(&self) -> bool {
        !self.detected && self.masked_by.is_none()
    }
}

/// What one pass applied and detected, per operator.
#[derive(Debug, Default)]
pub struct Matrix {
    pub operators: BTreeMap<Operator, Vec<Mutation>>,
    /// How many sites each operator could have mutated.
    pub sites: BTreeMap<Operator, usize>,
}

impl Matrix {
    /// How many mutations the pass applied.
    #[must_use]
    pub fn applied(&self) -> usize {
        self.operators.values().map(Vec::len).sum()
    }

    /// How many of them the replay would have reported.
    #[must_use]
    pub fn detected(&self) -> usize {
        self.operators
            .values()
            .flatten()
            .filter(|mutation| mutation.detected)
            .count()
    }

    /// The detected share, 1 when nothing applied.
    #[must_use]
    pub fn rate(&self) -> f64 {
        let applied = self.applied();
        if applied == 0 {
            return 1.0;
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "counts of a few thousand mutations are exact in an f64"
        )]
        let rate = self.detected() as f64 / applied as f64;
        rate
    }

    /// The operators with at least one undetected mutation.
    #[must_use]
    pub fn undetected_operators(&self) -> Vec<Operator> {
        self.operators
            .iter()
            .filter(|(_, mutations)| mutations.iter().any(|mutation| !mutation.detected))
            .map(|(operator, _)| *operator)
            .collect()
    }

    /// What the threshold refuses, given the blind entries a replay declares:
    /// below [`THRESHOLD`], an operator with unseen mutations and no blind
    /// entry; at any rate, a blind entry whose operator has no unseen
    /// mutation (stale) or that states no reason.
    #[must_use]
    pub fn problems(&self, blind: &[Blind]) -> Vec<String> {
        let mut problems = Vec::new();
        let unseen = |operator: Operator| -> Vec<&Mutation> {
            self.operators
                .get(&operator)
                .into_iter()
                .flatten()
                .filter(|mutation| mutation.unseen())
                .collect()
        };
        for entry in blind {
            if entry.reason.trim().is_empty() {
                problems.push(format!(
                    "the blind entry for {} states no reason",
                    entry.operator
                ));
            }
            if unseen(entry.operator).is_empty() {
                problems.push(format!(
                    "the blind entry for {} is stale: no {} mutation went unseen",
                    entry.operator, entry.operator
                ));
            }
        }
        if self.rate() < THRESHOLD {
            for operator in Operator::ALL {
                let missed = unseen(operator);
                if missed.is_empty() || blind.iter().any(|entry| entry.operator == operator) {
                    continue;
                }
                let applied = self.operators.get(&operator).map_or(0, Vec::len);
                let examples: Vec<String> = missed
                    .iter()
                    .take(5)
                    .map(|mutation| format!("{} {}", mutation.scenario, mutation.pointer))
                    .collect();
                problems.push(format!(
                    "{operator}: {} of {applied} mutations unseen, and no blind entry says why \
                     (for example {})",
                    missed.len(),
                    examples.join(", ")
                ));
            }
        }
        problems
    }

    /// The matrix as the record writes it.
    #[must_use]
    pub fn document(
        &self,
        heading: &str,
        corpus: &str,
        row: Option<&str>,
        blind: &[Blind],
    ) -> Value {
        let mut operators = Map::new();
        for operator in Operator::ALL {
            let mutations = self.operators.get(&operator).map_or(&[][..], Vec::as_slice);
            let undetected: Vec<Value> = mutations
                .iter()
                .filter(|mutation| !mutation.detected)
                .map(|mutation| {
                    json!({
                        "scenario": mutation.scenario,
                        "pointer": mutation.pointer,
                        "maskedBy": mutation.masked_by,
                    })
                })
                .collect();
            operators.insert(
                operator.name().to_owned(),
                json!({
                    "sites": self.sites.get(&operator).copied().unwrap_or(0),
                    "applied": mutations.len(),
                    "detected": mutations.iter().filter(|mutation| mutation.detected).count(),
                    "masked": mutations
                        .iter()
                        .filter(|mutation| mutation.masked_by.is_some())
                        .count(),
                    "undetected": undetected,
                }),
            );
        }
        json!({
            "heading": heading,
            "corpus": corpus,
            "row": row,
            "seed": format!("{SEED:#018x}"),
            "samples": samples().map_or(Value::String("all".to_owned()), Value::from),
            "applied": self.applied(),
            "detected": self.detected(),
            "rate": self.rate(),
            "threshold": THRESHOLD,
            "blind": blind
                .iter()
                .map(|entry| json!({"operator": entry.operator.name(), "reason": entry.reason}))
                .collect::<Vec<_>>(),
            "operators": operators,
        })
    }
}

/// How many raw mutations a renormalization report applied and how many its
/// normalization erased, summed over its operators.
#[must_use]
pub fn erasures(report: &Value) -> (u64, u64) {
    report["operators"].as_object().into_iter().flatten().fold(
        (0, 0),
        |(applied, erased), (_, operator)| {
            (
                applied + operator["applied"].as_u64().unwrap_or(0),
                erased + operator["erased"].as_u64().unwrap_or(0),
            )
        },
    )
}

/// Writes `document`, a [`Matrix::document`], under the records directory.
pub fn write(document: &Value) {
    let corpus = document["corpus"].as_str().unwrap_or_default();
    let heading = document["heading"].as_str().unwrap_or_default();
    let name = crate::text::sha256_hex(format!("{corpus}\n{heading}").as_bytes());
    let directory = records().join(DIRECTORY);
    let written = std::fs::create_dir_all(&directory).and_then(|()| {
        std::fs::write(
            directory.join(format!("{}.json", &name[..16])),
            format!("{document:#}\n"),
        )
    });
    if let Err(error) = written {
        eprintln!(
            "could not write the sensitivity matrix of `{heading}` under {}: {error}",
            directory.display()
        );
    }
    super::live::record(&format!("{DIRECTORY}/matrix.jsonl"), document);
}

#[cfg(test)]
mod sensitivity_tests;
