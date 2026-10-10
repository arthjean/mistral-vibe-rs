//! Differential oracle for the permission vocabulary.
//!
//! The Python reference is the authority on which scopes exist, which fields a
//! requirement carries, what the arity table holds, what
//! `build_session_pattern` and `wildcard_match` answer, which matcher reads a
//! sensitive pattern, an allowlist entry and a denylist entry, how a path grant
//! is encoded and matched, and what a stored rule covers.
//! `scripts/parity/permission_surface.py` asks it every question and records
//! the answers; this module replays them against [`PermissionScope`],
//! [`PathGrantScope`], [`PermissionRequirement`], [`arity::ARITY`],
//! [`arity::build_session_pattern`], [`wildcard_match`],
//! [`path_grant_pattern`], [`path_pattern_matches`], [`PermissionRule::covers`],
//! the file-tool chain, [`resolve_file_tool_permission`] composed with the
//! store's working-directory check, and the path scopes an approval offers and
//! grants, [`available_path_scopes`], [`approval_path_scope`] and
//! [`scope_required_permissions`].
//!
//! The corpus is committed: it carries enum values, field names, command names,
//! integers and the answers to cases this repository authored, all of which are
//! observations, and no reference-authored description text, which is what
//! `NOTICE` forbids shipping. Replay therefore runs unconditionally; only the
//! live probe that recaptures from the pinned checkout skips when it is absent.
//!
//! A difference the replay observes is admitted only through [`DIVERGENCES`],
//! one entry per corpus pointer carrying the answer this port gives instead.
//! An entry whose divergence stopped reproducing fails the replay until it is
//! removed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::*;
use crate::parity::{off_pin_reason, pinned_interpreter, reference_root};
use crate::tools::config::ToolConfigResolver;

const CAPTURE_SCRIPT: &str = "scripts/parity/permission_surface.py";
const CORPUS_RELATIVE: &str = "tests/permission-surface/vocabulary.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 6;

/// One observed difference from the reference, scoped to the corpus pointer
/// it contradicts.
struct Divergence {
    /// The JSON pointer into the corpus whose reference answer this port does
    /// not give.
    pointer: &'static str,
    /// What this port answers instead, with `<workdir>` and `<outside>`
    /// standing for the canonical temporary directories the way the corpus
    /// records them.
    port: &'static str,
    /// The reference change and its evidence, and what this port does instead.
    reason: &'static str,
}

/// Every difference the replay admits, measured at the pin in
/// `crate::parity::REFERENCE_COMMIT`.
///
/// The v2.26.0 re-pin opened four, the requirement's `pathScopeRoot` and the
/// three fields of the exact-path grant of a file outside the working
/// directory; the path grant pass closed them.
const DIVERGENCES: &[Divergence] = &[];

/// Whether the port's `answer` at `pointer` is the reference `expected` one or
/// the divergence [`DIVERGENCES`] records there, failing on anything else and
/// on a ledger entry that no longer reproduces.
fn check_against_ledger(pointer: &str, expected: &str, answer: &str) {
    let entry = DIVERGENCES.iter().find(|entry| entry.pointer == pointer);
    match entry {
        None => assert_eq!(
            answer, expected,
            "`{pointer}` diverged from the reference and no ledger entry admits it"
        ),
        Some(entry) => {
            assert_ne!(
                answer, expected,
                "`{pointer}` converged on the reference; remove its DIVERGENCES entry"
            );
            assert_eq!(
                answer, entry.port,
                "`{pointer}` diverges differently from its ledger entry"
            );
        }
    }
}

/// Python's `glob.escape` for a POSIX path: each metacharacter is wrapped in a
/// one-character class.
fn glob_escape(path: &str) -> String {
    path.chars()
        .map(|character| match character {
            '*' | '?' | '[' => format!("[{character}]"),
            other => other.to_string(),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    counts: Counts,
    scopes: Vec<String>,
    path_grant_scopes: Vec<String>,
    requirement: RequirementModel,
    arity: BTreeMap<String, usize>,
    session_patterns: Vec<SessionPatternCase>,
    wildcard_matches: Vec<WildcardCase>,
    sensitive_matches: Vec<SensitiveCase>,
    sensitive_chain: Vec<SensitiveChainCase>,
    file_tool_chain: FileToolChain,
    path_grants: PathGrants,
    covers: Vec<CoversCase>,
    list_chain: Vec<ListChainCase>,
    path_scopes: Vec<PathScopeCase>,
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
    path_grant_encoding_cases: usize,
    path_match_cases: usize,
    covers_cases: usize,
    list_chain_cases: usize,
    outside_targets: usize,
    path_scope_cases: usize,
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
    /// Declared on the model and read on input, but never serialized.
    excluded: bool,
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
    /// What the allowlist matcher, `path_pattern_matches`, answers.
    allow_matches: bool,
    /// What the denylist matcher, `fnmatch`, answers.
    deny_matches: bool,
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
    permission: String,
    outside: Vec<OutsideCase>,
}

/// The requirement the chain raises for one target outside the working
/// directory, with `<outside>` standing for the directory holding it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutsideCase {
    target: String,
    permission: String,
    scope: String,
    invocation_pattern: String,
    session_pattern: String,
    label: String,
    path_scope_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathGrants {
    encodings: Vec<EncodingCase>,
    matches: Vec<PathMatchCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodingCase {
    path: String,
    scope: PathGrantScope,
    pattern: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathMatchCase {
    path: String,
    pattern: String,
    matches: bool,
}

/// What a stored rule covers, as `PermissionStore.covers` answers it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CoversCase {
    rule_scope: PermissionScope,
    rule_pattern: String,
    scope: PermissionScope,
    invocation_pattern: String,
    literal: bool,
    covers: bool,
}

/// What the file-tool chain answers with one list entry configured, with
/// `{workdir}` and `{outside}` standing for the two temporary directories.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListChainCase {
    list: String,
    pattern: String,
    path: String,
    permission: Option<String>,
    scopes: Vec<String>,
}

/// Which scopes an approval of these requirements offers and what each
/// choice grants, as `available_path_scopes`, `scope_required_permissions` and
/// `approval_grant_permissions` answer.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PathScopeCase {
    requirements: Vec<PathScopeRequirement>,
    offered: Vec<PathGrantScope>,
    scoped: BTreeMap<String, Vec<String>>,
    choices: Vec<PathScopeChoice>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PathScopeRequirement {
    scope: PermissionScope,
    invocation_pattern: String,
    path_scope_root: Option<String>,
}

/// One choice an operator may send, and the session patterns it grants or the
/// cause of its refusal.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathScopeChoice {
    selected: Option<PathGrantScope>,
    granted: Option<Vec<String>>,
    refusal: Option<String>,
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

/// Every ledger entry points at a corpus value that exists and cites the pin
/// it was measured at, so an entry cannot outlive the case it admits.
#[test]
fn every_divergence_names_a_corpus_pointer_and_its_evidence() {
    let raw = fs::read_to_string(corpus_path()).expect("committed corpus reads");
    let corpus: serde_json::Value = serde_json::from_str(&raw).expect("corpus parses");
    for entry in DIVERGENCES {
        assert!(
            corpus.pointer(entry.pointer).is_some(),
            "`{}` names nothing in the corpus",
            entry.pointer
        );
        assert!(
            // The short form every corpus prints: the full hash may only be written in
            // the two pin sources, which `the_reference_commit_is_written_in_exactly_two_places`
            // enforces.
            entry
                .reason
                .contains(&crate::parity::REFERENCE_COMMIT[..12]),
            "`{}` does not cite the pin it was measured at",
            entry.pointer
        );
    }
    let mut pointers = DIVERGENCES
        .iter()
        .map(|entry| entry.pointer)
        .collect::<Vec<_>>();
    pointers.sort_unstable();
    pointers.dedup();
    assert_eq!(
        pointers.len(),
        DIVERGENCES.len(),
        "a pointer is ledgered twice"
    );
    eprintln!(
        "permission surface: {} admitted divergences",
        DIVERGENCES.len()
    );
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

/// The two path grant scopes, each under the value the encoded grant carries.
#[test]
fn the_path_grant_scope_vocabulary_is_the_reference_one() {
    let corpus = corpus();
    let spoken = PathGrantScope::ALL
        .into_iter()
        .map(|scope| {
            let wire = serde_json::to_value(scope)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .expect("a path grant scope serializes as a string");
            assert_eq!(wire, scope.label(), "the wire value and the label agree");
            wire
        })
        .collect::<Vec<_>>();
    assert_eq!(
        spoken, corpus.path_grant_scopes,
        "the path grant scope vocabulary diverged"
    );
    eprintln!(
        "permission surface: path grant scopes {}/{}",
        spoken.len(),
        corpus.path_grant_scopes.len()
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

    // A field the reference excludes from serialization never crosses the
    // wire. Every other field it declares is spoken here and refused when
    // missing exactly when the reference requires it, or the ledger admits the
    // difference; the serialized object may carry no key the reference lacks.
    for (index, field) in corpus.requirement.fields.iter().enumerate() {
        if field.excluded {
            continue;
        }
        let answer = if object.contains_key(&field.alias) {
            let mut missing = object.clone();
            missing.remove(&field.alias);
            let refused = serde_json::from_value::<PermissionRequirement>(missing.into()).is_err();
            if refused { "required" } else { "optional" }
        } else {
            "undeclared"
        };
        check_against_ledger(
            &format!("/requirement/fields/{index}"),
            if field.required {
                "required"
            } else {
                "optional"
            },
            answer,
        );
    }
    let invented = object
        .keys()
        .filter(|key| {
            !corpus
                .requirement
                .fields
                .iter()
                .any(|field| !field.excluded && &field.alias == *key)
        })
        .collect::<Vec<_>>();
    assert!(
        invented.is_empty(),
        "the requirement wire field set diverged: {invented:?} is spoken only here"
    );

    // An excluded field is still declared, so the reference reads it on input.
    // A `null` value makes the refusal name the key when it is undeclared here
    // and the type when it is declared, whatever type the field has.
    for (index, field) in corpus.requirement.fields.iter().enumerate() {
        if !field.excluded {
            continue;
        }
        let mut carrying = wire.clone();
        carrying[field.alias.as_str()] = serde_json::Value::Null;
        let undeclared = serde_json::from_value::<PermissionRequirement>(carrying)
            .err()
            .is_some_and(|error| {
                error
                    .to_string()
                    .contains(&format!("unknown field `{}`", field.alias))
            });
        check_against_ledger(
            &format!("/requirement/fields/{index}"),
            "declared",
            if undeclared { "undeclared" } else { "declared" },
        );
    }
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

/// US-108: the sensitive requirement the shared file-tool chain produces
/// carries the reference scope, patterns and label shape.
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
    // The corpus records the resolved working directory as `<workdir>`, raw in
    // the invocation pattern and `glob.escape`d in the session pattern. The
    // port's answer is folded into the same placeholder form, so the corpus
    // value, the answer and the ledger entry are all compared alike.
    let root = workspace
        .path()
        .canonicalize()
        .expect("canonical")
        .display()
        .to_string();
    let escaped = glob_escape(&root);
    check_against_ledger(
        "/fileToolChain/sensitiveInvocationPattern",
        &chain.sensitive_invocation_pattern,
        &requirement.invocation_pattern.replace(&root, "<workdir>"),
    );
    check_against_ledger(
        "/fileToolChain/sensitiveSessionPattern",
        &chain.sensitive_session_pattern,
        &requirement.session_pattern.replace(&escaped, "<workdir>"),
    );
    assert_eq!(
        requirement.label,
        chain.sensitive_label.replace("<tool>", "read_file")
    );
    assert_eq!(
        sensitive.permission.map(permission_label),
        Some(chain.permission.as_str())
    );
}

/// A store trusting `root`, which is what a session opening a workspace does.
async fn store_trusting(root: &Path) -> PermissionStore {
    let store = PermissionStore::default();
    store
        .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    store
}

/// What the chain answers for `context`, in the reference's terms: the
/// permission a list settled, or `ask` with the scopes of what the working
/// directory check still raises, or nothing when neither decided.
async fn chain_answer(
    store: &PermissionStore,
    context: &PermissionContext,
) -> (Option<String>, Vec<String>) {
    if let Some(mode) = context.permission
        && context.requirements.is_empty()
    {
        return (Some(permission_label(mode).to_owned()), Vec::new());
    }
    let resolution = store
        .resolve("read_file", context)
        .await
        .expect("resolution");
    if resolution.required_permissions.is_empty() {
        return (None, Vec::new());
    }
    (
        Some(permission_label(PermissionMode::Ask).to_owned()),
        resolution
            .required_permissions
            .iter()
            .map(|requirement| wire_scope(requirement.scope))
            .collect(),
    )
}

/// Since v2.25.8 a path outside the working directory is asked about by its
/// resolved path, granted by its exact encoded path, and offers a recursive
/// grant root only when it is a directory.
#[tokio::test]
async fn an_outside_target_raises_the_reference_requirement() {
    let corpus = corpus();
    let outside_cases = &corpus.file_tool_chain.outside;
    assert_eq!(outside_cases.len(), corpus.counts.outside_targets);
    let workspace = tempfile::tempdir().expect("workspace");
    let store = store_trusting(workspace.path()).await;
    let settings = ToolConfigResolver::new().view::<SharedToolConfig>("read_file");
    for case in outside_cases {
        let outside = tempfile::tempdir().expect("outside directory");
        let target = match case.target.as_str() {
            "missing-file" => outside.path().join("secret.txt"),
            "file" => {
                let file = outside.path().join("present.txt");
                fs::write(&file, "secret").expect("write");
                file
            }
            "directory" => {
                let directory = outside.path().join("folder");
                fs::create_dir(&directory).expect("directory");
                directory
            }
            other => panic!("the corpus names an unknown target `{other}`"),
        };
        let root = outside
            .path()
            .canonicalize()
            .expect("canonical")
            .display()
            .to_string();
        let context = resolve_file_tool_permission(&target, "read_file", &settings, None);
        let resolution = store
            .resolve("read_file", &context)
            .await
            .expect("resolution");
        assert_eq!(
            permission_label(resolution.mode),
            case.permission,
            "`{}` settles differently here",
            case.target
        );
        let [requirement] = resolution.required_permissions.as_slice() else {
            panic!(
                "`{}` raised {:?} here",
                case.target, resolution.required_permissions
            );
        };
        let placeholder = |value: &str| value.replace(&root, "<outside>");
        assert_eq!(wire_scope(requirement.scope), case.scope);
        assert_eq!(
            placeholder(&requirement.invocation_pattern),
            case.invocation_pattern,
            "`{}` is asked about differently here",
            case.target
        );
        assert_eq!(
            placeholder(&requirement.session_pattern),
            case.session_pattern,
            "`{}` is granted differently here",
            case.target
        );
        assert_eq!(placeholder(&requirement.label), case.label);
        assert_eq!(
            requirement.path_scope_root.as_deref().map(placeholder),
            case.path_scope_root,
            "`{}` offers another recursive grant root here",
            case.target
        );
    }
    eprintln!(
        "permission surface: outside targets {}/{}",
        outside_cases.len(),
        corpus.counts.outside_targets
    );
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
            pure_path_match(&case.pattern, &case.path),
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

/// The allowlist reads an entry under `path_pattern_matches` since v2.25.8
/// and the denylist keeps `fnmatch`, beside the sensitive matcher, so the
/// three path matchers answer differently and the corpus proves the
/// differences are measured.
#[test]
fn the_three_path_matchers_stay_distinct() {
    let corpus = corpus();
    let mut separating = Vec::new();
    for case in &corpus.sensitive_matches {
        assert_eq!(
            path_pattern_matches(&case.path, &case.pattern),
            case.allow_matches,
            "`{}` against `{}` is decided differently by the allowlist matcher here",
            case.pattern,
            case.path
        );
        assert_eq!(
            pattern_matches(&case.pattern, &case.path),
            case.deny_matches,
            "`{}` against `{}` is decided differently by the denylist matcher here",
            case.pattern,
            case.path
        );
        if case.allow_matches != case.deny_matches
            || case.sensitive_matches != Some(case.deny_matches)
        {
            separating.push(format!("{} -> {}", case.pattern, case.path));
        }
    }
    assert!(
        corpus
            .sensitive_matches
            .iter()
            .any(|case| case.allow_matches != case.deny_matches),
        "the corpus has to keep separating the allowlist from the denylist"
    );
    assert!(
        separating.len() >= 6,
        "the corpus has to keep separating the matchers: {separating:?}"
    );
    eprintln!(
        "permission surface: {} of {} pattern pairs separate the matchers",
        separating.len(),
        corpus.sensitive_matches.len()
    );
}

/// A path grant encodes the normalized path under its scope, as the reference
/// writes it for a POSIX and for a Windows path.
#[test]
fn every_path_grant_encoding_matches_the_reference() {
    let corpus = corpus();
    let encodings = &corpus.path_grants.encodings;
    assert_eq!(encodings.len(), corpus.counts.path_grant_encoding_cases);
    for case in encodings {
        assert_eq!(
            path_grant_pattern(&case.path, case.scope),
            case.pattern,
            "`{}` under {:?} is encoded differently here",
            case.path.escape_debug(),
            case.scope
        );
    }
    eprintln!(
        "permission surface: path grant encodings {}/{}",
        encodings.len(),
        corpus.counts.path_grant_encoding_cases
    );
}

/// An encoded grant, an absolute glob and a relative glob each cover what
/// they cover upstream.
#[test]
fn every_path_grant_verdict_matches_the_reference() {
    let corpus = corpus();
    let matches = &corpus.path_grants.matches;
    assert_eq!(matches.len(), corpus.counts.path_match_cases);
    for case in matches {
        assert_eq!(
            path_pattern_matches(&case.path, &case.pattern),
            case.matches,
            "`{}` against `{}` is decided differently here",
            case.path.escape_debug(),
            case.pattern.escape_debug()
        );
    }
    eprintln!(
        "permission surface: path grant verdicts {}/{}",
        matches.len(),
        corpus.counts.path_match_cases
    );
}

/// The requirement a tool raises for one case: an outside path under its
/// exact encoded grant, anything else under its own text.
fn path_scope_requirement(case: &PathScopeRequirement) -> PermissionRequirement {
    let outside = case.scope == PermissionScope::OutsideDirectory;
    PermissionRequirement {
        scope: case.scope,
        session_pattern: if outside {
            path_grant_pattern(&case.invocation_pattern, PathGrantScope::Exact)
        } else {
            case.invocation_pattern.clone()
        },
        label: case.invocation_pattern.clone(),
        literal: false,
        path_scope_root: case.path_scope_root.clone(),
        invocation_pattern: case.invocation_pattern.clone(),
    }
}

/// An approval offers the scopes the reference offers, rewrites its outside
/// paths under each scope as the reference does, and grants or refuses each
/// choice an operator can send as `approval_grant_permissions` does.
#[test]
fn every_path_scope_offer_and_grant_matches_the_reference() {
    let corpus = corpus();
    let cases = &corpus.path_scopes;
    assert_eq!(cases.len(), corpus.counts.path_scope_cases);
    let patterns = |requirements: &[PermissionRequirement]| {
        requirements
            .iter()
            .map(|requirement| requirement.session_pattern.clone())
            .collect::<Vec<_>>()
    };
    let mut choices = 0;
    for (index, case) in cases.iter().enumerate() {
        let requirements = case
            .requirements
            .iter()
            .map(path_scope_requirement)
            .collect::<Vec<_>>();
        assert_eq!(
            available_path_scopes(&requirements),
            case.offered,
            "case {index} offers other scopes here"
        );
        for scope in PathGrantScope::ALL {
            assert_eq!(
                Some(&patterns(&scope_required_permissions(&requirements, scope))),
                case.scoped.get(scope.label()),
                "case {index} is scoped differently under `{}` here",
                scope.label()
            );
        }
        for choice in &case.choices {
            let answer = approval_path_scope(&requirements, choice.selected)
                .map(|scope| patterns(&scope_required_permissions(&requirements, scope)));
            match (&answer, &choice.granted, choice.refusal.as_deref()) {
                (Ok(granted), Some(expected), None) => assert_eq!(
                    granted, expected,
                    "case {index} grants {:?} differently here",
                    choice.selected
                ),
                (Err(PathScopeRefusal::WithoutPaths), None, Some("withoutPaths"))
                | (Err(PathScopeRefusal::NotOffered(_)), None, Some("notOffered")) => {}
                _ => panic!(
                    "case {index} answers {:?} with {answer:?}, the reference with {:?} or {:?}",
                    choice.selected, choice.granted, choice.refusal
                ),
            }
            choices += 1;
        }
    }
    eprintln!(
        "permission surface: path scope cases {}/{}, choices {choices}/{choices}",
        cases.len(),
        corpus.counts.path_scope_cases
    );
}

/// A stored approval covers a requirement as `PermissionStore.covers` does:
/// by path for an outside-directory requirement, by wildcard or by its literal
/// text for every other scope.
#[test]
fn every_covers_verdict_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.covers.len(), corpus.counts.covers_cases);
    for case in &corpus.covers {
        let rule = PermissionRule {
            tool: "read_file".to_owned(),
            scope: Some(case.rule_scope),
            pattern: case.rule_pattern.clone(),
            mode: PermissionMode::Always,
            rationale: SESSION_APPROVAL.to_owned(),
        };
        let requirement = PermissionRequirement {
            scope: case.scope,
            invocation_pattern: case.invocation_pattern.clone(),
            session_pattern: case.invocation_pattern.clone(),
            label: case.invocation_pattern.clone(),
            literal: case.literal,
            path_scope_root: None,
        };
        assert_eq!(
            rule.covers("read_file", &requirement),
            case.covers,
            "a {:?} rule `{}` decides {:?} `{}` (literal {}) differently here",
            case.rule_scope,
            case.rule_pattern,
            case.scope,
            case.invocation_pattern,
            case.literal
        );
    }
    eprintln!(
        "permission surface: covers {}/{}",
        corpus.covers.len(),
        corpus.counts.covers_cases
    );
}

/// The whole chain, with one allowlist or denylist entry: the allowlist reads
/// an encoded grant and anchors an absolute glob, the denylist keeps
/// `fnmatch`, and a path outside the working directory that no entry grants
/// is still asked about.
#[tokio::test]
async fn the_list_chain_answers_like_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.list_chain.len(), corpus.counts.list_chain_cases);
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside directory");
    let store = store_trusting(workspace.path()).await;
    let canonical = |path: &Path| {
        path.canonicalize()
            .expect("canonical")
            .display()
            .to_string()
    };
    let (workdir, outside_root) = (canonical(workspace.path()), canonical(outside.path()));
    let fill = |template: &str| {
        template
            .replace("{workdir}", &workdir)
            .replace("{outside}", &outside_root)
    };
    for case in &corpus.list_chain {
        let entry = vec![fill(&case.pattern)];
        let settings = SharedToolConfig {
            permission: PermissionMode::Ask,
            allowlist: if case.list == "allowlist" {
                entry.clone()
            } else {
                Vec::new()
            },
            denylist: if case.list == "denylist" {
                entry
            } else {
                Vec::new()
            },
            sensitive_patterns: Vec::new(),
        };
        let path = PathBuf::from(fill(&case.path));
        let path = if path.is_absolute() {
            path
        } else {
            workspace.path().join(path)
        };
        let context = resolve_file_tool_permission(&path, "read_file", &settings, None);
        let (permission, scopes) = chain_answer(&store, &context).await;
        assert_eq!(
            (permission.as_deref(), scopes.as_slice()),
            (case.permission.as_deref(), case.scopes.as_slice()),
            "the {} entry `{}` decides `{}` differently here",
            case.list,
            case.pattern,
            case.path
        );
    }
    eprintln!(
        "permission surface: list chain {}/{}",
        corpus.list_chain.len(),
        corpus.counts.list_chain_cases
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
    assert_eq!(
        settings.sensitive_patterns,
        crate::tools::config::DOTENV_PATTERNS
    );
    for path in [
        "/.env",
        "/srv/.env",
        "/srv/app/.env",
        "/srv/app/.env.local",
        "/srv/app/nested/deep/.env.production",
        "/srv/app/.env~",
        "/srv/app/.envrc",
        "/srv/app/.envrc.local",
        "/srv/app/.envrc~",
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
