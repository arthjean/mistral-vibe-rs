//! Differential oracle for the published tool surface.
//!
//! The Python reference is the authority on which tools exist and what
//! `tools[].function.parameters` each one publishes. This module captures that
//! surface from the pinned checkout and diffs it against the definitions a real
//! session registers, reporting missing names, invented names and per-name
//! schema divergence as JSON pointers.
//!
//! Two rules keep the comparison honest. Description *text* is never compared,
//! only its presence: `NOTICE` forbids shipping reference prose, so the Rust
//! descriptions are original and are held to directive coverage elsewhere.
//! And the corpus itself is a gitignored local artifact, because it holds that
//! prose verbatim.
//!
//! The surviving divergence is checked against a committed baseline, so the
//! remaining gap is explicit and any *new* divergence fails the suite while the
//! later epics close the known one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore,
    TrustDecision, TrustRootKind,
};
use vibe_core::tools::{ToolError, ToolRegistry, ToolSpec, validate_arguments};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};

use crate::client::InteractiveSessionToolFactory;
use crate::server::SessionToolFactory;

/// The reference commit the corpus is captured from. A checkout at any other
/// revision is not an oracle for this corpus.
const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";
const REFERENCE_ROOT: &str = "/home/arthur/dev/mistral-vibe";
const CORPUS_RELATIVE: &str = ".parity/tool-surface-corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_surface.py";
const BASELINE_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/baseline.json";
/// Stands in for every description string so presence is compared and text is
/// not, keeping the diff inside what `NOTICE` allows.
const DESCRIBED: &str = "<described>";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    platform: String,
    tools: Vec<ReferenceTool>,
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
struct ReferenceTool {
    name: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    tool: String,
    case: String,
    arguments: Value,
    accepted: bool,
}

/// The divergence the port still carries, shrunk by the epics that follow.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Baseline {
    reference_commit: String,
    platform: String,
    note: String,
    missing_names: BTreeSet<String>,
    extra_names: BTreeSet<String>,
    schema_divergence: BTreeMap<String, BTreeSet<String>>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// The pinned checkout, or `None` when this machine cannot act as the oracle.
fn pinned_reference() -> Option<PathBuf> {
    let root = PathBuf::from(REFERENCE_ROOT);
    if !root.join(".venv/bin/python").is_file() {
        return None;
    }
    let revision = Command::new("git")
        .args(["-C", REFERENCE_ROOT, "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !revision.status.success() {
        return None;
    }
    let head = String::from_utf8(revision.stdout).ok()?;
    if head.trim() != REFERENCE_COMMIT {
        eprintln!(
            "the tool-surface oracle needs {REFERENCE_ROOT} at {REFERENCE_COMMIT}, found {}",
            head.trim()
        );
        return None;
    }
    Some(root)
}

/// Recaptures the corpus when the pinned checkout is present, otherwise falls
/// back to a corpus captured earlier from that same commit.
fn corpus() -> Option<Corpus> {
    let root = repo_root();
    let path = root.join(CORPUS_RELATIVE);
    if let Some(reference) = pinned_reference() {
        let capture = Command::new(reference.join(".venv/bin/python"))
            .arg(root.join(CAPTURE_SCRIPT))
            .args(["--reference", REFERENCE_ROOT])
            .arg("--output")
            .arg(&path)
            .current_dir(&root)
            .output()
            .expect("the capture script runs");
        assert!(
            capture.status.success(),
            "tool-surface capture failed: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
    }
    let Ok(raw) = fs::read_to_string(&path) else {
        eprintln!(
            "skipping the tool-surface oracle: no corpus at {} and no pinned checkout at \
             {REFERENCE_ROOT} on {REFERENCE_COMMIT}",
            path.display()
        );
        return None;
    };
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    assert_eq!(corpus.schema_version, 1, "unknown corpus schema version");
    if let Some(reason) = skip_reason(
        &corpus.reference.commit,
        &corpus.platform,
        running_platform(),
    ) {
        eprintln!("{reason}");
        return None;
    }
    Some(corpus)
}

fn running_platform() -> &'static str {
    std::env::consts::OS
}

/// Why a corpus cannot answer for this run, or `None` when it can.
///
/// A corpus is only an oracle for the commit it was captured from and for the
/// platform whose availability rules it recorded, so both mismatches skip with
/// an explicit message rather than failing or passing silently.
fn skip_reason(captured_commit: &str, captured_platform: &str, running: &str) -> Option<String> {
    if captured_commit != REFERENCE_COMMIT {
        return Some(format!(
            "skipping the tool-surface oracle: the corpus was captured from {captured_commit}, \
             not the pinned {REFERENCE_COMMIT}"
        ));
    }
    if captured_platform != running {
        return Some(format!(
            "skipping the tool-surface oracle: the corpus records the {captured_platform} \
             surface and this host is {running}"
        ));
    }
    None
}

struct RejectApproval;

impl ApprovalAgent for RejectApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

/// The tool definitions a real interactive session publishes: the workspace
/// tools registered by the server plus the interactive tools its surface
/// extension adds.
async fn published_specs() -> Vec<ToolSpec> {
    let directory = tempfile::tempdir().expect("tempdir");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    let policy = PermissionStore::default();
    policy
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review)
        .register(&registry, policy, Arc::new(RejectApproval))
        .expect("workspace tools register");
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    InteractiveSessionToolFactory {
        sender,
        plan_directory: Some(directory.path().to_path_buf()),
    }
    .register("session-1", &registry)
    .expect("interactive tools register");
    registry.list().expect("list")
}

/// Replaces every description with a sentinel so the diff reports a missing
/// description without ever comparing reference prose.
fn canonicalize(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if key == "description" && value.is_string() {
                        (key.clone(), Value::String(DESCRIBED.to_owned()))
                    } else {
                        (key.clone(), canonicalize(value))
                    }
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[derive(Debug)]
struct Divergence {
    pointer: String,
    expected: String,
    actual: String,
}

fn diff(expected: &Value, actual: &Value, pointer: &str, found: &mut Vec<Divergence>) {
    match (expected, actual) {
        (Value::Object(expected_object), Value::Object(actual_object)) => {
            for key in expected_object
                .keys()
                .chain(actual_object.keys())
                .collect::<BTreeSet<_>>()
            {
                match (expected_object.get(key), actual_object.get(key)) {
                    (Some(left), Some(right)) => {
                        diff(left, right, &format!("{pointer}/{key}"), found);
                    }
                    (left, right) => found.push(Divergence {
                        pointer: format!("{pointer}/{key}"),
                        expected: render(left),
                        actual: render(right),
                    }),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                diff(left, right, &format!("{pointer}/{index}"), found);
            }
        }
        (left, right) if left != right => found.push(Divergence {
            pointer: pointer.to_owned(),
            expected: render(Some(left)),
            actual: render(Some(right)),
        }),
        _ => {}
    }
}

fn render(value: Option<&Value>) -> String {
    value.map_or_else(|| "<absent>".to_owned(), ToString::to_string)
}

fn baseline() -> Baseline {
    let raw = fs::read_to_string(repo_root().join(BASELINE_RELATIVE)).expect("baseline");
    serde_json::from_str(&raw).expect("the baseline parses")
}

#[tokio::test]
async fn the_published_tool_surface_matches_the_reference_except_for_the_recorded_gap() {
    let Some(corpus) = corpus() else {
        return;
    };
    let published = published_specs().await;

    let reference_names = corpus
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let published_names = published
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<BTreeSet<_>>();
    let missing = reference_names
        .difference(&published_names)
        .cloned()
        .collect::<BTreeSet<_>>();
    let extra = published_names
        .difference(&reference_names)
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut schema_divergence: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut report = Vec::new();
    let mut conformant_schemas = 0;
    for tool in &corpus.tools {
        let Some(spec) = published.iter().find(|spec| spec.name == tool.name) else {
            continue;
        };
        let mut found = Vec::new();
        diff(
            &canonicalize(&tool.parameters),
            &canonicalize(&spec.input_schema),
            "",
            &mut found,
        );
        if found.is_empty() {
            conformant_schemas += 1;
            continue;
        }
        for divergence in &found {
            report.push(format!(
                "tool `{}` diverges at {}: expected {}, got {}",
                tool.name, divergence.pointer, divergence.expected, divergence.actual
            ));
        }
        schema_divergence.insert(
            tool.name.clone(),
            found
                .into_iter()
                .map(|divergence| divergence.pointer)
                .collect(),
        );
    }

    let matched_names = reference_names.len() - missing.len();
    println!(
        "tool surface: {matched_names}/{} names, {conformant_schemas}/{} schemas",
        reference_names.len(),
        reference_names.len()
    );
    for line in &report {
        println!("{line}");
    }
    if !extra.is_empty() {
        println!(
            "invented names: {}",
            extra.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let baseline = baseline();
    assert_eq!(
        baseline.reference_commit, REFERENCE_COMMIT,
        "the baseline records another reference commit"
    );
    assert_eq!(baseline.platform, corpus.platform, "baseline platform");
    assert_eq!(
        (missing, extra, schema_divergence),
        (
            baseline.missing_names,
            baseline.extra_names,
            baseline.schema_divergence
        ),
        "the tool surface moved away from the recorded gap; regenerate {BASELINE_RELATIVE} \
         only when the change is the intended one"
    );
}

#[tokio::test]
async fn arguments_the_reference_rejects_are_rejected_here_too() {
    let Some(corpus) = corpus() else {
        return;
    };
    let schemas = corpus
        .tools
        .iter()
        .map(|tool| (tool.name.as_str(), &tool.parameters))
        .collect::<BTreeMap<_, _>>();

    let mut checked = 0;
    let mut wrongly_accepted = Vec::new();
    let mut stricter_than_pydantic = Vec::new();
    for fixture in &corpus.fixtures {
        let Some(schema) = schemas.get(fixture.tool.as_str()) else {
            continue;
        };
        checked += 1;
        let verdict: Result<(), ToolError> = validate_arguments(&fixture.arguments, schema);
        match (fixture.accepted, verdict) {
            (false, Ok(())) => wrongly_accepted.push(format!(
                "{}/{}: {}",
                fixture.tool, fixture.case, fixture.arguments
            )),
            (true, Err(error)) => {
                stricter_than_pydantic.push(format!("{}/{}: {error}", fixture.tool, fixture.case));
            }
            _ => {}
        }
    }
    println!(
        "argument fixtures: {checked} replayed, {} accepted here and rejected there, {} rejected \
         here and accepted there",
        wrongly_accepted.len(),
        stricter_than_pydantic.len()
    );
    for line in &stricter_than_pydantic {
        println!("stricter than the reference: {line}");
    }
    assert!(
        wrongly_accepted.is_empty(),
        "arguments the reference rejects were accepted here: {}",
        wrongly_accepted.join("; ")
    );
    assert!(checked > 0, "the corpus carries no argument fixtures");
}

#[test]
fn a_corpus_from_another_commit_or_platform_skips_with_an_explicit_message() {
    let moved = skip_reason("0123456789abcdef0123456789abcdef01234567", "linux", "linux")
        .expect("a corpus from another commit cannot answer");
    assert!(moved.contains(REFERENCE_COMMIT), "{moved}");
    assert!(moved.contains("0123456789abcdef"), "{moved}");

    let elsewhere = skip_reason(REFERENCE_COMMIT, "windows", "linux")
        .expect("a corpus from another platform cannot answer");
    assert!(
        elsewhere.contains("windows") && elsewhere.contains("linux"),
        "{elsewhere}"
    );

    assert!(skip_reason(REFERENCE_COMMIT, running_platform(), running_platform()).is_none());
}

#[test]
fn the_corpus_stays_out_of_the_repository_and_the_baseline_carries_no_prose() {
    let ignored = Command::new("git")
        .args(["check-ignore", CORPUS_RELATIVE])
        .current_dir(repo_root())
        .output()
        .expect("git check-ignore runs");
    assert!(
        ignored.status.success(),
        "{CORPUS_RELATIVE} must stay out of the repository: it holds reference description text"
    );
    for (tool, pointers) in baseline().schema_divergence {
        for pointer in pointers {
            assert!(
                pointer.starts_with('/'),
                "the baseline records JSON pointers only, `{tool}` carries `{pointer}`"
            );
        }
    }
}
