//! Differential oracle for configuration layer composition.
//!
//! The Python reference is the authority on how two layer documents combine.
//! `scripts/parity/config_surface.py` drives its `ConfigBuilder` merge over a
//! set of synthetic layer stacks and records, per scenario, the merged document
//! it produces plus the census of every field it declares. This module replays
//! that corpus against [`LayeredConfig::load`].
//!
//! The corpus is committed, unlike the tool-surface one: it carries field names,
//! merge strategies, merge keys, editor kinds and values authored for the
//! capture, and no reference-authored description text, which is what `NOTICE`
//! forbids shipping. Replay therefore runs unconditionally; only the live probe
//! that recaptures from the pinned checkout skips when it is absent.
//!
//! Two comparisons are deliberately scoped. The corpus records each field's
//! editor kind, but this module asserts only the merge strategy and merge key,
//! which is what composition depends on; kinds and defaults are asserted when
//! the surface is published, in US-064. And a key the reference schema does not
//! declare is dropped by the reference merge and kept by this one, which FR-04
//! requires: the corpus records those keys so the divergence is proved rather
//! than assumed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};

use super::registry::{FIELDS, MergeStrategy};
use super::*;

/// The reference commit the corpus is captured from. A checkout at any other
/// revision is not an oracle for this corpus.
const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";
const REFERENCE_ROOT: &str = "/home/arthur/dev/mistral-vibe";
/// Lets a workstation holding the checkout elsewhere still run the live probe,
/// under the same name the parity scripts read.
const REFERENCE_VARIABLE: &str = "VIBE_REFERENCE";
/// An interpreter that can import the reference package.
const INTERPRETER_VARIABLE: &str = "VIBE_PARITY_PYTHON";
const CAPTURE_SCRIPT: &str = "scripts/parity/config_surface.py";
const CORPUS_RELATIVE: &str = "tests/config-surface/corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The scenario floor this epic commits to.
const MINIMUM_SCENARIOS: usize = 24;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    strategies: Strategies,
    fields: Vec<ReferenceField>,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Strategies {
    declared: Vec<String>,
    used: Vec<String>,
    unused: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReferenceField {
    name: String,
    strategy: String,
    merge_key: Option<String>,
    #[expect(
        dead_code,
        reason = "asserted when the surface is published, in US-064"
    )]
    kind: String,
    #[expect(
        dead_code,
        reason = "asserted when the surface is published, in US-064"
    )]
    choices: Vec<String>,
    #[expect(dead_code, reason = "asserted by the settings surface, in US-068")]
    popular: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    name: String,
    layers: Vec<ScenarioLayer>,
    merged: Map<String, JsonValue>,
    dropped_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioLayer {
    name: String,
    toml: String,
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let path = corpus_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus was captured by another version of {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

#[test]
fn every_reference_field_is_declared_with_the_strategy_the_reference_uses() {
    let corpus = corpus();
    let declared = FIELDS
        .iter()
        .map(|spec| (spec.name, spec))
        .collect::<BTreeMap<_, _>>();
    let mut missing = Vec::new();
    for field in &corpus.fields {
        let Some(spec) = declared.get(field.name.as_str()) else {
            missing.push(field.name.clone());
            continue;
        };
        assert_eq!(
            spec.strategy.as_str(),
            field.strategy,
            "field `{}` merges by the wrong strategy",
            field.name
        );
        assert_eq!(
            spec.merge_key.map(str::to_owned),
            field.merge_key,
            "field `{}` declares the wrong merge key",
            field.name
        );
    }
    assert!(
        missing.is_empty(),
        "the registry does not declare these reference fields: {missing:?}"
    );
}

#[test]
fn the_port_implements_every_strategy_the_reference_reaches() {
    let corpus = corpus();
    let implemented = BTreeSet::from([
        MergeStrategy::Replace.as_str(),
        MergeStrategy::Concat.as_str(),
        MergeStrategy::Union.as_str(),
        MergeStrategy::DeepMerge.as_str(),
    ]);
    let used = corpus
        .strategies
        .used
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        used, implemented,
        "a reference field adopted a strategy this port does not implement"
    );
    // `WithShallowMerge` and `WithConflictMerge` exist in the reference
    // vocabulary under the strategy values `merge` and `conflict`; no field
    // declares either, which is why neither is implemented here.
    assert_eq!(corpus.strategies.unused, ["conflict", "merge"]);
    assert!(
        corpus.strategies.declared.len() == used.len() + corpus.strategies.unused.len(),
        "the reference strategy vocabulary changed"
    );
}

#[test]
fn every_scenario_composes_the_document_the_reference_produces() {
    let corpus = corpus();
    assert!(
        corpus.scenarios.len() >= MINIMUM_SCENARIOS,
        "the corpus covers {} scenarios, below the {MINIMUM_SCENARIOS} this epic commits to",
        corpus.scenarios.len()
    );
    for scenario in &corpus.scenarios {
        replay(scenario);
    }
}

fn replay(scenario: &Scenario) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");

    // Every layer is composed through the real load: the lowest becomes the
    // defaults document, the next the selected file, and the rest the
    // experiments, runtime and agent layers, in the order `load` composes them.
    let mut documents = scenario.layers.iter().map(|layer| {
        layer
            .toml
            .parse::<Table>()
            .unwrap_or_else(|error| panic!("{}: layer `{}`: {error}", scenario.name, layer.name))
    });
    let mut config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home.clone(),
            working_directory: temporary.path().join("project"),
        },
        documents.next().unwrap_or_default(),
    );
    if let Some(selected) = documents.next() {
        fs::write(home.join(CONFIG_FILE), selected.to_string()).expect("selected fixture");
    }
    if let Some(experiments) = documents.next() {
        config.experiments = experiments;
    }
    if let Some(runtime) = documents.next() {
        config.runtime = runtime;
    }
    if let Some(agent) = documents.next() {
        config.agent = agent;
    }
    assert!(
        documents.next().is_none(),
        "{}: a scenario may stack at most five layers",
        scenario.name
    );

    let effective = config
        .load()
        .unwrap_or_else(|error| panic!("{}: {error}", scenario.name))
        .effective;
    let actual = serde_json::to_value(&effective).expect("effective document serializes");

    for (key, expected) in &scenario.merged {
        // TOML has no null, so a field the reference merged to `None` is absent
        // here rather than present and empty.
        if expected.is_null() {
            assert!(
                !effective.contains_key(key),
                "{}: `{key}` should be absent where the reference merged it to null",
                scenario.name
            );
            continue;
        }
        let Some(found) = actual.get(key) else {
            panic!(
                "{}: `{key}` is missing from the merged document",
                scenario.name
            );
        };
        if let Some((pointer, want, got)) = difference(&format!("/{key}"), expected, found) {
            panic!(
                "{}: merged configuration diverges at {pointer}: reference {want}, port {got}",
                scenario.name
            );
        }
    }

    let unexpected = effective
        .keys()
        .filter(|key| registry::field(key).is_some())
        .filter(|key| !scenario.merged.contains_key(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "{}: the port composed fields the reference did not: {unexpected:?}",
        scenario.name
    );

    let dropped = effective
        .keys()
        .filter(|key| registry::field(key).is_none())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        dropped, scenario.dropped_keys,
        "{}: the keys the reference drops and this port preserves changed",
        scenario.name
    );
}

/// The first JSON pointer at which two documents disagree, with both values
/// rendered and any value under a sensitive key redacted.
fn difference(
    pointer: &str,
    expected: &JsonValue,
    actual: &JsonValue,
) -> Option<(String, String, String)> {
    match (expected, actual) {
        (JsonValue::Object(expected), JsonValue::Object(actual)) => {
            for (key, value) in expected {
                let nested = format!("{pointer}/{}", escape_pointer_token(key));
                let Some(found) = actual.get(key) else {
                    return Some((nested, render(key, value), "absent".to_owned()));
                };
                if let Some(found) = difference(&nested, value, found) {
                    return Some(found);
                }
            }
            for key in actual.keys() {
                if !expected.contains_key(key) {
                    let nested = format!("{pointer}/{}", escape_pointer_token(key));
                    return Some((nested, "absent".to_owned(), render(key, &actual[key])));
                }
            }
            None
        }
        (JsonValue::Array(expected_entries), JsonValue::Array(actual_entries)) => {
            if expected_entries.len() != actual_entries.len() {
                return Some((
                    pointer.to_owned(),
                    format!("{} entries", expected_entries.len()),
                    format!("{} entries", actual_entries.len()),
                ));
            }
            expected_entries
                .iter()
                .zip(actual_entries)
                .enumerate()
                .find_map(|(index, (expected, actual))| {
                    difference(&format!("{pointer}/{index}"), expected, actual)
                })
        }
        (expected, actual) if expected == actual => None,
        (expected, actual) => {
            let key = pointer.rsplit('/').next().unwrap_or_default();
            Some((
                pointer.to_owned(),
                render(key, expected),
                render(key, actual),
            ))
        }
    }
}

fn render(key: &str, value: &JsonValue) -> String {
    if is_sensitive_key(key) {
        return "[redacted]".to_owned();
    }
    redacted(value).to_string()
}

/// Redacts every nested value whose own key is sensitive.
///
/// A divergence at a key one side omits renders that side's whole subtree, so
/// checking only the key at the pointer would let a nested secret through.
fn redacted(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        JsonValue::from("[redacted]")
                    } else {
                        redacted(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        JsonValue::Array(entries) => JsonValue::Array(entries.iter().map(redacted).collect()),
        value => value.clone(),
    }
}

fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// No corpus scenario diverges in a passing suite, so the message a divergence
/// would produce is proved here instead: it has to name the pointer and both
/// values, and it must never carry a value read from a sensitive-named key.
#[test]
fn a_divergence_names_its_pointer_and_redacts_a_sensitive_value() {
    let (pointer, expected, actual) = difference(
        "",
        &serde_json::json!({"theme": "system"}),
        &serde_json::json!({"theme": "nord"}),
    )
    .expect("the two documents disagree on the theme");
    assert_eq!(pointer, "/theme");
    assert_eq!(expected, "\"system\"");
    assert_eq!(actual, "\"nord\"");

    let (pointer, expected, actual) = difference(
        "",
        &serde_json::json!({
            "providers": [{"name": "mistral", "api_key_env_var": "REFERENCE_VALUE"}]
        }),
        &serde_json::json!({
            "providers": [{"name": "mistral", "api_key_env_var": "PORT_VALUE"}]
        }),
    )
    .expect("the two documents disagree under a sensitive key");
    assert_eq!(pointer, "/providers/0/api_key_env_var");
    assert!(is_sensitive_key("api_key_env_var"));
    assert_eq!(expected, "[redacted]");
    assert_eq!(actual, "[redacted]");

    // A key present on one side only renders that side's whole subtree, so the
    // redaction has to reach a sensitive key nested inside it.
    let (pointer, expected, actual) = difference(
        "",
        &serde_json::json!({"tools": {"bash": {"token": "reference", "timeout": 30}}}),
        &serde_json::json!({"tools": {}}),
    )
    .expect("the port dropped a nested table");
    assert_eq!(pointer, "/tools/bash");
    assert!(expected.contains("\"token\":\"[redacted]\""), "{expected}");
    assert!(!expected.contains("reference"), "{expected}");
    assert!(expected.contains("\"timeout\":30"), "{expected}");
    assert_eq!(actual, "absent");
}

/// The pinned checkout and an interpreter that can drive it, or `None` when the
/// live probe cannot run here. Every replay above still ran against the
/// committed corpus.
fn pinned_reference() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var_os(REFERENCE_VARIABLE)
        .map_or_else(|| PathBuf::from(REFERENCE_ROOT), PathBuf::from);
    if !root.is_dir() {
        return None;
    }
    let revision = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !revision.status.success() {
        return None;
    }
    let head = String::from_utf8(revision.stdout).ok()?;
    if head.trim() != REFERENCE_COMMIT {
        eprintln!(
            "skipping the live configuration oracle probe: {} is at {}, not the pinned {REFERENCE_COMMIT}",
            root.display(),
            head.trim()
        );
        return None;
    }
    let interpreter = std::env::var_os(INTERPRETER_VARIABLE)
        .map(PathBuf::from)
        .into_iter()
        .chain([
            root.join(".venv/bin/python"),
            root.join(".venv/Scripts/python.exe"),
        ])
        .find(|candidate| candidate.is_file())?;
    Some((root, interpreter))
}

#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some((root, interpreter)) = pinned_reference() else {
        eprintln!(
            "skipping the live configuration oracle probe: no pinned checkout to capture from"
        );
        return;
    };
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("corpus.json");
    let output = Command::new(&interpreter)
        .arg(workspace.join(CAPTURE_SCRIPT))
        .arg("--reference")
        .arg(&root)
        .arg("--output")
        .arg(&captured)
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
        recaptured.replace("\r\n", "\n"),
        committed.replace("\r\n", "\n"),
        "the committed corpus no longer matches the pinned reference; rerun {CAPTURE_SCRIPT}"
    );
}
