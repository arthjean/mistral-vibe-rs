//! Differential oracle for what the tools actually *do*.
//!
//! The tool-surface oracle proves `grep` publishes the right schema. Nothing
//! proved `grep` returns the right matches, which is the ceiling
//! `docs/parity.md` names for itself. This module closes it: it drives this
//! port's tools over the same fixture tree
//! (`crates/vibe-app-server/tests/tool-execution/tree`) that
//! `scripts/parity/tool_execution.py` drove the reference over, projects both
//! results through the same rules, and diffs them as JSON pointers.
//!
//! The replay is unconditional. The committed corpus carries names, pointers,
//! counts and digests and no reference prose, so CI reports a conformance count
//! instead of skipping for want of a checkout, which is what FR-15 requires.
//!
//! The surviving divergence is held in [`LEDGER`], one entry per known gap with
//! the story that closes it. A divergence outside the ledger fails the suite,
//! and so does a ledger entry whose divergence has been fixed: a ledger that
//! cannot rot is the only kind worth keeping.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use vibe_core::parity::REFERENCE_COMMIT;
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore,
    TrustDecision, TrustRootKind,
};
use vibe_core::tools::builtins::BuiltinTools;
use vibe_core::tools::{ToolError, ToolInvocation, ToolRegistry};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};

/// Grants every approval, so a case measures the tool body rather than the
/// policy in front of it.
///
/// The reference is driven with no permission store at all, which is what makes
/// its capture a measurement of the tool. Refusing here would compare this
/// port's guard against the reference's absent one and call the difference a
/// behavioral gap, which it is not. The permission model has its own epic.
struct GrantApproval;

impl ApprovalAgent for GrantApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

const CORPUS_RELATIVE: &str = "crates/vibe-app-server/tests/tool-execution/corpus.json";
const TREE_RELATIVE: &str = "crates/vibe-app-server/tests/tool-execution/tree";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The case floor this epic commits to, so a regeneration that captured almost
/// nothing fails instead of reporting a clean but empty run.
const MINIMUM_CASES: usize = 40;
/// The prefix a fixture dotfile is stored under, so a fixture `.gitignore`
/// cannot apply to this repository. Mirrors `DOT_PREFIX` in the capture script.
const DOT_PREFIX: &str = "dot-";
const TREE_PLACEHOLDER: &str = "{tree}";

/// The divergences this port still carries, each with the story that closes it.
///
/// A pointer is matched by prefix, so `/typedResult` covers every field under
/// it. Keep the list ordered by tool then by case.
const LEDGER: &[Divergence] = &[
    Divergence {
        tool: "read_file",
        case: "*",
        pointer: "/typedResult",
        story: "US-113",
        why: "the whole result shape diverges: the reference publishes file_path, content, \
               num_lines, start_line, requested_offset, requested_limit, total_lines and \
               was_truncated, this port publishes path, numberedContent, contentBytes, startLine, \
               endLine, totalLines and truncated",
    },
    Divergence {
        tool: "read_file",
        case: "*",
        pointer: "/modelText",
        story: "US-113",
        why: "each tool renders its own model-facing text here, while the reference joins the \
               result fields in the agent loop and appends any subdirectory AGENTS.md",
    },
    Divergence {
        tool: "grep",
        case: "*",
        pointer: "/typedResult",
        story: "US-112",
        why: "this port walks the tree with the regex crate, case-sensitively, against a \
               two-entry hardcoded ignore set, so both the match set and the field names differ \
               until grep moves onto the ripgrep library crates",
    },
    Divergence {
        tool: "grep",
        case: "*",
        pointer: "/modelText",
        story: "US-112",
        why: "follows the result divergence above, plus the same rendering difference",
    },
    Divergence {
        tool: "write_file",
        case: "*",
        pointer: "/typedResult",
        story: "US-114",
        why: "the reference publishes file_path, content and bytes_written; this port publishes \
               path, bytesWritten, filesChanged and a diff",
    },
    Divergence {
        tool: "write_file",
        case: "*",
        pointer: "/modelText",
        story: "US-114",
        why: "follows the result divergence above",
    },
    Divergence {
        tool: "edit",
        case: "*",
        pointer: "/typedResult",
        story: "US-114",
        why: "the reference publishes file, old_string, new_string and a message; this port \
               publishes path, bytesWritten, filesChanged and a diff",
    },
    Divergence {
        tool: "edit",
        case: "*",
        pointer: "/modelText",
        story: "US-114",
        why: "follows the result divergence above",
    },
    // Scoped to the one case, so fixing it retires exactly these two entries
    // rather than hiding behind the tool-wide ones above.
    Divergence {
        tool: "edit",
        case: "identical-strings",
        pointer: "/outcome",
        story: "US-114",
        why: "an edit whose old_string equals its new_string is refused upstream and succeeds \
               here, writing the file back unchanged with an empty diff",
    },
    Divergence {
        tool: "edit",
        case: "identical-strings",
        pointer: "/error",
        story: "US-114",
        why: "the same case: the reference raises where this port returns, so it reports no error \
               at all",
    },
    Divergence {
        tool: "todo",
        case: "*",
        pointer: "/typedResult",
        story: "US-115",
        why: "the reference publishes total_count and a message; this port publishes totalCount \
               and no message",
    },
    Divergence {
        tool: "todo",
        case: "*",
        pointer: "/modelText",
        story: "US-115",
        why: "follows the result divergence above",
    },
];

/// One tolerated gap between this port and the reference.
#[derive(Debug, Clone, Copy)]
struct Divergence {
    tool: &'static str,
    /// `*` tolerates the divergence for every case of the tool.
    case: &'static str,
    /// Matched by prefix against the reported JSON pointer.
    pointer: &'static str,
    story: &'static str,
    /// Why the gap stands, asserted non-empty so an entry cannot be added
    /// without a stated reason.
    why: &'static str,
}

impl Divergence {
    fn covers(&self, tool: &str, case: &str, pointer: &str) -> bool {
        self.tool == tool
            && (self.case == "*" || self.case == case)
            && pointer.starts_with(self.pointer)
    }
}

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
            document.insert("error".to_owned(), error.clone());
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
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

// --------------------------------------------------------------------------
// The fixture tree
// --------------------------------------------------------------------------

/// Copies the checked-in tree, restoring the dotfiles it stores prefixed.
///
/// Mirrors `materialize_tree` in the capture script: both sides must see the
/// same bytes, or the diff measures the fixture rather than the tool.
fn materialize_tree(source: &Path, destination: &Path) {
    let entries = fs::read_dir(source).expect("the fixture tree is readable");
    fs::create_dir_all(destination).expect("the destination is writable");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_str().expect("a fixture name is UTF-8");
        let restored = name
            .strip_prefix(DOT_PREFIX)
            .map_or_else(|| name.to_owned(), |rest| format!(".{rest}"));
        let target = destination.join(&restored);
        if entry.path().is_dir() {
            materialize_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("a fixture copies");
        }
    }
}

// --------------------------------------------------------------------------
// Normalization and projection, mirroring the capture script
// --------------------------------------------------------------------------

fn normalize(value: &Value, tree: &str) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), normalize(item, tree)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|i| normalize(i, tree)).collect()),
        Value::String(text) => {
            let replaced = text.replace(tree, TREE_PLACEHOLDER);
            Value::String(if replaced.contains(TREE_PLACEHOLDER) {
                replaced.replace('\\', "/")
            } else {
                replaced
            })
        }
        other => other.clone(),
    }
}

fn digest(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
        use std::fmt::Write;
        let _ = write!(accumulator, "{byte:02x}");
        accumulator
    });
    format!("sha256:{}", &hex[..32])
}

/// Whether a captured string may be compared as it stands, mirroring
/// `keeps_literal` in the capture script.
fn keeps_literal(text: &str, authored: &BTreeSet<String>) -> bool {
    if authored.contains(text) {
        return true;
    }
    if (text.starts_with(TREE_PLACEHOLDER) || text.starts_with("{scratchpad}"))
        && !text.contains(' ')
    {
        return true;
    }
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && text.len() <= 32
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
        Value::String(text) if !keeps_literal(text, authored) => json!({
            "length": text.chars().count(),
            "digest": digest(text),
        }),
        other => other.clone(),
    }
}

/// Every string the case's own arguments supplied, at any depth.
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

// --------------------------------------------------------------------------
// Driving this port
// --------------------------------------------------------------------------

/// A registry publishing the builtins this oracle drives, rooted at `tree`.
async fn registry_for(tree: &Path, home: &Path) -> ToolRegistry {
    let workspace = Arc::new(Workspace::open(tree).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    let policy = PermissionStore::default();
    policy
        .set_trust(tree, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    // A mutating tool snapshots the file it is about to change, which this port
    // only allows inside a turn. A real session always has one open, so the
    // oracle opens one too: without it every write would report the turn guard
    // instead of the tool body.
    review.begin_turn("turn-1").expect("a turn opens");

    let registry = ToolRegistry::default();
    BuiltinTools::new(home, None)
        .register(
            "session-1",
            tree,
            true,
            &registry,
            policy.clone(),
            Arc::new(GrantApproval),
        )
        .expect("universal tools register");
    WorkspaceTools::new(workspace, review)
        .register(&registry, policy, Arc::new(GrantApproval))
        .expect("workspace tools register");
    registry
}

/// The arguments the reference was given, with the relative paths this corpus
/// stores resolved against the materialized tree.
fn absolute_arguments(arguments: &Value, tree: &Path) -> Value {
    let mut arguments = arguments.clone();
    if let Some(fields) = arguments.as_object_mut() {
        for key in ["file_path", "path"] {
            if let Some(Value::String(relative)) = fields.get(key)
                && !relative.is_empty()
            {
                let joined = tree.join(relative);
                fields.insert(key.to_owned(), Value::String(joined.display().to_string()));
            }
        }
    }
    arguments
}

/// What this port produces for one case, in the corpus document shape.
async fn observe(case: &Case, tree: &Path, home: &Path) -> Value {
    let registry = registry_for(tree, home).await;
    let arguments = absolute_arguments(&case.arguments, tree);
    let outcome = registry
        .invoke(
            &case.tool,
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments,
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

/// The error's kind, named the way the reference names its exception class, so
/// the two sides compare a kind rather than a language's type name.
fn error_type(error: &ToolError) -> &'static str {
    match error {
        ToolError::SchemaViolation { .. } => "ValidationError",
        _ => "ToolError",
    }
}

// --------------------------------------------------------------------------
// Diffing
// --------------------------------------------------------------------------

/// Every difference under `pointer`, as a JSON pointer and both values.
///
/// All of them rather than the first, because the ledger is matched per
/// pointer: reporting only the earliest would leave an entry for a later field
/// permanently unexercised, and the staleness check would then delete a gap
/// that is still open.
fn differences(pointer: &str, expected: &Value, actual: &Value, into: &mut Vec<Difference>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                let nested = format!("{pointer}/{key}");
                match right.get(key) {
                    Some(other) => differences(&nested, value, other, into),
                    None => into.push(Difference {
                        pointer: nested,
                        expected: value.clone(),
                        actual: Value::String("absent".to_owned()),
                    }),
                }
            }
            for key in right.keys().filter(|key| !left.contains_key(*key)) {
                into.push(Difference {
                    pointer: format!("{pointer}/{key}"),
                    expected: Value::String("absent".to_owned()),
                    actual: right[key].clone(),
                });
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (a, b)) in left.iter().zip(right).enumerate() {
                differences(&format!("{pointer}/{index}"), a, b, into);
            }
        }
        _ if expected == actual => {}
        _ => into.push(Difference {
            pointer: pointer.to_owned(),
            expected: expected.clone(),
            actual: actual.clone(),
        }),
    }
}

#[derive(Debug)]
struct Difference {
    pointer: String,
    expected: Value,
    actual: Value,
}

/// What this port produces for one case, projected the way the corpus is.
async fn observed_document(case: &Case, source: &Path, scratch: &Path, index: usize) -> Value {
    let tree = scratch.join(format!("case-{index}"));
    materialize_tree(source, &tree);
    let tree = tree.canonicalize().expect("the tree resolves");
    let home = scratch.join("home");
    fs::create_dir_all(&home).expect("home");

    let observed = observe(case, &tree, &home).await;
    let mut authored = BTreeSet::new();
    authored_values(&case.arguments, &mut authored);
    project(
        &normalize(&observed, &tree.display().to_string()),
        &authored,
    )
}

#[tokio::test]
async fn tool_execution_matches_the_reference_except_for_the_recorded_gap() {
    let corpus = corpus();
    assert!(
        corpus.cases.len() >= MINIMUM_CASES,
        "the corpus shrank to {} cases",
        corpus.cases.len()
    );
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }

    let root = repo_root();
    let source = root.join(TREE_RELATIVE);
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut conforming = 0;
    let mut tolerated: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unlisted = Vec::new();

    for (index, case) in corpus.cases.iter().enumerate() {
        // A fresh tree per case, so a mutating tool cannot leak into the next
        // one. The capture script isolates the same way.
        let observed = observed_document(case, &source, scratch.path(), index).await;

        let mut found = Vec::new();
        differences("", &case.document(), &observed, &mut found);
        if found.is_empty() {
            conforming += 1;
        }
        for difference in found {
            match LEDGER
                .iter()
                .find(|entry| entry.covers(&case.tool, &case.case, &difference.pointer))
            {
                Some(entry) => {
                    tolerated.insert((
                        format!("{}:{}", entry.tool, entry.pointer),
                        entry.story.to_owned(),
                    ));
                }
                None => unlisted.push(format!(
                    "{} at {}: the reference says {}, this port says {}",
                    case.id(),
                    difference.pointer,
                    difference.expected,
                    difference.actual
                )),
            }
        }
    }

    println!(
        "tool execution: {conforming}/{} cases match the reference at {}, {} ledger entries \
         exercised",
        corpus.cases.len(),
        &corpus.reference_commit[..12],
        tolerated.len()
    );
    for (entry, story) in &tolerated {
        println!("  tolerated {entry} until {story}");
    }

    assert!(
        unlisted.is_empty(),
        "execution divergences outside the ledger:\n  {}",
        unlisted.join("\n  ")
    );
}

#[tokio::test]
async fn a_ledger_entry_whose_divergence_is_fixed_fails_the_suite() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let root = repo_root();
    let source = root.join(TREE_RELATIVE);
    let scratch = tempfile::tempdir().expect("tempdir");

    // Which ledger entries an actual divergence still reaches.
    let mut exercised: BTreeSet<usize> = BTreeSet::new();
    for (index, case) in corpus.cases.iter().enumerate() {
        let observed = observed_document(case, &source, scratch.path(), index).await;
        let mut found = Vec::new();
        differences("", &case.document(), &observed, &mut found);
        for difference in found {
            if let Some(position) = LEDGER
                .iter()
                .position(|entry| entry.covers(&case.tool, &case.case, &difference.pointer))
            {
                exercised.insert(position);
            }
        }
    }

    let stale = LEDGER
        .iter()
        .enumerate()
        .filter(|(position, _)| !exercised.contains(position))
        .map(|(_, entry)| {
            format!(
                "{}/{} at {} ({})",
                entry.tool, entry.case, entry.pointer, entry.story
            )
        })
        .collect::<Vec<_>>();

    assert!(
        stale.is_empty(),
        "these ledger entries no longer describe a divergence and must be removed, which is what \
         keeps the ledger from rotting:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let mut offending = Vec::new();
    for case in &corpus.cases {
        let mut authored = BTreeSet::new();
        authored_values(&case.arguments, &mut authored);
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
            // A digested string is recorded as exactly these two keys.
            if fields.len() == 2 && fields.contains_key("digest") && fields.contains_key("length") {
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
    // with `hashlib.sha256(text.encode()).hexdigest()[:32]`.
    assert_eq!(
        digest("alpha one"),
        "sha256:447ddb49ae0e88206741f4e0d10b1371"
    );

    let authored = BTreeSet::from(["gather".to_owned()]);
    assert!(keeps_literal("gather", &authored));
    assert!(keeps_literal("{tree}/alpha.txt", &authored));
    assert!(keeps_literal("ToolError", &authored));
    assert!(keeps_literal("in_progress", &authored));
    assert!(!keeps_literal(
        "File not found at: {tree}/nowhere.txt",
        &authored
    ));
    assert!(!keeps_literal("Updated 2 todos", &authored));
    assert!(!keeps_literal("", &authored));
}

#[test]
fn every_ledger_entry_names_a_story_that_closes_it() {
    for entry in LEDGER {
        assert!(
            entry.story.starts_with("US-"),
            "a tolerated divergence names the story that closes it, not {}",
            entry.story
        );
        assert!(entry.pointer.starts_with('/'), "{}", entry.pointer);
        assert!(!entry.why.is_empty(), "{}/{}", entry.tool, entry.case);
    }
}

#[test]
fn the_fixture_tree_is_committed_and_deterministic() {
    let source = repo_root().join(TREE_RELATIVE);
    let scratch = tempfile::tempdir().expect("tempdir");
    let first = scratch.path().join("first");
    let second = scratch.path().join("second");
    materialize_tree(&source, &first);
    materialize_tree(&source, &second);

    let listing = |root: &Path| -> BTreeMap<String, Vec<u8>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path
                        .strip_prefix(root)
                        .expect("under the root")
                        .display()
                        .to_string();
                    found.insert(relative, fs::read(&path).expect("readable"));
                }
            }
        }
        found
    };

    let first = listing(&first);
    assert_eq!(
        first,
        listing(&second),
        "materialization is not deterministic"
    );
    assert!(
        first.contains_key(".gitignore"),
        "the dot-prefixed fixture is restored: {:?}",
        first.keys().collect::<Vec<_>>()
    );
    assert!(first.contains_key("alpha.txt"));
}
