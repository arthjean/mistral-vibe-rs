//! Replays the tool-presentation corpus captured from the pinned reference.
//!
//! `scripts/parity/tool_presentation.py` hands the reference's unified tool
//! projection (`vibe/app_server/_unified_tool_projection.py`,
//! `project_unified_history_entry`) one generic effect entry per scenario, the
//! way a harness reports a call, and records the typed detail and the settled
//! state it makes of it. Each scenario carries the entry as the reference's
//! wire model validated it, every default filled, so this module feeds the
//! same entry to [`project_effect`] (or, for an approval request, to
//! [`project_callback_effect`]) and compares the two through a [`Tally`].
//!
//! A string the reference authored is committed as a `{prose, sha256}` digest
//! rather than as text, which `NOTICE` requires; this module digests its own
//! side the same way before comparing, so a digest is still a conformance
//! target. Everything the scenario supplied, every identifier and every path
//! compares as it stands.
//!
//! The replay is unconditional: the projection is pure, so nothing here needs
//! a reference checkout. Only
//! [`the_committed_corpus_still_matches_the_pinned_reference`] does, and it
//! skips when the attested environment is absent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_core::parity::debt::{Note, assert_notes};
use vibe_core::parity::is_pin_commit;
use vibe_core::parity::replay::{Tally, differences};

use super::{project_callback_effect, project_effect};

const CORPUS_RELATIVE: &str = "crates/vibe-harness/tests/tool-presentation/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_presentation.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 3;

/// Every family the capture records, as the scenario name prefix it is keyed
/// by, with the floor of scenarios it may not fall below. A recapture that
/// lost one fails here by name instead of reporting a smaller green run.
const FAMILY_FLOORS: [(&str, usize); 9] = [
    ("call", 36),
    ("callback", 4),
    ("failed", 2),
    ("invalid", 6),
    ("label", 17),
    ("path", 25),
    ("result", 32),
    ("shell", 10),
    ("unknown", 1),
];

/// Why the port departs from the reference, one cause per constant, each
/// shared by every entry it explains.
const EDIT_RENAMED: &str = "v3 publishes the file edit tool as `edit` and `file_system.edit`, taking \
     `file_path`, `old_string` and `new_string`; this port still projects only the v2 \
     `search_replace` names, so the call stays generic and its result unsettled";
const CONNECTOR_SEARCH: &str = "v3 gives the two calls of its web search connector the typed details of \
     `web_search` and `web_fetch`, and settles them from the cards and the page the \
     connector answers with; this port only labels them";
const CRON_GENERIC: &str = "the reference keeps a cron call's generic detail, `mcpApp` included, and \
     only swaps its header; this port rebuilds the detail without that field";
const PROCESS_ENVELOPE: &str = "the v3 harness answers the process tools with camelCase fields \
     (`processId`, `exitCode`, `bytesAvailable`, `hasMore`, `bytesWritten`); this port \
     reads other keys, so its settled envelope carries nulls there";
const SCRATCHPAD_RESULT: &str = "this port does not settle a scratchpad result the v3 harness answers with \
     a verb and a file list or a path, so the raw envelope stays where the reference \
     publishes the typed output";
const READ_IMAGE: &str = "the reference settles a read whose content holds an image block by naming \
     the image; this port counts its lines";
const LARGE_OUTPUT_RECEIPT: &str = "the reference recognizes a large-output receipt standing in for the \
     structured result and keeps the typed header with a truncated settlement; this \
     port leaves the call generic";
const SHELL_GREP: &str = "v3 renders a plain `grep -n` or `rg -n` shell call as a file search; this \
     port shows every shell call as a shell command";
const DISPLAY_ROOTS: &str = "the reference shows a path relative to the first session display root \
     that holds it; this port's projection takes no display roots and shows the path as \
     the harness reported it";
const V3_LABELS: &str = "v3 labels `self.wait` and the `agents.*` tools that replaced `subagent.*`; \
     this port knows only the v2 labels, so these calls keep the raw key";

/// What the re-pin's measured gaps of this corpus are known to stand on:
/// each note explains groups of its measured debt
/// (`vibe_core::parity::debt`) and counts nothing.
const DEBT_NOTES: &[Note] = &[
    Note {
        scenarios: &[
            "call-edit",
            "call-file_system.edit",
            "path-edit-dotted",
            "path-edit-inside",
            "path-edit-outside",
            "path-edit-relative",
            "path-edit-sibling-prefix",
            "path-edit-the-root",
            "result-edit",
        ],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/settledVerb",
            "/detail/display/statusText",
            "/detail/display/summary",
            "/detail/display/verb",
            "/detail/input/filePath",
            "/detail/input/file_path",
            "/detail/input/newString",
            "/detail/input/new_string",
            "/detail/input/oldString",
            "/detail/input/old_string",
            "/detail/input/replaceAll",
            "/detail/input/replace_all",
            "/detail/kind",
            "/detail/mcpApp",
        ],
        reason: EDIT_RENAMED,
    },
    Note {
        scenarios: &[
            "path-edit-dotted",
            "path-edit-inside",
            "path-edit-outside",
            "path-edit-relative",
            "path-edit-sibling-prefix",
            "path-edit-the-root",
        ],
        pointers: &[
            "/state/display/message",
            "/state/display/verb",
            "/state/output/_meta",
            "/state/output/content",
            "/state/output/file",
            "/state/output/newString",
            "/state/output/occurrences",
            "/state/output/oldString",
            "/state/output/previousContent",
            "/state/output/structured_content",
        ],
        reason: EDIT_RENAMED,
    },
    Note {
        scenarios: &["result-edit"],
        pointers: &[
            "/state/display/message",
            "/state/display/verb",
            "/state/display/warnings/*",
            "/state/output/_meta",
            "/state/output/content",
            "/state/output/file",
            "/state/output/newString",
            "/state/output/occurrences",
            "/state/output/oldString",
            "/state/output/previousContent",
            "/state/output/structured_content",
        ],
        reason: EDIT_RENAMED,
    },
    Note {
        scenarios: &[
            "call-connector_web_search.web_search",
            "result-connector_web_search.web_search",
        ],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/summary",
            "/detail/kind",
            "/detail/mcpApp",
        ],
        reason: CONNECTOR_SEARCH,
    },
    Note {
        scenarios: &[
            "call-connector_web_search.open_url",
            "result-connector_web_search.open_url",
        ],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/settledVerb",
            "/detail/display/statusText",
            "/detail/display/summary",
            "/detail/display/verb",
            "/detail/input/timeout",
            "/detail/kind",
            "/detail/mcpApp",
        ],
        reason: CONNECTOR_SEARCH,
    },
    Note {
        scenarios: &["result-connector_web_search.web_search"],
        pointers: &[
            "/state/display/message",
            "/state/output/answer",
            "/state/output/content",
            "/state/output/query",
            "/state/output/sources",
            "/state/output/structured_content",
        ],
        reason: CONNECTOR_SEARCH,
    },
    Note {
        scenarios: &["result-connector_web_search.open_url"],
        pointers: &[
            "/state/display/message",
            "/state/display/verb",
            "/state/output/content",
            "/state/output/contentType",
            "/state/output/structured_content",
            "/state/output/url",
            "/state/output/wasTruncated",
        ],
        reason: CONNECTOR_SEARCH,
    },
    Note {
        scenarios: &[
            "call-vibe.cron-cancel",
            "call-vibe.cron-clear",
            "call-vibe.cron-list",
            "call-vibe.cron-schedule",
            "call-vibe.cron-schedule_cron",
            "result-vibe.cron-cancel",
            "result-vibe.cron-clear",
            "result-vibe.cron-list-empty",
        ],
        pointers: &["/detail/mcpApp"],
        reason: CRON_GENERIC,
    },
    Note {
        scenarios: &["result-process.start"],
        pointers: &["/state/output/processId"],
        reason: PROCESS_ENVELOPE,
    },
    Note {
        scenarios: &["result-process.stop", "result-process.write"],
        pointers: &[
            "/state/output/bytesWritten",
            "/state/output/exitCode",
            "/state/output/processId",
        ],
        reason: PROCESS_ENVELOPE,
    },
    Note {
        scenarios: &["result-process.output"],
        pointers: &[
            "/state/output/bytesAvailable",
            "/state/output/hasMore",
            "/state/output/processId",
        ],
        reason: PROCESS_ENVELOPE,
    },
    Note {
        scenarios: &[
            "result-vibe.unified_harness_scratchpad-list",
            "result-vibe.unified_harness_scratchpad-write",
        ],
        pointers: &[
            "/state/output/content",
            "/state/output/files",
            "/state/output/path",
            "/state/output/structured_content",
        ],
        reason: SCRATCHPAD_RESULT,
    },
    Note {
        scenarios: &["result-read_file-image"],
        pointers: &["/state/display/message"],
        reason: READ_IMAGE,
    },
    Note {
        scenarios: &["result-read_file-receipt"],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/settledVerb",
            "/detail/display/statusText",
            "/detail/display/summary",
            "/detail/display/verb",
            "/detail/input/filePath",
            "/detail/input/offset",
            "/detail/input/path",
            "/detail/kind",
            "/detail/mcpApp",
            "/state/display/message",
            "/state/display/suffix",
            "/state/display/verb",
            "/state/output",
            "/state/outputText",
        ],
        reason: LARGE_OUTPUT_RECEIPT,
    },
    Note {
        scenarios: &[
            "shell-grep-no-match-result",
            "shell-grep-pattern-flag",
            "shell-grep-recursive",
            "shell-grep-recursive-result",
            "shell-grep-rg-directory",
            "shell-grep-rg-file-prefixed",
        ],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/settledVerb",
            "/detail/display/statusText",
            "/detail/display/summary",
            "/detail/display/verb",
            "/detail/input/command",
            "/detail/input/maxMatches",
            "/detail/input/path",
            "/detail/input/pattern",
            "/detail/kind",
            "/detail/toolName",
        ],
        reason: SHELL_GREP,
    },
    Note {
        scenarios: &["shell-grep-recursive-result"],
        pointers: &[
            "/state/display/message",
            "/state/display/verb",
            "/state/output/matchCount",
            "/state/output/matches",
            "/state/output/output",
            "/state/output/parsedMatches",
            "/state/output/stderr",
            "/state/output/stdout",
            "/state/output/truncated",
            "/state/output/wasTruncated",
        ],
        reason: SHELL_GREP,
    },
    Note {
        scenarios: &["shell-grep-no-match-result"],
        pointers: &[
            "/state/display/message",
            "/state/display/success",
            "/state/display/verb",
            "/state/output/matchCount",
            "/state/output/matches",
            "/state/output/output",
            "/state/output/parsedMatches",
            "/state/output/stderr",
            "/state/output/stdout",
            "/state/output/truncated",
            "/state/output/wasTruncated",
        ],
        reason: SHELL_GREP,
    },
    Note {
        scenarios: &[
            "path-grep-dotted",
            "path-grep-inside",
            "path-grep-the-root",
            "path-read_file-dotted",
            "path-read_file-inside",
            "path-read_file-second-root",
            "path-read_file-the-root",
            "path-write_file-dotted",
            "path-write_file-inside",
            "path-write_file-the-root",
        ],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/summary",
        ],
        reason: DISPLAY_ROOTS,
    },
    Note {
        scenarios: &[
            "path-read_file-dotted",
            "path-read_file-inside",
            "path-read_file-the-root",
            "path-write_file-dotted",
            "path-write_file-inside",
            "path-write_file-the-root",
        ],
        pointers: &["/state/display/message"],
        reason: DISPLAY_ROOTS,
    },
    Note {
        scenarios: &[
            "callback-agents.list",
            "label-agents.list",
            "label-agents.list-result",
            "label-agents.list-result-kept",
            "label-agents.send_message",
            "label-agents.send_message-result",
            "label-self.wait",
            "label-self.wait-result",
        ],
        pointers: &[
            "/detail/display/message",
            "/detail/display/settledMessage",
            "/detail/display/settledVerb",
            "/detail/display/statusText",
            "/detail/display/summary",
            "/detail/display/verb",
            "/detail/effect/display/message",
            "/detail/effect/display/settledMessage",
            "/detail/effect/display/settledVerb",
            "/detail/effect/display/statusText",
            "/detail/effect/display/summary",
            "/detail/effect/display/verb",
        ],
        reason: V3_LABELS,
    },
    Note {
        scenarios: &[
            "label-agents.list-result",
            "label-agents.send_message-result",
            "label-self.wait-result",
        ],
        pointers: &["/state/display/message", "/state/display/verb"],
        reason: V3_LABELS,
    },
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    scenario: Inputs,
    /// The projected detail, and for an effect the settled state, compared as
    /// one observation.
    observed: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Inputs {
    /// The wire entry the harness reported, as the reference validated it.
    entry: Value,
    /// The directories the reference shows a path relative to. This port's
    /// projection takes none.
    display_roots: Vec<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus_text() -> String {
    fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed")
}

fn corpus() -> Corpus {
    let corpus: Corpus = serde_json::from_str(&corpus_text()).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `{CAPTURE_SCRIPT} --corpus`"
    );
    assert!(
        is_pin_commit(&corpus.reference.commit),
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

// --------------------------------------------------------------------------
// Projection, mirroring the capture script
// --------------------------------------------------------------------------

/// Prose as the corpus records it: its length in code points and its
/// SHA-256. Mirrors `digest`.
fn digest(text: &str) -> Value {
    json!({
        "prose": text.chars().count(),
        "sha256": vibe_core::text::sha256_hex(text.as_bytes()),
    })
}

/// An identifier-shaped token. Mirrors `_IDENTIFIER`.
fn is_identifier(text: &str) -> bool {
    text.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && text.chars().count() <= 32
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// A path or a size. Mirrors `_POINTER`.
fn is_pointer(text: &str) -> bool {
    (1..=128).contains(&text.chars().count())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-'))
}

/// Whether a captured string may be compared as it stands. Mirrors
/// `keeps_literal`.
fn keeps_literal(text: &str, authored: &BTreeSet<String>) -> bool {
    authored.contains(text) || is_identifier(text) || is_pointer(text)
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

/// Every string `value` holds, at any depth.
fn strings(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => fields.values().for_each(|item| strings(item, into)),
        Value::Array(items) => items.iter().for_each(|item| strings(item, into)),
        Value::String(text) => {
            into.insert(text.clone());
        }
        _ => {}
    }
}

/// The vocabulary one scenario supplied: its entry and its display roots.
/// Mirrors the seeding in `project_record`.
fn authored(inputs: &Inputs) -> BTreeSet<String> {
    let mut authored = BTreeSet::new();
    strings(&inputs.entry, &mut authored);
    authored.extend(inputs.display_roots.iter().cloned());
    authored
}

// --------------------------------------------------------------------------
// Driving this port
// --------------------------------------------------------------------------

/// What this port makes of one scenario's entry, in the corpus's observation
/// shape and projected the way the corpus is.
fn port_observation(inputs: &Inputs) -> Value {
    let mut entry = inputs.entry.clone();
    let observed = match entry["type"].as_str() {
        Some("effect") => {
            project_effect(&mut entry);
            json!({"detail": entry["detail"], "state": entry["state"]})
        }
        Some("callback") => {
            if entry["detail"]["kind"] == "approval" {
                project_callback_effect(&mut entry["detail"]["effect"]);
            }
            json!({"detail": entry["detail"]})
        }
        other => panic!("the corpus holds an entry of type {other:?}, which no scenario authors"),
    };
    project(&observed, &authored(inputs))
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_corpus_covers_every_family_the_replay_claims() {
    let corpus = corpus();
    let mut per_family: BTreeMap<&str, usize> = BTreeMap::new();
    let mut names = BTreeSet::new();
    for scenario in &corpus.scenarios {
        assert!(
            names.insert(scenario.name.as_str()),
            "two scenarios are named {}",
            scenario.name
        );
        let family = scenario
            .name
            .split_once('-')
            .map_or(scenario.name.as_str(), |(family, _)| family);
        *per_family.entry(family).or_default() += 1;
    }
    for (family, floor) in FAMILY_FLOORS {
        let found = per_family.get(family).copied().unwrap_or_default();
        assert!(
            found >= floor,
            "the {family} family holds {found} scenarios, below its floor of {floor}; regenerate \
             with `{CAPTURE_SCRIPT} --corpus`"
        );
    }
    let known: BTreeSet<&str> = FAMILY_FLOORS.iter().map(|(family, _)| *family).collect();
    let unknown: Vec<&&str> = per_family.keys().filter(|f| !known.contains(**f)).collect();
    assert!(
        unknown.is_empty(),
        "the corpus records families this replay does not name: {unknown:?}"
    );
    assert_eq!(
        corpus.platform, "linux",
        "the corpus is captured on Linux, where the reference resolves the paths it shows"
    );
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let mut offending = Vec::new();
    for scenario in &corpus.scenarios {
        let authored = authored(&scenario.scenario);
        collect_literals(&scenario.observed, &authored, &mut offending);
    }
    assert!(
        offending.is_empty(),
        "the corpus carries strings that are neither a value its scenario supplied, an \
         identifier, nor a path, so they may be reference prose: {offending:?}"
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
    // Anchored on a value computed by the Python side with
    // `hashlib.sha256(text.encode()).hexdigest()`.
    assert_eq!(
        digest("alpha one"),
        json!({
            "prose": 9,
            "sha256": "447ddb49ae0e88206741f4e0d10b13711675bb523d438de9c21de83c81a3fff4"
        })
    );
    let authored = BTreeSet::from(["check the build".to_owned()]);
    assert!(keeps_literal("check the build", &authored));
    assert!(keeps_literal("Reading", &authored));
    assert!(keeps_literal("src/alpha.txt", &authored));
    assert!(keeps_literal("/workspace-other/alpha.txt", &authored));
    assert!(keeps_literal("1.5KB", &authored));
    assert!(!keeps_literal("Reading src/alpha.txt", &authored));
    assert!(!keeps_literal("(truncated)", &authored));
    assert!(!keeps_literal("", &authored));
}

#[test]
fn the_tool_projection_matches_the_reference() {
    let text = corpus_text();
    let corpus = corpus();
    let mut tally = Tally::new(&text, []);
    for scenario in &corpus.scenarios {
        let port = port_observation(&scenario.scenario);
        let found = differences(&scenario.observed, &port);
        tally.record(&scenario.name, &scenario.observed, &port, found);
    }
    tally.conclude("tool presentation parity:");
}

/// Recaptures against the attested reference environment and asserts the
/// committed corpus is still what the pinned reference answers.
///
/// This is the only test here that needs the reference, and it skips naming
/// why when this machine cannot act as the oracle. Everything above replays
/// regardless, which is what keeps a missing environment from failing
/// `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some(root) = vibe_core::parity::replay::pinned_reference("tool presentation") else {
        return;
    };
    let repository = repo_root();
    let recaptured = repository.join("target/tool-presentation-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/tool-presentation-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .env_remove("FORCE_COLOR")
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
    let committed: Value = serde_json::from_str(&corpus_text()).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}

#[test]
fn every_debt_note_explains_a_measured_difference() {
    assert_notes(CORPUS_RELATIVE, DEBT_NOTES);
}
