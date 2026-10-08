//! Replays the committed connectors corpus against this port's app server.
//!
//! `scripts/parity/connectors.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the Mistral API that answers the
//! connector bootstrap, the authorization pages and the connector gateway, and
//! drives the methods and turns row 21 of `docs/parity.md` is about. `catalog`
//! records what `connector_catalog/*` answers with and without a session;
//! `cache` how the bootstrap cache is read, trusted and rewritten; `publish`
//! which connector tools a session publishes to its runtime and to the model;
//! `call` a connector tool call as the gateway, the model and the client see
//! it; `lifecycle` how a session's catalog moves under refreshes, toggles,
//! running turns and authorization requests. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing, the sentences a failed gateway
//! call tells the model, and the turn identifier a refusal names.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 21 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("connectors-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/connectors.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 43;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// The accepted divergence the row 21 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "Connector call failures are this port's own prose";

/// A difference this port keeps in the scenarios named, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

const GATEWAY_FAILURES: &[&str] = &["call/gateway-401", "call/gateway-404", "call/gateway-500"];

/// The scenarios whose recorded turn streams an assistant entry.
const ASSISTANT_TURNS: &[&str] = &[
    "call/ask-approval",
    "call/disabled-tool",
    "call/gateway-401",
    "call/gateway-404",
    "call/gateway-500",
    "call/not-ready",
    "call/structured",
    "call/success",
    "call/tool-error",
];

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: GATEWAY_FAILURES,
        pointer: "/requests/1/conversation/2/content",
        row: "21",
        reason: "a gateway call that fails tells the model what failed, for which connector and \
                 with which status, in this port's own sentences",
    },
    Divergence {
        scenarios: GATEWAY_FAILURES,
        pointer: "/steps/1/turn/3/patch/0/value/display/message",
        row: "21",
        reason: "the same sentence, as the failed effect's header",
    },
    Divergence {
        scenarios: GATEWAY_FAILURES,
        pointer: "/steps/1/turn/3/patch/0/value/error/message",
        row: "21",
        reason: "the same sentence, as the failed effect's error",
    },
    Divergence {
        scenarios: &["lifecycle/toggle-during-turn"],
        pointer: "/steps/2/response/error/message",
        row: "17",
        reason: "the refusal names the running turn, which this port numbers `turn-N` where the \
                 reference mints a UUID",
    },
    Divergence {
        scenarios: &["lifecycle/toggle-during-turn"],
        pointer: "/steps/3/response/error/message",
        row: "17",
        reason: "the same turn identifier, in the refused refresh",
    },
    Divergence {
        scenarios: &["lifecycle/toggle-during-turn"],
        pointer: "/steps/5/turn/0/entryId",
        row: "17",
        reason: "the normalizer numbers identifiers in order of appearance, and the reference's \
                 turn identifier took a number this port's does not",
    },
    Divergence {
        scenarios: &["lifecycle/toggle-during-turn"],
        pointer: "/steps/7/turn/0/entryId",
        row: "17",
        reason: "the same renumbering, in the second turn",
    },
    Divergence {
        scenarios: ASSISTANT_TURNS,
        pointer: "/steps/1/turn/1/entry/inputEntryId",
        row: "17",
        reason: "since v2.26.0 every public history entry declares the user entry its turn \
                 answers (`vibe/app_server/models.py:949` at `376f6a3`), null on these entries; \
                 this port's entries do not carry the field",
    },
    Divergence {
        scenarios: &["call/ask-approval"],
        pointer: "/callbacks/0/pathScopeChoices",
        row: "17",
        reason: "since v2.26.0 an approval callback lists the path grant scopes it offers \
                 (`vibe/app_server/models.py:322`, filled by `vibe/app_server/_turns.py:863` at \
                 `376f6a3`), empty for a connector call; this port's callback does not carry the \
                 field",
    },
    Divergence {
        scenarios: &["call/ask-approval"],
        pointer: "/steps/1/turn/4/patch/0/value/output/decision/pathScope",
        row: "17",
        reason: "since v2.26.0 the approval decision an effect records holds the path grant \
                 scope it chose (`vibe/app_server/models.py:312` at `376f6a3`), null for a plain \
                 approval; this port's decision does not carry the field",
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
        .expect("python3 runs the connectors capture script")
}

#[test]
fn the_port_runs_every_connector_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("connectors-parity-port.json");
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
        "connectors parity: {conformant}/{} scenarios conformant, {} ledgered differences \
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
        // Row 21 is what this corpus measures, so a difference it keeps is a
        // gap unless the scorecard decided to keep it.
        assert!(
            entry.row != "21" || SCORECARD.contains(&format!("| {OWN_PROSE} |")),
            "the ledger entry for {} keeps a row 21 difference that no accepted divergence \
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
    if let Some(reason) = off_pin_reason(&root, "connectors") {
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
