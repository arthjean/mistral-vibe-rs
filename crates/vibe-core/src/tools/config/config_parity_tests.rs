//! Differential oracle for the per-tool configuration surface.
//!
//! The Python reference is the authority on what an operator may write under
//! `tools.<name>`. `scripts/parity/tool_config.py` asks it through
//! `ToolManager.discover_tool_defaults`, which enumerates every builtin class
//! and returns the configuration model each one declares, and records the same
//! table as it reaches an operator through `create_default_config`. This module
//! replays that corpus against [`DECLARATIONS`] and against the document
//! [`ToolConfigResolver::published_defaults`] composes.
//!
//! The corpus is committed: it carries tool names, key names and default
//! values, which are observations, and no reference-authored description text,
//! which is what `NOTICE` forbids shipping. Replay therefore runs
//! unconditionally; only the live probe that recaptures from the pinned
//! checkout skips when it is absent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};

use super::*;
use crate::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

const CAPTURE_SCRIPT: &str = "scripts/parity/tool_config.py";
const CORPUS_RELATIVE: &str = "tests/tool-config/defaults.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    /// Which shell branch the capturing host composed the shell lists from.
    shell_branch: String,
    counts: Counts,
    keys: Vec<String>,
    shell_lists: BTreeMap<String, ShellLists>,
    tools: BTreeMap<String, Map<String, JsonValue>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counts {
    tools: usize,
    keys: usize,
    pairs: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellLists {
    allowlist: Vec<String>,
    denylist: Vec<String>,
    denylist_standalone: Vec<String>,
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let path = corpus_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("tool configuration corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("tool configuration corpus at {}: {error}", path.display()));
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

/// A resolver reading the shell branch the corpus was captured under, so the
/// replay compares like with like on either host.
fn captured_resolver(corpus: &Corpus) -> ToolConfigResolver {
    ToolConfigResolver::new().with_posix_shell(corpus.shell_branch == "posix")
}

fn as_json(table: &Table) -> Map<String, JsonValue> {
    let JsonValue::Object(object) = serde_json::to_value(table).expect("a table serializes") else {
        panic!("a table serializes to an object");
    };
    object
}

/// US-104: every tool class the reference declares configuration for is
/// declared here, with the same keys and the same defaults.
#[test]
fn the_declared_tool_configuration_matches_the_reference_enumeration() {
    let corpus = corpus();
    let resolver = captured_resolver(&corpus);
    let published = resolver.published_defaults();

    let expected_names = corpus.tools.keys().cloned().collect::<BTreeSet<_>>();
    let declared_names = DECLARATIONS
        .iter()
        .map(|declaration| declaration.tool.to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        declared_names, expected_names,
        "the declared tool classes diverge from the reference enumeration"
    );

    let mut divergences = Vec::new();
    let mut pairs = 0;
    for (tool, expected) in &corpus.tools {
        let document = published
            .get(tool.as_str())
            .and_then(Value::as_table)
            .unwrap_or_else(|| panic!("tool `{tool}` publishes no configuration document"));
        let actual = as_json(document);
        for (key, value) in expected {
            pairs += 1;
            match actual.get(key) {
                None => divergences.push(format!("{tool}.{key} has no declaration")),
                Some(found) if found != value => {
                    divergences.push(format!(
                        "{tool}.{key} is {found}, the reference says {value}"
                    ));
                }
                Some(_) => {}
            }
        }
        for key in actual.keys() {
            if !expected.contains_key(key) {
                divergences.push(format!(
                    "{tool}.{key} is declared here and nowhere upstream"
                ));
            }
        }
    }
    assert!(
        divergences.is_empty(),
        "the declared tool configuration diverges from the reference: {divergences:#?}"
    );

    let keys = corpus
        .tools
        .values()
        .flat_map(|entry| entry.keys().cloned())
        .collect::<BTreeSet<_>>();
    assert_eq!(keys.iter().cloned().collect::<Vec<_>>(), corpus.keys);
    assert_eq!(corpus.counts.tools, corpus.tools.len());
    assert_eq!(corpus.counts.keys, keys.len());
    assert_eq!(corpus.counts.pairs, pairs);
    println!(
        "tool configuration: {}/{} (tool, key) pairs declared across {} classes and {} keys, at {}",
        pairs,
        corpus.counts.pairs,
        corpus.counts.tools,
        corpus.counts.keys,
        &corpus.reference.commit[..12]
    );
}

/// The three host-composed shell lists are declared for both branches, so a
/// Windows host reads the lists the reference composes there rather than the
/// POSIX ones this corpus was captured under.
#[test]
fn both_shell_branches_declare_the_reference_lists() {
    let corpus = corpus();
    for (branch, expected) in &corpus.shell_lists {
        let posix = branch == "posix";
        assert_eq!(
            shell_allowlist(posix),
            expected.allowlist,
            "the {branch} shell allowlist diverges"
        );
        assert_eq!(
            shell_denylist(posix),
            expected.denylist,
            "the {branch} shell denylist diverges"
        );
        assert_eq!(
            shell_denylist_standalone(posix),
            expected.denylist_standalone,
            "the {branch} standalone denylist diverges"
        );
    }
    assert_eq!(corpus.shell_lists.len(), 2, "both branches are recorded");
}

/// US-103: every reference `(tool, key)` pair has a reader, proven by writing a
/// value under it and finding that value in the typed configuration the tool's
/// handler reads.
#[test]
fn every_reference_configuration_pair_reaches_the_view_its_tool_reads() {
    let corpus = corpus();
    let mut unread = Vec::new();
    let mut pairs = 0;
    for (tool, expected) in &corpus.tools {
        for (key, default) in expected {
            pairs += 1;
            let written = distinct_value(default);
            let resolver = captured_resolver(&corpus);
            resolver.update(Table::from_iter([(
                tool.clone(),
                Value::Table(Table::from_iter([(key.clone(), written.clone())])),
            )]));
            let document = resolver.view_document(tool);
            match document.get(key.as_str()) {
                Some(found) if *found == written => {}
                Some(found) => unread.push(format!(
                    "{tool}.{key} was set to {written} and the view reads {found}"
                )),
                None => unread.push(format!("{tool}.{key} has no reader in its tool's view")),
            }
        }
    }
    assert!(
        unread.is_empty(),
        "these configuration pairs have no live reader: {unread:#?}"
    );
    assert_eq!(pairs, corpus.counts.pairs);
    println!(
        "tool configuration: {pairs}/{} pairs have a live reader",
        corpus.counts.pairs
    );
}

/// A value of the same kind as `default` and different from it, so a reader
/// that ignores the configuration and answers with the default fails the check.
fn distinct_value(default: &JsonValue) -> Value {
    match default {
        JsonValue::Bool(flag) => Value::Boolean(!flag),
        JsonValue::Number(number) if number.is_f64() && number.as_f64() != Some(0.0) => {
            Value::Float(number.as_f64().unwrap_or_default() + 1.0)
        }
        JsonValue::Number(number) => Value::Integer(number.as_i64().unwrap_or_default() + 7),
        JsonValue::String(text) if parse_permission(text).is_some() => {
            // The three permissions are a closed set, so the distinct value is
            // another member rather than a new string.
            Value::String(
                permission_label(match parse_permission(text) {
                    Some(PermissionMode::Always) => PermissionMode::Never,
                    _ => PermissionMode::Always,
                })
                .to_owned(),
            )
        }
        JsonValue::String(text) => Value::String(format!("{text}-configured")),
        JsonValue::Array(_) => Value::Array(vec![Value::String("configured-entry".to_owned())]),
        other => panic!("the corpus carries a default no configuration can hold: {other}"),
    }
}

/// The pinned checkout and an interpreter that can drive it, or `None` when the
/// live probe cannot run here. Every replay above still ran against the
/// committed corpus.
fn pinned_reference() -> Option<(PathBuf, PathBuf)> {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "tool configuration oracle") {
        eprintln!("{reason}");
        return None;
    }
    let interpreter = pinned_interpreter(&root)?;
    Some((root, interpreter))
}

#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some((root, interpreter)) = pinned_reference() else {
        eprintln!("skipping the live tool configuration probe: no pinned checkout to capture from");
        return;
    };
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("defaults.json");
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
