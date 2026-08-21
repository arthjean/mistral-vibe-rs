//! Differential oracle for the permission vocabulary.
//!
//! The Python reference is the authority on which scopes exist, which fields a
//! requirement carries, what the arity table holds, and what
//! `build_session_pattern` and `wildcard_match` answer, and which matcher
//! reads a sensitive pattern.
//! `scripts/parity/permission_surface.py` asks it all six questions and
//! records the answers; this module replays them against
//! [`PermissionScope`], [`PermissionRequirement`], [`arity::ARITY`],
//! [`arity::build_session_pattern`], [`wildcard_match`] and the two path
//! matchers [`resolve_file_tool_permission`] reads its lists with.
//!
//! The corpus is committed: it carries enum values, field names, command names,
//! integers and the answers to cases this repository authored, all of which are
//! observations, and no reference-authored description text, which is what
//! `NOTICE` forbids shipping. Replay therefore runs unconditionally; only the
//! live probe that recaptures from the pinned checkout skips when it is absent.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

use super::*;
use crate::parity::{off_pin_reason, pinned_interpreter, reference_root};
use crate::tools::config::ToolConfigResolver;

const CAPTURE_SCRIPT: &str = "scripts/parity/permission_surface.py";
const CORPUS_RELATIVE: &str = "tests/permission-surface/vocabulary.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    counts: Counts,
    scopes: Vec<String>,
    requirement: RequirementModel,
    arity: BTreeMap<String, usize>,
    session_patterns: Vec<SessionPatternCase>,
    wildcard_matches: Vec<WildcardCase>,
    sensitive_matches: Vec<SensitiveCase>,
    sensitive_chain: Vec<SensitiveChainCase>,
    file_tool_chain: FileToolChain,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Counts {
    scopes: usize,
    arity_entries: usize,
    session_pattern_cases: usize,
    wildcard_cases: usize,
    sensitive_cases: usize,
    sensitive_chain_cases: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequirementModel {
    fields: Vec<RequirementField>,
    forbids_extra: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementField {
    #[expect(dead_code, reason = "the alias is what crosses the wire")]
    name: String,
    alias: String,
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionPatternCase {
    tokens: Vec<String>,
    pattern: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WildcardCase {
    text: String,
    pattern: String,
    matches: bool,
}

/// What each matcher answers for one pattern and one path.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SensitiveCase {
    pattern: String,
    path: String,
    /// `None` when the reference refused to read the pattern at all.
    sensitive_matches: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "the refusal is named by the null verdict")]
    sensitive_raises: Option<String>,
    list_matches: bool,
}

/// What the whole file-tool chain answers for one sensitive pattern.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SensitiveChainCase {
    pattern: String,
    path: String,
    permission: Option<String>,
    scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileToolChain {
    sensitive_scope: String,
    sensitive_invocation_pattern: String,
    sensitive_session_pattern: String,
    sensitive_label: String,
    outside_scope: String,
    outside_invocation_pattern: String,
    outside_session_pattern: String,
    outside_label: String,
    permission: String,
}

/// The value `scope` crosses the wire as.
fn wire_scope(scope: PermissionScope) -> String {
    serde_json::to_value(scope)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .expect("a scope serializes as a string")
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let path = corpus_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("permission surface corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("permission surface corpus at {}: {error}", path.display()));
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; update this runner with the capture script"
    );
    assert_eq!(
        corpus.reference.commit,
        crate::parity::REFERENCE_COMMIT,
        "the corpus was captured from another commit than the pin"
    );
    corpus
}

/// US-105: the four scopes, and nothing else.
#[test]
fn the_scope_vocabulary_is_the_reference_one() {
    let corpus = corpus();
    // The comparison goes through serde rather than through a second spelling:
    // what has to match the reference is the value that crosses the wire.
    let spoken = PermissionScope::ALL
        .into_iter()
        .map(|scope| {
            serde_json::to_value(scope)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .expect("a scope serializes as a string")
        })
        .collect::<Vec<_>>();

    assert_eq!(spoken, corpus.scopes, "the scope vocabulary diverged");
    assert_eq!(corpus.scopes.len(), corpus.counts.scopes);
    eprintln!(
        "permission surface: scopes {}/{}, 0 missing, 0 invented",
        spoken.len(),
        corpus.counts.scopes
    );
}

/// US-105: the requirement carries exactly the reference fields, under the
/// reference aliases, and refuses a surplus one.
#[test]
fn the_requirement_model_is_the_reference_one() {
    let corpus = corpus();
    let requirement = PermissionRequirement::command("cargo test");
    let wire = serde_json::to_value(&requirement).expect("serialize");
    let object = wire.as_object().expect("a requirement is an object");

    // The serialized object is key-sorted, so the declared aliases are compared
    // in the same order rather than in the reference's declaration order.
    let mut declared = corpus
        .requirement
        .fields
        .iter()
        .map(|field| field.alias.clone())
        .collect::<Vec<_>>();
    declared.sort();
    let spoken = object.keys().cloned().collect::<Vec<_>>();
    assert_eq!(spoken, declared, "the requirement field set diverged");
    assert!(
        corpus.requirement.fields.iter().all(|field| field.required),
        "the reference declares every requirement field as required"
    );
    assert!(
        corpus.requirement.forbids_extra,
        "the reference model forbids a surplus field"
    );

    let mut surplus = wire;
    surplus["reason"] = serde_json::Value::String("extra".to_owned());
    assert!(
        serde_json::from_value::<PermissionRequirement>(surplus).is_err(),
        "a surplus field has to be refused here too"
    );
}

/// US-106: the arity table holds the reference entries with the reference
/// values, and neither side may drift.
#[test]
fn the_arity_table_is_the_reference_table() {
    let corpus = corpus();
    let ported = arity::ARITY
        .iter()
        .map(|(prefix, arity)| ((*prefix).to_owned(), *arity))
        .collect::<BTreeMap<_, _>>();

    let missing = corpus
        .arity
        .iter()
        .filter(|(prefix, _)| !ported.contains_key(*prefix))
        .map(|(prefix, _)| prefix.clone())
        .collect::<Vec<_>>();
    let invented = ported
        .keys()
        .filter(|prefix| !corpus.arity.contains_key(*prefix))
        .cloned()
        .collect::<Vec<_>>();
    let disagreeing = corpus
        .arity
        .iter()
        .filter(|(prefix, arity)| ported.get(*prefix).is_some_and(|ported| ported != *arity))
        .map(|(prefix, arity)| format!("{prefix}: reference {arity}, here {:?}", ported[prefix]))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "arity entries missing here: {missing:?}"
    );
    assert!(
        invented.is_empty(),
        "arity entries this port invented: {invented:?}"
    );
    assert!(
        disagreeing.is_empty(),
        "arity values diverged: {disagreeing:?}"
    );
    assert_eq!(ported.len(), corpus.counts.arity_entries);
    eprintln!(
        "permission surface: arity {}/{} entries",
        ported.len(),
        corpus.counts.arity_entries
    );
}

/// US-106: the session pattern a command reduces to is the reference's.
#[test]
fn every_session_pattern_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.session_patterns.len(),
        corpus.counts.session_pattern_cases
    );
    for case in &corpus.session_patterns {
        let tokens = case.tokens.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            arity::build_session_pattern(&tokens),
            case.pattern,
            "`{}` reduces differently here",
            tokens.join(" ")
        );
    }
    eprintln!(
        "permission surface: session patterns {}/{}",
        corpus.session_patterns.len(),
        corpus.counts.session_pattern_cases
    );
}

/// US-107: the wildcard rule, including the optional trailing arguments.
#[test]
fn every_wildcard_verdict_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.wildcard_matches.len(), corpus.counts.wildcard_cases);
    for case in &corpus.wildcard_matches {
        assert_eq!(
            wildcard_match(&case.text, &case.pattern),
            case.matches,
            "`{}` against `{}` is decided differently here",
            case.text,
            case.pattern
        );
    }
    eprintln!(
        "permission surface: wildcard verdicts {}/{}",
        corpus.wildcard_matches.len(),
        corpus.counts.wildcard_cases
    );
}

/// US-108: the two requirements the shared file-tool chain produces carry the
/// reference scope, patterns and label shape.
#[test]
fn the_file_tool_chain_produces_the_reference_requirements() {
    let corpus = corpus();
    let chain = &corpus.file_tool_chain;
    let workspace = tempfile::tempdir().expect("workspace");
    let settings = ToolConfigResolver::new().view::<SharedToolConfig>("read_file");

    let sensitive_path = workspace.path().join(".env");
    fs::write(&sensitive_path, "SECRET=1").expect("write");
    let sensitive = resolve_file_tool_permission(&sensitive_path, "read_file", &settings, None);
    let requirement = sensitive
        .requirements
        .first()
        .expect("a sensitive path raises a requirement");
    assert_eq!(wire_scope(requirement.scope), chain.sensitive_scope);
    assert_eq!(
        requirement.invocation_pattern,
        chain.sensitive_invocation_pattern
    );
    assert_eq!(requirement.session_pattern, chain.sensitive_session_pattern);
    assert_eq!(
        requirement.label,
        chain.sensitive_label.replace("<tool>", "read_file")
    );
    assert_eq!(
        sensitive.permission.map(permission_label),
        Some(chain.permission.as_str())
    );

    let glob = format!(
        "{}/*",
        workspace
            .path()
            .canonicalize()
            .expect("canonical")
            .display()
    );
    let outside = PermissionRequirement::outside_directory(&glob);
    assert_eq!(wire_scope(outside.scope), chain.outside_scope);
    assert_eq!(
        outside.invocation_pattern,
        chain.outside_invocation_pattern.replace("<glob>", &glob)
    );
    assert_eq!(
        outside.session_pattern,
        chain.outside_session_pattern.replace("<glob>", &glob)
    );
    assert_eq!(outside.label, chain.outside_label.replace("<glob>", &glob));
}

/// The settings a sensitive-pattern case is measured under: one pattern, no
/// list beside it, and the permission the capture ran with.
fn sensitive_settings(pattern: &str) -> SharedToolConfig {
    SharedToolConfig {
        permission: PermissionMode::Always,
        allowlist: Vec::new(),
        denylist: Vec::new(),
        sensitive_patterns: vec![pattern.to_owned()],
    }
}

/// US-263: a sensitive pattern is read the way the reference reads it, which is
/// component by component and anchored on the right.
#[test]
fn every_sensitive_pattern_verdict_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.sensitive_matches.len(),
        corpus.counts.sensitive_cases
    );
    let mut refused = 0_usize;
    for case in &corpus.sensitive_matches {
        // A pattern the reference refuses to read raises there and names
        // nothing here, so both sides end up naming no path through it.
        let expected = case.sensitive_matches.unwrap_or(false);
        refused = refused.saturating_add(usize::from(case.sensitive_matches.is_none()));
        assert_eq!(
            path_pattern_matches(&case.pattern, &case.path),
            expected,
            "`{}` against `{}` is decided differently here",
            case.pattern,
            case.path
        );
    }
    eprintln!(
        "permission surface: sensitive patterns {}/{}, {refused} the reference refused to read",
        corpus.sensitive_matches.len(),
        corpus.counts.sensitive_cases
    );
}

/// US-263: the allowlist and the denylist keep the unanchored matcher, so the
/// two answer differently and the corpus proves the difference is measured.
#[test]
fn the_two_path_matchers_stay_distinct() {
    let corpus = corpus();
    let mut separating = Vec::new();
    for case in &corpus.sensitive_matches {
        assert_eq!(
            pattern_matches(&case.pattern, &case.path),
            case.list_matches,
            "`{}` against `{}` is decided differently by the list matcher here",
            case.pattern,
            case.path
        );
        if case.sensitive_matches != Some(case.list_matches) {
            separating.push(format!("{} -> {}", case.pattern, case.path));
        }
    }
    assert!(
        separating.len() >= 6,
        "the corpus has to keep separating the two matchers: {separating:?}"
    );
    eprintln!(
        "permission surface: {} of {} pattern pairs separate the two matchers",
        separating.len(),
        corpus.sensitive_matches.len()
    );
}

/// US-263: the whole chain, not just the matcher, raises what the reference
/// raises for a sensitive pattern.
#[test]
fn the_sensitive_chain_answers_like_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.sensitive_chain.len(),
        corpus.counts.sensitive_chain_cases
    );
    let workspace = tempfile::tempdir().expect("workspace");
    for case in &corpus.sensitive_chain {
        let file = workspace.path().join(&case.path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).expect("parent directories");
        }
        fs::write(&file, "SECRET=1").expect("write");
        let context = resolve_file_tool_permission(
            &file,
            "read_file",
            &sensitive_settings(&case.pattern),
            None,
        );
        assert_eq!(
            context.permission.map(permission_label),
            case.permission.as_deref(),
            "`{}` against `{}` settles differently here",
            case.pattern,
            case.path
        );
        let raised = context
            .requirements
            .iter()
            .map(|requirement| wire_scope(requirement.scope))
            .collect::<Vec<_>>();
        assert_eq!(
            raised, case.scopes,
            "`{}` against `{}` raises other requirements here",
            case.pattern, case.path
        );
    }
    eprintln!(
        "permission surface: sensitive chain {}/{}",
        corpus.sensitive_chain.len(),
        corpus.counts.sensitive_chain_cases
    );
}

/// US-263: the shipped defaults keep naming a dotenv file at any depth, the
/// root included, which is the reach the new matcher had to preserve.
#[test]
fn the_shipped_sensitive_defaults_still_name_a_dotenv_file() {
    let settings = ToolConfigResolver::new().view::<SharedToolConfig>("read_file");
    assert_eq!(settings.sensitive_patterns, ["**/.env", "**/.env.*"]);
    for path in [
        "/.env",
        "/srv/.env",
        "/srv/app/.env",
        "/srv/app/.env.local",
        "/srv/app/nested/deep/.env.production",
    ] {
        assert!(
            matched_path_pattern(&settings.sensitive_patterns, path).is_some(),
            "`{path}` stopped being sensitive"
        );
    }
    for path in ["/srv/app/env", "/srv/app/.environment", "/srv/.env/keep"] {
        assert!(
            matched_path_pattern(&settings.sensitive_patterns, path).is_none(),
            "`{path}` became sensitive"
        );
    }
}

/// US-263: a pattern nothing can read raises nothing and never panics, and the
/// scan still stops on the first pattern that does name the path.
#[test]
fn an_unreadable_sensitive_pattern_raises_nothing_and_stops_nothing() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file = workspace.path().join(".env");
    fs::write(&file, "SECRET=1").expect("write");

    for pattern in ["", ".", "/", "[", "**/["] {
        let context =
            resolve_file_tool_permission(&file, "read_file", &sensitive_settings(pattern), None);
        assert!(
            context.requirements.is_empty(),
            "`{pattern}` raised a requirement"
        );
        assert_eq!(context.permission, None, "`{pattern}` settled the call");
    }

    // Two patterns naming the same file raise one requirement, not two: the
    // reference breaks out of the scan on the first match.
    let settings = SharedToolConfig {
        permission: PermissionMode::Always,
        allowlist: Vec::new(),
        denylist: Vec::new(),
        sensitive_patterns: vec![
            String::new(),
            "**/.env".to_owned(),
            ".env".to_owned(),
            "*".to_owned(),
        ],
    };
    let context = resolve_file_tool_permission(&file, "read_file", &settings, None);
    assert_eq!(context.requirements.len(), 1);
    assert_eq!(context.permission, Some(PermissionMode::Ask));
    assert!(
        context
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("`**/.env`")),
        "the first matching pattern is the one named: {:?}",
        context.reason
    );
}

/// The live probe: recaptures from the pinned checkout and compares, so a
/// reference that moves is reported rather than silently replayed.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "permission surface") {
        eprintln!("{reason}");
        return;
    }
    let Some(interpreter) = pinned_interpreter(&root) else {
        eprintln!("skipping the live permission-surface probe: no reference interpreter");
        return;
    };
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("vocabulary.json");
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
