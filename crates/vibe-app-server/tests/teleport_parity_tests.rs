//! Replays the committed Teleport corpus against this port's app server.
//!
//! `scripts/parity/teleport.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home and a fresh Git checkout, behind a scripted stand-in for the
//! chat completions, the account, the Vibe Code projects and sessions
//! endpoints and the telemetry sink, and drives the `vibeCode/*` methods row 22
//! of `docs/parity.md` is about. `gate` records who may open a picker; `picker`
//! what `vibeCode/projects/*` answer and the link they leave; `run` the events
//! a run publishes through each push decision and Git state; `summary` the
//! summarization call and the context the start carries; `nuage` the start
//! request, its retries and how each answer is reported; `lifecycle`
//! cancellation and the execution slot; `git` hostile configuration and remote
//! spellings; `telemetry` the events the server sends. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: the refusals and failures the
//! picker and a run report, and the summarization request; plus the turn
//! identifier one refusal names.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 22 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("teleport-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/teleport.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 104;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// The accepted divergence the row 22 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "Teleport and Vibe Code refusals are this port's own prose";

/// A difference this port keeps in the scenarios named, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

/// Refusals of the first call a scenario makes after its session starts: the
/// gate, the checkout and the first project listing.
const OPEN_REFUSALS: &[&str] = &[
    "gate/account-unavailable",
    "gate/codestral-key",
    "gate/missing-key-configure",
    "gate/no-commits",
    "gate/no-github-remote",
    "gate/no-history-no-prompt",
    "gate/no-remote",
    "gate/non-mistral-model",
    "gate/not-a-repository",
    "gate/not-a-repository-teleport",
    "gate/provider-without-backend",
    "gate/unverified-key",
    "git/remote-spelling-3",
    "git/remote-spelling-4",
    "git/remote-spelling-7",
    "git/remote-spelling-8",
    "picker/list-failures",
    "picker/list-invalid-json",
    "picker/list-invalid-schema",
    "picker/not-ready",
    "telemetry/gate-refusals",
];

/// Runs whose start the Vibe Code service refused or that never reached it,
/// reported in the `failed` event after the workflow started.
const START_FAILURES: &[&str] = &[
    "nuage/forbidden",
    "nuage/gateway-timeout-exhausted",
    "nuage/huge-diff",
    "nuage/invalid-json",
    "nuage/invalid-schema",
    "nuage/not-found",
    "nuage/server-error",
    "telemetry/failed-start",
];

/// Sessions with history to summarize, whose summarization request carries
/// this port's own instructions.
const SUMMARIZED: &[&str] = &[
    "summary/empty-answer",
    "summary/history-with-prompt",
    "summary/model-failure",
    "summary/plain-answer",
    "summary/too-long",
    "telemetry/summary",
];

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: OPEN_REFUSALS,
        pointer: "/steps/1/response/error/message",
        row: "22",
        reason: "the sentence refusing the picker: a non-Mistral model, an ineligible or \
                 missing key, no history, no GitHub checkout, or a project listing that failed",
    },
    Divergence {
        scenarios: &[
            "picker/create-blank",
            "picker/create-refused",
            "picker/select-other-repository",
            "picker/select-read-only",
            "picker/select-unknown",
        ],
        pointer: "/steps/2/response/error/message",
        row: "22",
        reason: "the sentence refusing a project the picker cannot create or select",
    },
    Divergence {
        scenarios: &["picker/cancel", "picker/create-blank", "picker/stale-id"],
        pointer: "/steps/3/response/error/message",
        row: "22",
        reason: "the sentence refusing a call on a picker that is gone or was never opened",
    },
    Divergence {
        scenarios: &[
            "run/configure-picker",
            "run/project-id-spaces",
            "run/project-mismatch",
        ],
        pointer: "/steps/4/sequence/0/response/error/message",
        row: "22",
        reason: "the sentence refusing a start the picker did not prepare",
    },
    Divergence {
        scenarios: &["run/detached-head"],
        pointer: "/steps/4/sequence/1/event/error/message",
        row: "22",
        reason: "the failure a run on a detached HEAD reports",
    },
    Divergence {
        scenarios: &["run/new-branch-without-origin-head"],
        pointer: "/steps/4/sequence/2/event/error/message",
        row: "22",
        reason: "the failure a run reports when no base tells how far the branch is ahead",
    },
    Divergence {
        scenarios: START_FAILURES,
        pointer: "/steps/4/sequence/3/event/error/message",
        row: "22",
        reason: "the failure a refused, malformed, exhausted or oversized start reports",
    },
    Divergence {
        scenarios: &["run/push-denied", "telemetry/push-denied"],
        pointer: "/steps/5/sequence/1/event/error/message",
        row: "22",
        reason: "the failure a run reports when the operator declines the push",
    },
    Divergence {
        scenarios: &[
            "run/push-rejected",
            "summary/empty-answer",
            "summary/model-failure",
        ],
        pointer: "/steps/5/sequence/2/event/error/message",
        row: "22",
        reason: "the failure a rejected push or a failed summarization reports",
    },
    Divergence {
        scenarios: &["lifecycle/respond-without-push"],
        pointer: "/steps/5/sequence/0/response/error/message",
        row: "22",
        reason: "the sentence refusing a push answer no run is waiting for",
    },
    Divergence {
        scenarios: &["lifecycle/respond-without-push"],
        pointer: "/steps/6/response/error/message",
        row: "22",
        reason: "the same refusal, for an operation that already ended",
    },
    Divergence {
        scenarios: &["lifecycle/cancel-while-push-pending"],
        pointer: "/steps/6/sequence/0/response/error/message",
        row: "22",
        reason: "the sentence refusing a push answer for a run that was canceled",
    },
    Divergence {
        scenarios: SUMMARIZED,
        pointer: "/requests/0/conversation/2/content",
        row: "22",
        reason: "the summarization request: the compaction instructions, this port's handoff \
                 section and the quoted prompt",
    },
    Divergence {
        scenarios: &["summary/history-without-prompt"],
        pointer: "/requests/0/conversation/1/content",
        row: "22",
        reason: "the same request, one message earlier when the prompt is the last message",
    },
    Divergence {
        scenarios: &["lifecycle/picker-during-turn"],
        pointer: "/steps/4/response/error/message",
        row: "17",
        reason: "the refusal names the running turn, which this port numbers `turn-N` where the \
                 reference mints a UUID",
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
        .expect("python3 runs the teleport capture script")
}

#[test]
fn the_port_runs_every_teleport_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("teleport-parity-port.json");
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
                None => unexplained.push(format!("{name} {pointer}")),
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
    println!(
        "teleport parity: {conformant}/{} scenarios conformant, {} ledgered differences \
         across {} entries",
        recorded.len(),
        reproduced.values().sum::<usize>(),
        LEDGER.len()
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
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let rows: BTreeSet<&str> = SCORECARD
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|line| line.split_once(" |"))
        .map(|(number, _)| number.trim())
        .filter(|number| number.parse::<u32>().is_ok())
        .collect();
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    for entry in LEDGER {
        assert!(
            rows.contains(entry.row),
            "the ledger entry for {} names row {}, which docs/parity.md does not carry",
            entry.pointer,
            entry.row
        );
        // Row 22 is what this corpus measures, so a difference it keeps is a
        // gap unless the scorecard decided to keep it.
        assert!(
            entry.row != "22" || SCORECARD.contains(&format!("| {OWN_PROSE} |")),
            "the ledger entry for {} keeps a row 22 difference that no accepted divergence \
             decides",
            entry.pointer
        );
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
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "teleport") {
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
