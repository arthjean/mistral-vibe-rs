//! Replays the committed Unified Harness corpus against this port's app server.
//!
//! `scripts/parity/unified_harness.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`, which since v2.26.0 opens every session
//! on its Unified Harness: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, and records what
//! row 36 of `docs/parity.md` is about. The harness selection and its flags;
//! turns, tool calls and their approvals, smart approve, rewinds, background
//! titles, questions, steering and the queue; the turn preparation (skills,
//! mentioned files, the scratchpad); images for a model that reads them, one
//! that is described them and one that cannot; provider failures; code mode,
//! background processes and subagents with their saved sessions; sessions an
//! older Vibe saved, listed, read and imported; scheduled loops; compaction,
//! manual and automatic; and the unified session store across a restart.
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: the system prompt, the tool and
//! argument descriptions, the built-in skills, and the sentences the harness
//! writes into the conversation around a skill, the scratchpad, `/loop` and a
//! compaction.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, or a
//! `FIELDS` entry for a field that differs wherever it appears, and every
//! entry has to still reproduce, so row 36 of `docs/parity.md` is a reading
//! of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("unified-harness-parity/corpus.json");

/// The scorecard the accepted divergence below is decided in.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/unified_harness.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 91;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// The accepted divergence every entry below stands on.
const OWN_PROSE: &str = "The Unified Harness's own prose";

/// A difference this port keeps in the scenarios named.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    reason: &'static str,
}

const SELECTIONS: &[&str] = &["selection/default", "selection/experimental-flag"];
const AUTO_COMPACTIONS: &[&str] = &[
    "compact/auto-empty",
    "compact/auto-failed",
    "compact/auto-midturn",
    "compact/auto-next-turn",
];

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: SELECTIONS,
        pointer: "/steps/2/response/result/runtime/skills/",
        reason: "the built-in skills' prompts, as `runtime/read` publishes them",
    },
    Divergence {
        scenarios: &["selection/legacy-flag"],
        pointer: "/steps/2/response/result/runtime/skills/",
        reason: "the built-in skills' prompts, as the legacy runtime publishes them",
    },
    Divergence {
        scenarios: &["mcp/runtime"],
        pointer: "/steps/1/response/result/runtime/skills/",
        reason: "the built-in skills' prompts, as a session with servers configured publishes them",
    },
    Divergence {
        scenarios: &["mcp/runtime"],
        pointer: "/steps/1/response/result/runtime/issues/0/message",
        reason: "a plugin server the discovery cannot launch is reported in this port's own words",
    },
    Divergence {
        scenarios: &["tools/skill"],
        pointer: "/requests/2/conversation/4/content",
        reason: "the skill tool's answer wraps the skill's content in the harness's own sentence",
    },
    Divergence {
        scenarios: &["tools/skill"],
        pointer: "/steps/0/turn/9/params/patch/7/value",
        reason: "the same answer, as the settled effect's output",
    },
    Divergence {
        scenarios: &["prepare/skills"],
        pointer: "/requests/",
        reason: "a skill a message invokes reaches the model inside the harness's own sentence, \
                 in that turn's request and every later one",
    },
    Divergence {
        scenarios: &["prepare/skills"],
        pointer: "/steps/",
        reason: "the same sentence, as the invocation effect's output, live and read back",
    },
    Divergence {
        scenarios: &["prepare/scratchpad"],
        pointer: "/requests/",
        reason: "the scratchpad restatement a turn opens with is the harness's own sentence",
    },
    Divergence {
        scenarios: &["prepare/scratchpad"],
        pointer: "/steps/",
        reason: "the same restatement, as the user entry carries it",
    },
    Divergence {
        scenarios: &["loops/slash-loop"],
        pointer: "/requests/",
        reason: "`/loop` hands the model the harness's own instructions",
    },
    Divergence {
        scenarios: &["session/compact", "titles/after-compaction"],
        pointer: "/requests/",
        reason: "a compaction asks for its summary and reopens the conversation in the \
                 harness's own words",
    },
    Divergence {
        scenarios: AUTO_COMPACTIONS,
        pointer: "/requests/",
        reason: "the same summary request and reopening, after an automatic compaction",
    },
];

/// A field this port answers differently wherever it appears, in any scenario.
struct FieldDivergence {
    /// Every difference whose JSON pointer ends with this suffix is covered.
    suffix: &'static str,
    reason: &'static str,
}

const FIELDS: &[FieldDivergence] = &[
    FieldDivergence {
        suffix: "/system/0/prose",
        reason: "the system prompt is this port's own wording of the same directives",
    },
    FieldDivergence {
        suffix: "/system/0/sha256",
        reason: "the same system prompt",
    },
    FieldDivergence {
        suffix: "/description/prose",
        reason: "a tool's, an argument's and a built-in skill's description",
    },
    FieldDivergence {
        suffix: "/description/sha256",
        reason: "the same descriptions",
    },
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn scenarios(corpus: &Value) -> BTreeMap<String, &Value> {
    corpus["scenarios"]
        .as_array()
        .expect("the corpus holds a scenario list")
        .iter()
        .map(|entry| {
            (
                entry["name"]
                    .as_str()
                    .expect("every scenario is named")
                    .to_owned(),
                entry,
            )
        })
        .collect()
}

/// Every JSON pointer at which `port` departs from `reference`.
fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let child = format!("{pointer}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        _ if reference != port => found.push(pointer.to_owned()),
        _ => {}
    }
}

fn capture(arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(arguments)
        .args(["--jobs", JOBS])
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the Unified Harness capture script")
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows: unified paths are published verbatim (US-009, US-010) and process.* finds no shell (US-015)"
)]
fn the_port_runs_every_unified_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("unified-harness-parity-port.json");
    let output = capture(&[
        "--server".as_ref(),
        env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture").as_ref(),
        "--output".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "the port capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replayed: Value = serde_json::from_str(
        &std::fs::read_to_string(&output_path).expect("the port capture is readable"),
    )
    .expect("the port capture parses");
    let replayed = scenarios(&replayed);
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        replayed.keys().collect::<Vec<_>>(),
        "the capture script declares other scenarios than the corpus records; recapture the \
         reference with `{CAPTURE_SCRIPT}`"
    );

    let mut unexplained = Vec::new();
    // One count per entry and scenario it names, so an entry that stopped
    // reproducing in one of its scenarios is reported for that one.
    let mut reproduced = BTreeMap::<(usize, &str), usize>::new();
    let mut fields = vec![0_usize; FIELDS.len()];
    let mut conformant = 0;
    for (name, reference) in &recorded {
        let port = replayed[name];
        assert_eq!(
            reference["scenario"], port["scenario"],
            "the capture script changed scenario {name} since the corpus was recorded; \
             recapture the reference with `{CAPTURE_SCRIPT}`"
        );
        let mut found = Vec::new();
        differences(&reference["observed"], &port["observed"], "", &mut found);
        if found.is_empty() {
            conformant += 1;
        }
        for pointer in found {
            match LEDGER.iter().position(|entry| {
                entry.scenarios.contains(&name.as_str()) && pointer.starts_with(entry.pointer)
            }) {
                Some(index) => {
                    let scenario = LEDGER[index]
                        .scenarios
                        .iter()
                        .find(|scenario| **scenario == name.as_str())
                        .expect("the entry names the scenario it matched");
                    *reproduced.entry((index, scenario)).or_default() += 1;
                }
                None => match FIELDS
                    .iter()
                    .position(|field| pointer.ends_with(field.suffix))
                {
                    Some(index) => fields[index] += 1,
                    None => unexplained.push(format!(
                        "{name} {pointer}\n  reference: {}\n  port:      {}",
                        reference["observed"]
                            .pointer(&pointer)
                            .map_or_else(|| "<absent>".to_owned(), Value::to_string),
                        port["observed"]
                            .pointer(&pointer)
                            .map_or_else(|| "<absent>".to_owned(), Value::to_string),
                    )),
                },
            }
        }
    }
    let mut stale = Vec::new();
    for (index, entry) in LEDGER.iter().enumerate() {
        for scenario in entry.scenarios {
            if !reproduced.contains_key(&(index, *scenario)) {
                stale.push(format!("{scenario} {}", entry.pointer));
            }
        }
    }
    for (field, count) in FIELDS.iter().zip(&fields) {
        if *count == 0 {
            stale.push(field.suffix.to_owned());
        }
    }
    println!(
        "unified harness parity: {conformant}/{} scenarios conformant, {} ledgered differences \
         across {} entries",
        recorded.len(),
        reproduced.values().chain(&fields).sum::<usize>(),
        LEDGER.len() + FIELDS.len()
    );
    assert!(
        unexplained.is_empty(),
        "the port departs from the corpus where no ledger entry says it may:\n{}",
        unexplained.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these ledger entries no longer reproduce and should be removed: {stale:?}"
    );
}

#[test]
fn every_ledger_entry_stands_on_the_accepted_divergence_and_a_recorded_scenario() {
    assert!(
        SCORECARD.contains(&format!("| {OWN_PROSE} |")),
        "docs/parity.md no longer accepts the divergence every ledger entry here stands on"
    );
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    for entry in LEDGER {
        assert!(
            !entry.scenarios.is_empty()
                && entry
                    .scenarios
                    .iter()
                    .all(|scenario| recorded.contains_key(*scenario))
                && entry.pointer.starts_with('/')
                && !entry.reason.is_empty(),
            "the ledger entry for {} needs recorded scenarios, a pointer prefix and a reason",
            entry.pointer
        );
    }
    for field in FIELDS {
        assert!(
            field.suffix.starts_with('/') && !field.reason.is_empty(),
            "the field entry for {} needs a pointer suffix and a reason",
            field.suffix
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "unified harness") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let output = capture(&["--check".as_ref(), "--reference".as_ref(), root.as_os_str()]);
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `{CAPTURE_SCRIPT}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
