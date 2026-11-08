//! Differential oracle for what the tools actually *do*.
//!
//! The tool-surface oracle proves a tool publishes the right schema. Nothing
//! there proves the tool answers the right result, which is the ceiling
//! `docs/parity.md` names for itself. This module closes it: it drives this
//! port's tools with the same arguments `scripts/parity/tool_execution.py`
//! drove the reference with, projects both results through the same rules, and
//! diffs them as JSON pointers.
//!
//! Since v3.0 the reference executes one tool inside its `vibe` package, so one
//! is driven here: `todo`, which the reference's app server provides to the
//! harness (`vibe/app_server/_provided_tools.py`). The other ten tools the v2
//! corpus measured now execute in the reference's harness runtime or were
//! deleted; the capture script's docstring says where each went. `todo` goes
//! through [`BuiltinTools`], the registration path a session uses.
//!
//! The replay is unconditional. The committed corpus carries names, counts and
//! digests and no reference prose, so CI reports a conformance count instead of
//! skipping for want of a checkout, which is what FR-15 requires. The one test
//! that does need the checkout is the live probe at the bottom, which
//! recaptures and asserts the committed corpus is still what the pinned
//! reference answers.
//!
//! The comparison runs through a [`Tally`]: every difference has to fall under
//! a typed entry of [`LEDGER`], every entry has to reproduce exactly the
//! differences it declares, and the counts have to equal this replay's line of
//! `docs/parity.md` "Measured figures".

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use vibe_core::parity::is_pin_commit;
use vibe_core::parity::replay::{Class, PatternDivergence, Tally, differences, entries};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore, ToolGuard,
};
use vibe_core::skills::SkillDiscovery;
use vibe_core::tools::builtins::BuiltinTools;
use vibe_core::tools::{ToolError, ToolInvocation, ToolRegistry};

/// Grants every approval, so a case measures the tool body rather than the
/// policy in front of it.
///
/// The reference executor is driven with no permission resolver in front of
/// it, which is what makes its capture a measurement of the tool. Refusing here
/// would compare this port's guard against the reference's absent one and call
/// the difference a behavioral gap, which it is not. The permission model has
/// its own epic.
struct GrantApproval;

impl ApprovalAgent for GrantApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

const CORPUS_RELATIVE: &str = "crates/vibe-app-server/tests/tool-execution/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_execution.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 3;
/// The case floor, so a regeneration that captured almost nothing fails
/// instead of reporting a clean but empty run. NFR-1. It is every case the
/// reference's `vibe` package can still answer at v3.0.
const MINIMUM_CASES: usize = 4;
/// The per-tool floor, so a corpus cannot reach the total above by driving one
/// tool many times while another goes unmeasured. NFR-1.
const MINIMUM_CASES_PER_TOOL: usize = 4;
/// Every tool whose execution the reference still runs in its `vibe` package,
/// so dropping one is a named failure rather than a smaller green run. NFR-1.
const MINIMUM_TOOLS: usize = 1;

/// Why the port's model text for a settled `todo` call differs.
const TODO_MODEL_TEXT: &str = "since v3.0 the reference hands the model the provided tool's \
     structured result as one JSON object (`vibe/app_server/_provided_tools.py`); this port still \
     renders one `field: value` line per field, the v2 legacy loop's form";

/// The differences this port keeps. A gap of row 3 is admitted only while the
/// corpus sits at the target of an open re-pin; the replay fails once the
/// transition closes with one still listed.
const LEDGER: &[PatternDivergence] = &[PatternDivergence {
    scenario: "todo/*",
    pointer: "/modelText",
    count: 3,
    row: "3",
    class: Class::Gap,
    reason: TODO_MODEL_TEXT,
}];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference_commit: String,
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    tool: String,
    case: String,
    arguments: Value,
    outcome: String,
    #[serde(default)]
    typed_result: Option<Value>,
    #[serde(default)]
    model_text: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

impl Case {
    fn id(&self) -> String {
        format!("{}/{}", self.tool, self.case)
    }

    /// The corpus entry as one comparable document, so a missing field on one
    /// side is a pointer difference rather than a special case.
    fn document(&self) -> Value {
        let mut document = Map::new();
        document.insert("outcome".to_owned(), Value::String(self.outcome.clone()));
        if let Some(typed) = &self.typed_result {
            document.insert("typedResult".to_owned(), typed.clone());
        }
        if let Some(text) = &self.model_text {
            document.insert("modelText".to_owned(), text.clone());
        }
        if let Some(error) = &self.error {
            // The corpus records the message digest so a re-pin that reworded a
            // refusal is visible in its diff. It is not a conformance target:
            // the PRD lists byte-identical error text as a non-goal, because
            // reaching a reference digest means writing the reference's own
            // sentence. Only the presence and the kind are compared.
            let mut error = error.clone();
            if let Some(message) = error.pointer_mut("/message").and_then(Value::as_object_mut) {
                message.remove("sha256");
            }
            document.insert("error".to_owned(), error);
        }
        Value::Object(document)
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Why this corpus cannot answer for this host, or [`None`] when it can.
///
/// A corpus records what the reference did on one platform, so replaying a
/// Linux capture on a Windows workstation would diff the host rather than the
/// port. That skips with a named reason rather than failing, which is the rule
/// `skip_reason_for` in the tool-surface oracle already states: a corpus that
/// cannot answer must neither fail nor pass silently. Both tests below consult
/// this, so they cannot drift into disagreeing about the same mismatch.
fn skip_reason(corpus: &Corpus) -> Option<String> {
    (corpus.platform != std::env::consts::OS).then(|| {
        format!(
            "skipping the tool-execution replay: the corpus records the {} behavior and this host \
             is {}; recapture with `scripts/parity/tool_execution.py --corpus`",
            corpus.platform,
            std::env::consts::OS
        )
    })
}

fn corpus() -> Corpus {
    let raw =
        fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `scripts/parity/tool_execution.py --corpus`"
    );
    assert!(
        is_pin_commit(&corpus.reference_commit),
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

/// Fails when the corpus no longer covers what NFR-1 commits to, naming the
/// count so a shrunken corpus cannot pass as a green one.
fn assert_corpus_floor(corpus: &Corpus) {
    assert!(
        corpus.cases.len() >= MINIMUM_CASES,
        "the corpus shrank to {} cases, below the floor of {MINIMUM_CASES}",
        corpus.cases.len()
    );
    let mut per_tool: BTreeMap<&str, usize> = BTreeMap::new();
    for case in &corpus.cases {
        *per_tool.entry(case.tool.as_str()).or_default() += 1;
    }
    assert!(
        per_tool.len() >= MINIMUM_TOOLS,
        "the corpus drives {} tools, below the floor of {MINIMUM_TOOLS}: {:?}",
        per_tool.len(),
        per_tool.keys().collect::<Vec<_>>()
    );
    let thin = per_tool
        .iter()
        .filter(|(_, count)| **count < MINIMUM_CASES_PER_TOOL)
        .map(|(tool, count)| format!("{tool} has {count}"))
        .collect::<Vec<_>>();
    assert!(
        thin.is_empty(),
        "every tool carries at least {MINIMUM_CASES_PER_TOOL} cases, but {}",
        thin.join(", ")
    );
}

// --------------------------------------------------------------------------
// Projection, mirroring the capture script
// --------------------------------------------------------------------------

/// Prose as the corpus records it: its length in code points and its
/// SHA-256. Mirrors `digest` in the capture script.
fn digest(text: &str) -> Value {
    json!({
        "prose": text.chars().count(),
        "sha256": vibe_core::text::sha256_hex(text.as_bytes()),
    })
}

/// Whether a captured string may be compared as it stands, mirroring
/// `keeps_literal` in the capture script.
fn keeps_literal(text: &str, authored: &BTreeSet<String>) -> bool {
    if authored.contains(text) {
        return true;
    }
    // An identifier carries no sentence. Mirrors `_IDENTIFIER`.
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && text.len() <= 32
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn project(value: &Value, authored: &BTreeSet<String>) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), project(item, authored)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|i| project(i, authored)).collect()),
        Value::String(text) if !keeps_literal(text, authored) => digest(text),
        other => other.clone(),
    }
}

/// Every string the case supplied, at any depth.
fn authored_values(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => fields.values().for_each(|item| authored_values(item, into)),
        Value::Array(items) => items.iter().for_each(|item| authored_values(item, into)),
        Value::String(text) => {
            into.insert(text.clone());
        }
        _ => {}
    }
}

/// The vocabulary one case authored: its arguments. Mirrors the seeding in
/// `project_case`.
fn authored_vocabulary(case: &Case) -> BTreeSet<String> {
    let mut authored = BTreeSet::new();
    authored_values(&case.arguments, &mut authored);
    authored
}

// --------------------------------------------------------------------------
// Driving this port
// --------------------------------------------------------------------------

/// A registry publishing the universal tools for one fresh session, so one
/// case can never read the list another case wrote. The capture script builds
/// a fresh executor per case the same way.
fn registry_for(home: &Path) -> ToolRegistry {
    let registry = ToolRegistry::default();
    let guard = ToolGuard::new(PermissionStore::default(), Arc::new(GrantApproval));
    BuiltinTools::new(home, None)
        .register("session-1", SkillDiscovery::default(), &registry, &guard)
        .expect("universal tools register");
    registry
}

/// What this port produces for one case, in the corpus document shape.
async fn observe(case: &Case, home: &Path) -> Value {
    let registry = registry_for(home);
    let outcome = registry
        .invoke(
            &case.tool,
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: case.arguments.clone(),
            },
        )
        .await;

    let mut document = Map::new();
    match outcome {
        Ok(output) => {
            document.insert("outcome".to_owned(), Value::String("returned".to_owned()));
            document.insert("typedResult".to_owned(), output.typed_result);
            document.insert("modelText".to_owned(), Value::String(output.model_text));
        }
        Err(error) => {
            document.insert("outcome".to_owned(), Value::String("raised".to_owned()));
            // The message is compared by presence, never by content: the PRD
            // lists byte-identical error text as a non-goal, so a message must
            // name the same cause rather than reproduce the reference wording.
            document.insert(
                "error".to_owned(),
                json!({
                    "type": error_type(&error),
                    "message": { "present": !error.to_string().is_empty() },
                }),
            );
        }
    }
    Value::Object(document)
}

/// The error's kind, named the way the reference's executor reports it, so the
/// two sides compare a kind rather than a language's type name.
///
/// Since v3.0 a provided tool's failure leaves the reference as a protocol
/// error whose code is `tool_failed` (`vibe/app_server/_provided_tools.py`),
/// whatever exception raised it. A schema violation never reaches that
/// executor: the reference's harness core rejects it first, so it keeps a
/// label of its own here and cannot pass for a tool failure.
fn error_type(error: &ToolError) -> &'static str {
    match error {
        ToolError::Approved { source, .. } => error_type(source),
        ToolError::SchemaViolation { .. } => "ValidationError",
        _ => "tool_failed",
    }
}

// --------------------------------------------------------------------------
// Diffing
// --------------------------------------------------------------------------

/// Every JSON pointer at which the two documents differ, a prose digest
/// counting as one value.
///
/// When the two sides disagree on the outcome itself, only that is reported:
/// every field below it is a consequence of returning where the reference
/// raised, and reporting them as well would turn one gap into several ledger
/// entries that all close together.
fn compare(expected: &Value, actual: &Value) -> Vec<String> {
    if expected.get("outcome") != actual.get("outcome") {
        return vec!["/outcome".to_owned()];
    }
    differences(expected, actual)
}

/// What this port produces for one case, projected the way the corpus is.
async fn observed_document(case: &Case, scratch: &Path, index: usize) -> Value {
    let home = scratch.join(format!("home-{index}"));
    fs::create_dir_all(&home).expect("home");
    project(&observe(case, &home).await, &authored_vocabulary(case))
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[tokio::test]
async fn tool_execution_matches_the_reference_except_for_the_recorded_gap() {
    let text =
        fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed");
    let corpus = corpus();
    assert_corpus_floor(&corpus);
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let mut tally = Tally::new(&text, entries(LEDGER));
    for (index, case) in corpus.cases.iter().enumerate() {
        // A fresh session per case, so one case cannot read another's state.
        // The capture script isolates the same way.
        let observed = observed_document(case, scratch.path(), index).await;
        let expected = case.document();
        let found = compare(&expected, &observed);
        tally.record(&case.id(), &expected, &observed, found);
    }
    tally.conclude("tool execution parity:");
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let mut offending = Vec::new();
    for case in &corpus.cases {
        let authored = authored_vocabulary(case);
        collect_literals(&case.document(), &authored, &mut offending);
    }
    assert!(
        offending.is_empty(),
        "the corpus carries strings that are neither an argument it authored, a normalized path, \
         nor an identifier, so they may be reference prose: {offending:?}"
    );
}

/// Every committed string that `keeps_literal` would not have admitted, which
/// is what a prose leak would look like.
fn collect_literals(value: &Value, authored: &BTreeSet<String>, into: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            if vibe_core::parity::replay::is_digest(value) {
                return;
            }
            fields
                .values()
                .for_each(|item| collect_literals(item, authored, into));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_literals(item, authored, into)),
        Value::String(text) if !keeps_literal(text, authored) => into.push(text.clone()),
        _ => {}
    }
}

#[test]
fn the_projection_agrees_with_the_capture_script() {
    // The two sides must digest identically, or every projected string would
    // read as a divergence. Anchored on a value computed by the Python side
    // with `hashlib.sha256(text.encode()).hexdigest()`.
    assert_eq!(
        digest("alpha one"),
        json!({
            "prose": 9,
            "sha256": "447ddb49ae0e88206741f4e0d10b13711675bb523d438de9c21de83c81a3fff4"
        })
    );

    let authored = BTreeSet::from(["first task".to_owned()]);
    assert!(keeps_literal("first task", &authored));
    assert!(keeps_literal("tool_failed", &authored));
    assert!(keeps_literal("in_progress", &authored));
    assert!(!keeps_literal("Updated 2 todos", &authored));
    assert!(!keeps_literal("second task", &authored));
    assert!(!keeps_literal("", &authored));
}

/// Recaptures against the local checkout and asserts the committed corpus is
/// still what the pinned reference answers.
///
/// This is the only test here that needs the checkout, and it skips naming the
/// pin and the way back when the checkout is absent or off-pin. The replay
/// above runs regardless, which is what keeps a missing checkout from failing
/// `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some(root) = vibe_core::parity::replay::pinned_reference("tool execution") else {
        return;
    };
    let repository = repo_root();
    let recaptured = repository.join("target/tool-execution-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/tool-execution-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the tool-execution capture script runs");
    assert!(
        output.status.success(),
        "the tool-execution capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh: Value = serde_json::from_str(
        &fs::read_to_string(&recaptured).expect("the recaptured corpus is readable"),
    )
    .expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(
        &fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable"),
    )
    .expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}
