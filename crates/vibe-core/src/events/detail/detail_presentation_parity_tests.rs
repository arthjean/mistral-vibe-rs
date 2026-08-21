//! Replays the presentation corpus this repository captured from the pinned
//! reference.
//!
//! `scripts/parity/tool_presentation.py` drives the reference's
//! `ToolUIDataAdapter` over every tool the Linux surface publishes plus a stub
//! MCP tool and a stub connector tool, and records both presentation entry
//! points: the eight call-display fields with the effect kind, and the five
//! result-display fields with the projected output. This module rebuilds the
//! same displays from [`EffectDetail::for_call`], [`EffectDetail::for_encoded_call`]
//! and the three [`EffectResultDisplay`] constructors, then compares them field
//! by field.
//!
//! A presentation string the reference authored is committed as a
//! `{described, length}` marker rather than as text, which `NOTICE` requires.
//! The replay therefore projects its own value the same way before comparing,
//! so a digest is still a conformance target: any change to what this port
//! renders changes the digest and fails here.
//!
//! Every difference that still stands is listed in [`ledger`], one entry per
//! field per case, naming the story that closes it. A ledger entry whose
//! divergence no longer reproduces fails the suite, so the list shrinks as the
//! stories land instead of aging into a wrong score.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::text::hex_encode;

use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::{EffectDetail, EffectResultDisplay, RemoteSettlement, RemoteToolOrigin};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/tool-presentation/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_presentation.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The case floor this epic commits to, so a regeneration that captured almost
/// nothing fails instead of reporting a clean but empty run.
const MINIMUM_CASES: usize = 40;
/// The tool floor, so a corpus cannot reach the total above by driving one tool
/// forty times.
const MINIMUM_TOOLS: usize = 14;
/// The per-tool floor: the six cases every tool is driven through.
const MINIMUM_CASES_PER_TOOL: usize = 6;

/// The marker key a projected string carries in place of its content. Mirrors
/// `DESCRIBED` in the capture script.
const DESCRIBED: &str = "described";

/// The error the capture authored for its failed calls, mirroring
/// `ERROR_MESSAGE` in the capture script. It is this repository's own text, so
/// the corpus carries it verbatim and a replay can assert the adapter forwards
/// it unchanged.
const ERROR_MESSAGE: &str = "the capture authored this failure";
/// The remote tool name and the server alias both stubs are built from,
/// mirroring `REMOTE_NAME` and `REMOTE_ALIAS`.
const REMOTE_NAME: &str = "create_issue";
const REMOTE_ALIAS: &str = "acme";

/// What a divergence names when no story can close it, because closing it would
/// mean shipping reference prose.
const LICENSING: &str = "NOTICE";

/// The story range `tasks/prd-tool-infrastructure-parity.md` numbers its work
/// in, US-254 through US-268. A closer is checked against it rather than
/// against the shape `US-` alone, so a mistyped identifier naming no story in
/// this PRD fails the audit instead of reading as a plan.
const PRD_STORIES: std::ops::RangeInclusive<u16> = 254..=268;

/// Whether `closed_by` names a story this PRD carries.
fn names_a_story(closed_by: &str) -> bool {
    closed_by
        .strip_prefix("US-")
        .and_then(|number| number.parse::<u16>().ok())
        .is_some_and(|number| PRD_STORIES.contains(&number))
}

/// Why a divergence stands, shared by every case that carries the same gap: the
/// reason is a property of the gap, not of the case that happens to reveal it.
/// Each is stated once so a story landing removes one sentence rather than
/// dozens.
const NO_ARGUMENTS: &str = "the reference answers a call whose arguments never arrived with \
     `get_no_args_display`, whose summary is the tool's display name alone; this port renders the \
     kind's header over empty arguments";
const INVALID_ARGUMENTS: &str = "the reference answers arguments of the wrong class with \
     `get_invalid_args_display`, whose summary is a label the reference authored; this port has \
     no argument model at this boundary, and reaching that digest would mean shipping the \
     reference's label";
const PLAN_DISPLAY: &str = "the reference's `exit_plan_mode` publishes a call display it authored \
     the sentences of; this port renders its own, and reaching those digests would mean copying \
     them";
const PLAN_STATUS: &str = "the reference names the plan-mode wait with a sentence it authored; \
     this port publishes its own label";
const FETCH_SUMMARY: &str = "the reference renders the fetch target and its timeout in a form it \
     authored; this port renders the target alone";
const FETCH_STATUS: &str = "the reference names the fetch with a label it authored; this port \
     publishes its own";
const CALL_CONTENT: &str = "the reference's edit and write displays publish a `content` preview \
     of what the call would change; this port publishes null on every tool";
const RESULT_SETTLEMENT: &str = "the reference settles the call from the tool's own result \
     display, which reads the returned payload; this port settles from the generic kind and \
     repeats the call subject";
const ERROR_FORWARDING: &str = "the reference's adapter reports the error as the result message; \
     this port keeps the call subject and leaves the error to the entry that carries it";
const ERROR_VERB: &str = "the reference's adapter publishes no verb on an errored result; this \
     port keeps the settled verb of the call";
const SKIP_LABEL: &str = "the reference's adapter reports the default skip label; this port \
     writes its own";

/// The twelve tools the Linux surface publishes, as `get_name()` names them.
/// The list is spelled out rather than derived from the corpus so a tool that
/// stops being published fails the floor instead of quietly shrinking the
/// ledger with it.
const BUILTIN_TOOLS: [&str; 12] = [
    "ask_user_question",
    "bash",
    "edit",
    "exit_plan_mode",
    "grep",
    "read_file",
    "skill",
    "task",
    "todo",
    "web_fetch",
    "web_search",
    "write_file",
];

/// The builtins whose kind names a verb of its own, which is why their verbs
/// diverge on the two cases where the reference falls back to the running
/// default. The other four already publish that default.
const SELF_NAMED_VERB_TOOLS: [&str; 8] = [
    "ask_user_question",
    "edit",
    "grep",
    "read_file",
    "skill",
    "web_fetch",
    "web_search",
    "write_file",
];

/// The two builtins whose displays publish a `content` preview upstream.
const CONTENT_TOOLS: [&str; 2] = ["edit", "write_file"];

/// The three call cases every tool is driven through.
const CALL_CASES: [&str; 3] = ["valid-arguments", "absent-arguments", "wrong-argument-type"];

const PLAN_TOOL: &str = "exit_plan_mode";
const FETCH_TOOL: &str = "web_fetch";
const TASK_TOOL: &str = "task";

/// The divergences this port still carries, each with what closes it.
///
/// A pointer is matched by prefix, so `/display` covers every field under it.
/// An entry answers for one field on one case of one tool, which is what keeps
/// the ledger from outliving the divergence it was written for; a gap that
/// spans fourteen tools is expanded over those fourteen names below rather than
/// hidden behind a wildcard.
///
/// Two kinds of entry live here. One names the story that closes it, and the
/// staleness check below deletes it as soon as the story lands. The other names
/// [`LICENSING`], which is the boundary `NOTICE` draws: reaching those digests
/// would mean writing the reference's own sentences into this repository.
fn ledger() -> Vec<Divergence> {
    let mut entries = Vec::new();
    let mut add = |tool: &'static str,
                   case: &'static str,
                   pointer: &'static str,
                   closed_by: &'static str,
                   why: &'static str| {
        entries.push(Divergence {
            tool,
            case,
            pointer,
            closed_by,
            why,
        });
    };
    for tool in BUILTIN_TOOLS {
        add(
            tool,
            "absent-arguments",
            "/display/summary",
            "US-268",
            NO_ARGUMENTS,
        );
        add(
            tool,
            "absent-arguments",
            "/display/message",
            "US-268",
            NO_ARGUMENTS,
        );
        add(
            tool,
            "absent-arguments",
            "/display/settledMessage",
            "US-268",
            NO_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/summary",
            LICENSING,
            INVALID_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/message",
            LICENSING,
            INVALID_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/settledMessage",
            LICENSING,
            INVALID_ARGUMENTS,
        );
        add(
            tool,
            "error-result",
            "/display/message",
            "US-268",
            ERROR_FORWARDING,
        );
        add(tool, "error-result", "/display/verb", "US-268", ERROR_VERB);
        add(
            tool,
            "skipped-result",
            "/display/message",
            "US-268",
            SKIP_LABEL,
        );
    }
    for tool in SELF_NAMED_VERB_TOOLS {
        add(
            tool,
            "absent-arguments",
            "/display/verb",
            "US-268",
            NO_ARGUMENTS,
        );
        add(
            tool,
            "absent-arguments",
            "/display/settledVerb",
            "US-268",
            NO_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/verb",
            "US-268",
            INVALID_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/settledVerb",
            "US-268",
            INVALID_ARGUMENTS,
        );
    }
    for tool in CONTENT_TOOLS {
        add(
            tool,
            "valid-arguments",
            "/display/content",
            "US-268",
            CALL_CONTENT,
        );
    }
    for case in CALL_CASES {
        add(
            PLAN_TOOL,
            case,
            "/display/statusText",
            LICENSING,
            PLAN_STATUS,
        );
        add(
            FETCH_TOOL,
            case,
            "/display/statusText",
            LICENSING,
            FETCH_STATUS,
        );
    }
    add(
        PLAN_TOOL,
        "valid-arguments",
        "/display/summary",
        LICENSING,
        PLAN_DISPLAY,
    );
    add(
        PLAN_TOOL,
        "valid-arguments",
        "/display/message",
        LICENSING,
        PLAN_DISPLAY,
    );
    add(
        PLAN_TOOL,
        "valid-arguments",
        "/display/verb",
        LICENSING,
        PLAN_DISPLAY,
    );
    add(
        PLAN_TOOL,
        "valid-arguments",
        "/display/settledVerb",
        LICENSING,
        PLAN_DISPLAY,
    );
    add(
        FETCH_TOOL,
        "valid-arguments",
        "/display/summary",
        LICENSING,
        FETCH_SUMMARY,
    );
    add(
        FETCH_TOOL,
        "valid-arguments",
        "/display/message",
        LICENSING,
        FETCH_SUMMARY,
    );
    add(
        PLAN_TOOL,
        "valid-arguments",
        "/display/settledMessage",
        LICENSING,
        PLAN_DISPLAY,
    );
    add(
        FETCH_TOOL,
        "valid-arguments",
        "/display/settledMessage",
        LICENSING,
        FETCH_SUMMARY,
    );
    add(
        PLAN_TOOL,
        "successful-result",
        "/display/message",
        "US-268",
        RESULT_SETTLEMENT,
    );
    add(
        PLAN_TOOL,
        "successful-result",
        "/display/verb",
        "US-268",
        RESULT_SETTLEMENT,
    );
    add(
        TASK_TOOL,
        "successful-result",
        "/display/message",
        "US-268",
        RESULT_SETTLEMENT,
    );
    for tool in REMOTE_TOOLS {
        add(
            tool,
            "wrong-argument-type",
            "/display/summary",
            LICENSING,
            INVALID_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/message",
            LICENSING,
            INVALID_ARGUMENTS,
        );
        add(
            tool,
            "wrong-argument-type",
            "/display/settledMessage",
            LICENSING,
            INVALID_ARGUMENTS,
        );
        add(tool, "error-result", "/display/verb", "US-268", ERROR_VERB);
    }
    entries
}

/// The two stub tools the capture builds from the reference's own remote
/// factories, named as their `get_name()` names them.
const REMOTE_TOOLS: [&str; 2] = ["acme_create_issue", "connector_acme_create_issue"];

/// One tolerated gap between this port and the reference.
#[derive(Debug, Clone, Copy)]
struct Divergence {
    tool: &'static str,
    /// The one case this entry answers for. Wildcards are refused by the audit
    /// test: a gap that spans a tool spans it one case at a time, and saying so
    /// is what keeps the ledger from outliving the divergence.
    case: &'static str,
    /// Matched by prefix against the reported JSON pointer.
    pointer: &'static str,
    /// The story that closes this gap, or [`LICENSING`] when none can.
    closed_by: &'static str,
    /// Why the gap stands, asserted non-empty so an entry cannot be added
    /// without a stated reason.
    why: &'static str,
}

impl Divergence {
    fn covers(&self, tool: &str, case: &str, pointer: &str) -> bool {
        self.tool == tool && self.case == case && pointer.starts_with(self.pointer)
    }
}

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Case {
    tool: String,
    /// Which factory published the tool: `builtin`, `mcp` or `connector`.
    source: String,
    case: String,
    /// `call` or `result`, naming which presentation entry point was driven.
    phase: String,
    /// The arguments the case authored, absent for a call case that drives the
    /// no-arguments or the wrong-class branch.
    arguments: Value,
    /// The result the case authored, present only on a successful result.
    #[serde(default)]
    output: Value,
    #[serde(default)]
    error: Value,
    #[serde(default)]
    skipped: bool,
    presentation: Value,
}

impl Case {
    fn id(&self) -> String {
        format!("{}/{}", self.tool, self.case)
    }

    /// The recorded presentation without the projected output, which this port
    /// publishes no counterpart for. Its presence is asserted separately, so
    /// dropping it from a recapture is a named failure rather than a silently
    /// smaller comparison.
    fn document(&self) -> Value {
        let mut document = self.presentation.clone();
        if let Some(fields) = document.as_object_mut() {
            fields.remove("projectedOutput");
        }
        document
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
/// A corpus records what the reference published on one platform, and the
/// surface differs by platform, so replaying a Linux capture on a Windows
/// workstation would diff the host rather than the port.
fn skip_reason(corpus: &Corpus) -> Option<String> {
    (corpus.platform != std::env::consts::OS).then(|| {
        format!(
            "skipping the tool-presentation replay: the corpus records the {} surface and this \
             host is {}; recapture with `scripts/parity/tool_presentation.py --corpus`",
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
        "the corpus layout moved; regenerate with `scripts/parity/tool_presentation.py --corpus`"
    );
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

/// Fails when the corpus no longer covers what the epic commits to, naming the
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
    let sources: BTreeSet<&str> = corpus
        .cases
        .iter()
        .map(|case| case.source.as_str())
        .collect();
    for source in ["builtin", "mcp", "connector"] {
        assert!(
            sources.contains(source),
            "the corpus drops the {source} half of the contract: {sources:?}"
        );
    }
}

// --------------------------------------------------------------------------
// The projection, mirroring the capture script
// --------------------------------------------------------------------------

/// A string's identity without its content. Mirrors `digest` in the capture
/// script, which takes the first thirty-two hex characters of the SHA-256.
fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let hexadecimal = hex_encode(&hasher.finalize());
    format!("sha256:{}", &hexadecimal[..32])
}

/// Every string the case supplied, at any depth, including the object keys.
/// Mirrors `literal_values`.
fn literal_values(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            for (key, item) in fields {
                into.insert(key.clone());
                literal_values(item, into);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| literal_values(item, into)),
        Value::String(text) => {
            into.insert(text.clone());
        }
        _ => {}
    }
}

/// Whether a captured string may be committed as it stands. Mirrors
/// `keeps_literal`: the empty string, a value the capture supplied, a path or
/// request target, and an identifier-shaped token.
fn keeps_literal(value: &str, authored: &BTreeSet<String>) -> bool {
    if value.is_empty() || authored.contains(value) {
        return true;
    }
    if is_pointer(value) || is_identifier(value) {
        return true;
    }
    false
}

/// `/[A-Za-z0-9._~/-]{0,95}` anchored, mirroring `_POINTER`.
fn is_pointer(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('/') else {
        return false;
    };
    rest.len() <= 95
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-'))
}

/// `[A-Za-z][A-Za-z0-9_-]{0,31}` anchored, mirroring `_IDENTIFIER`.
fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && value.len() <= 32
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// The committable form: names and pointers verbatim, prose as a marker.
/// Mirrors `project`.
fn project(value: &Value, authored: &BTreeSet<String>) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), project(item, authored)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| project(item, authored)).collect())
        }
        Value::String(text) if !keeps_literal(text, authored) => {
            json!({DESCRIBED: digest(text), "length": text.chars().count()})
        }
        other => other.clone(),
    }
}

/// The vocabulary one case authored, mirroring `project_case`.
fn authored_vocabulary(case: &Case) -> BTreeSet<String> {
    let mut authored = BTreeSet::from([
        ERROR_MESSAGE.to_owned(),
        REMOTE_NAME.to_owned(),
        REMOTE_ALIAS.to_owned(),
        case.tool.clone(),
    ]);
    literal_values(&case.arguments, &mut authored);
    literal_values(&case.output, &mut authored);
    authored
}

// --------------------------------------------------------------------------
// What this port publishes for the same case
// --------------------------------------------------------------------------

/// Arguments of a shape no tool declares, which is this port's counterpart to
/// the reference's wrong-class branch: there is no argument model at this
/// boundary, so what arrives is whatever the model sent.
fn wrong_arguments() -> Value {
    json!({"unexpected": "wrong"})
}

/// The call display this port builds for the case, which a result case needs
/// too because a settled header is derived from the live one.
fn call_detail(case: &Case) -> Option<EffectDetail> {
    let tool = case.tool.as_str();
    let arguments = match case.case.as_str() {
        // A call whose arguments never arrived reaches this port as an
        // unparseable argument string, which is the state the engine records.
        "absent-arguments" => String::new(),
        "wrong-argument-type" => wrong_arguments().to_string(),
        "valid-arguments" | "successful-result" | "error-result" | "skipped-result" => {
            case.arguments.to_string()
        }
        _ => return None,
    };
    Some(match remote_origin(case) {
        Some(remote) => EffectDetail::for_proxied_call(tool, &arguments, &remote),
        None => EffectDetail::for_encoded_call(tool, &arguments),
    })
}

/// The remote a case's tool is published by, read from the source the capture
/// recorded rather than guessed from the published name.
fn remote_origin(case: &Case) -> Option<RemoteToolOrigin> {
    match case.source.as_str() {
        "mcp" => Some(RemoteToolOrigin::mcp(REMOTE_NAME)),
        "connector" => Some(RemoteToolOrigin::connector(REMOTE_NAME)),
        _ => None,
    }
}

/// What this port publishes for one case, in the corpus's own shape.
fn observed(case: &Case) -> Value {
    let detail = call_detail(case).expect("the corpus names one of the six cases");
    if case.phase == "call" {
        return json!({"kind": detail.kind, "display": detail.display});
    }
    let display = match &detail.remote {
        Some(remote) => {
            let error = case.error.as_str().unwrap_or_default();
            let settled = if !error.is_empty() {
                RemoteSettlement::failed(error, &case.output)
            } else if case.skipped {
                RemoteSettlement::skipped("", &case.output)
            } else {
                RemoteSettlement::answered(&case.output)
            };
            EffectResultDisplay::for_remote(remote, &detail.display, &settled)
        }
        None if !case.error.is_null() => EffectResultDisplay::failed(&detail.display),
        None if case.skipped => EffectResultDisplay::skipped(&case.tool),
        None => {
            EffectResultDisplay::completed(detail.kind, &detail.display, &case.output, &Value::Null)
        }
    };
    json!({"kind": detail.kind, "display": display})
}

/// What this port publishes, projected the way the corpus is, so a described
/// field is compared by digest and an authored one by value.
fn observed_document(case: &Case) -> Value {
    project(&observed(case), &authored_vocabulary(case))
}

// --------------------------------------------------------------------------
// Diffing
// --------------------------------------------------------------------------

#[derive(Debug)]
struct Difference {
    pointer: String,
    expected: Value,
    actual: Value,
}

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

fn compare(expected: &Value, actual: &Value) -> Vec<Difference> {
    let mut found = Vec::new();
    differences("", expected, actual, &mut found);
    found
}

// --------------------------------------------------------------------------
// The tests
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    assert_corpus_floor(&corpus);
    let ledger = ledger();

    let mut conforming = 0;
    let mut tolerated: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut unlisted = Vec::new();
    for case in &corpus.cases {
        let found = compare(&case.document(), &observed_document(case));
        if found.is_empty() {
            conforming += 1;
            continue;
        }
        for difference in found {
            match ledger
                .iter()
                .find(|entry| entry.covers(&case.tool, &case.case, &difference.pointer))
            {
                Some(entry) => {
                    tolerated.insert((
                        format!("{} at {}", case.id(), difference.pointer),
                        entry.closed_by,
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

    let tools: BTreeSet<&str> = corpus.cases.iter().map(|case| case.tool.as_str()).collect();
    println!(
        "tool presentation: {conforming}/{} cases match the reference at {} over {} tools, {} \
         ledger entries exercised",
        corpus.cases.len(),
        &corpus.reference_commit[..12],
        tools.len(),
        tolerated.len()
    );
    for (entry, closed_by) in &tolerated {
        println!("  tolerated {entry} until {closed_by}");
    }

    assert!(
        unlisted.is_empty(),
        "presentation divergences outside the ledger:\n  {}",
        unlisted.join("\n  ")
    );
}

#[test]
fn a_ledger_entry_whose_divergence_is_fixed_fails_the_suite() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let ledger = ledger();
    let mut exercised: BTreeSet<usize> = BTreeSet::new();
    for case in &corpus.cases {
        for difference in compare(&case.document(), &observed_document(case)) {
            if let Some(position) = ledger
                .iter()
                .position(|entry| entry.covers(&case.tool, &case.case, &difference.pointer))
            {
                exercised.insert(position);
            }
        }
    }

    let stale = ledger
        .iter()
        .enumerate()
        .filter(|(position, _)| !exercised.contains(position))
        .map(|(_, entry)| {
            format!(
                "{}/{} at {} ({})",
                entry.tool, entry.case, entry.pointer, entry.closed_by
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
fn every_ledger_entry_names_what_closes_it() {
    let ledger = ledger();
    assert!(!ledger.is_empty(), "the ledger is built from its families");
    let mut seen: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    for entry in &ledger {
        assert!(
            names_a_story(entry.closed_by) || entry.closed_by == LICENSING,
            "a tolerated divergence names a story of this PRD (US-{}..=US-{}) or the licensing \
             boundary that keeps it open, not {}",
            PRD_STORIES.start(),
            PRD_STORIES.end(),
            entry.closed_by
        );
        assert!(entry.pointer.starts_with('/'), "{}", entry.pointer);
        assert_ne!(
            entry.case, "*",
            "a ledger entry answers for one case, not for every case of {}: a wildcard outlives \
             the divergence it was written for",
            entry.tool
        );
        assert_ne!(
            entry.tool, "*",
            "a ledger entry answers for one tool, not for every tool of {}",
            entry.case
        );
        // One field on one case: the pointer names a leaf under `/display`, not
        // the whole display and not the whole presentation.
        assert!(
            entry.pointer.starts_with("/display/") && entry.pointer.matches('/').count() == 2,
            "{}/{} is scoped to {}, which is wider than one field on one case",
            entry.tool,
            entry.case,
            entry.pointer
        );
        assert!(!entry.why.is_empty(), "{}/{}", entry.tool, entry.case);
        assert!(
            seen.insert((entry.tool, entry.case, entry.pointer)),
            "{}/{} at {} is listed twice",
            entry.tool,
            entry.case,
            entry.pointer
        );
    }
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let mut offending = Vec::new();
    for case in &corpus.cases {
        let authored = authored_vocabulary(case);
        collect_literals(&case.presentation, &authored, &mut offending);
    }
    assert!(
        offending.is_empty(),
        "the corpus carries strings that are neither a value the capture authored, a pointer, nor \
         an identifier, so they may be reference prose: {offending:?}"
    );
}

/// Every committed string that [`keeps_literal`] would not have admitted, which
/// is what a prose leak would look like.
fn collect_literals(value: &Value, authored: &BTreeSet<String>, into: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            // A described string is recorded as exactly these two keys.
            if fields.len() == 2 && fields.contains_key(DESCRIBED) && fields.contains_key("length")
            {
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
    // The two sides must digest identically, or every described string would
    // read as a divergence. Anchored on a value computed by the Python side
    // with `hashlib.sha256(text.encode()).hexdigest()[:32]`.
    assert_eq!(
        digest("alpha one"),
        "sha256:447ddb49ae0e88206741f4e0d10b1371"
    );

    let authored = BTreeSet::from(["Which target?".to_owned()]);
    assert!(keeps_literal("Which target?", &authored));
    assert!(keeps_literal("", &authored));
    assert!(keeps_literal("/workspace/alpha.txt", &authored));
    assert!(keeps_literal("Running", &authored));
    assert!(keeps_literal("acme_create_issue", &authored));
    assert!(keeps_literal("in_progress", &authored));
    assert!(!keeps_literal("Waiting for user input", &authored));
    assert!(!keeps_literal(
        "Reading alpha.txt (limit 20 lines)",
        &authored
    ));
    // A described marker counts code points, not bytes, because Python's `len`
    // does.
    assert_eq!(
        project(&json!("→"), &authored),
        json!({DESCRIBED: digest("→"), "length": 1})
    );
}

/// The projected output is recorded for every result case, so a recapture that
/// stopped driving `project_result` fails by name.
///
/// This port publishes no counterpart today, which is why the field is carried
/// rather than compared: the corpus has to hold the whole contract before a
/// later epic can answer for the part of it this port does not implement.
#[test]
fn the_corpus_records_a_projected_output_for_every_result_case() {
    let corpus = corpus();
    let missing = corpus
        .cases
        .iter()
        .filter(|case| case.phase == "result")
        .filter(|case| case.presentation.get("projectedOutput").is_none())
        .map(Case::id)
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "these result cases record no projected output: {missing:?}"
    );
    let projected = corpus
        .cases
        .iter()
        .filter(|case| {
            case.presentation
                .get("projectedOutput")
                .is_some_and(|value| !value.is_null())
        })
        .count();
    assert!(
        projected >= 2,
        "the reference lets a tool override `project_result` and two builtins do; the corpus \
         records {projected} non-null projections"
    );
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
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "tool presentation") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let recaptured = repository.join("target/tool-presentation-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/tool-presentation-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the tool-presentation capture script runs");
    assert!(
        output.status.success(),
        "the tool-presentation capture failed: {}",
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
