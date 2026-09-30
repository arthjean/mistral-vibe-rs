//! Replays the committed agents corpus against this port's app server.
//!
//! `scripts/parity/agents.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, lays out the agent
//! files it declares and drives the methods and turns row 20 of
//! `docs/parity.md` is about. `registry` records which profiles a server
//! discovers and publishes on `agents/list` and `runtime/read`, and what a
//! legacy profile file reads as once migrated; `selection` which profiles a
//! session may start under or switch to, and the refusals it answers with;
//! `profile` what a profile changes in the requests a session sends;
//! `delegation` a `task` call as the parent's client sees it, as the child
//! runs it and as both are saved; `switch` the profile `exit_plan_mode` moves
//! a session to and when the model starts running under it. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: the plan-mode reminders, the
//! sentences `task` and `exit_plan_mode` answer with, the plan review's
//! question and headers, and the notices a turn publishes.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 20 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agents-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/agents.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 38;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// A difference this port keeps in the scenarios named, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

const PLAN_REVIEWS: &[&str] = &[
    "switch/exit-plan-auto",
    "switch/exit-plan-manual",
    "switch/exit-plan-clear",
    "switch/exit-plan-stay",
];

const SWITCHES: &[&str] = &[
    "switch/exit-plan-auto",
    "switch/exit-plan-manual",
    "switch/exit-plan-clear",
];

const TASK_REFUSALS: &[&str] = &[
    "delegation/unknown-agent",
    "delegation/primary-agent",
    "delegation/disabled-subagent",
];

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-clear",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/0/conversation/1/content",
        row: "34",
        reason: "the reminder a plan session injects before its first request is this port's own \
                 prose (`NOTICE`), naming the same plan file, the same read-only rule and the same \
                 two tools as reference `ReadOnlyAgentMiddleware`",
    },
    Divergence {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/1/conversation/1/content",
        row: "34",
        reason: "the same reminder, as the next request resends it",
    },
    Divergence {
        scenarios: &["profile/builtin-tools"],
        pointer: "/requests/2/conversation/1/content",
        row: "34",
        reason: "the same reminder, in the third request",
    },
    Divergence {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
        ],
        pointer: "/requests/1/conversation/4/content",
        row: "34",
        reason: "the reminder a session reads once it leaves plan mode is this port's own prose",
    },
    Divergence {
        scenarios: &["profile/builtin-tools"],
        pointer: "/requests/2/conversation/4/content",
        row: "34",
        reason: "the same leaving reminder, as the third request resends it",
    },
    Divergence {
        scenarios: &[
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/1/conversation/3/content",
        row: "3",
        reason: "`exit_plan_mode` writes its own sentence for each plan-review outcome, as the \
                 model reads the result",
    },
    Divergence {
        scenarios: TASK_REFUSALS,
        pointer: "/requests/1/conversation/2/content",
        row: "3",
        reason: "`task` refuses an agent the session is not offered and a primary agent in its \
                 own sentences, as the model reads them",
    },
    Divergence {
        scenarios: TASK_REFUSALS,
        pointer: "/steps/1/turn/4/patch/0/value/display/message",
        row: "3",
        reason: "the same refusal, as the failed effect's header",
    },
    Divergence {
        scenarios: TASK_REFUSALS,
        pointer: "/steps/1/turn/4/patch/0/value/error/message",
        row: "3",
        reason: "the same refusal, as the failed effect's error",
    },
    Divergence {
        scenarios: &["delegation/depth"],
        pointer: "/requests/2/conversation/2/content",
        row: "3",
        reason: "a subagent that asks for `task` is refused in this port's own sentence",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/3/detail/display/statusText",
        row: "3",
        reason: "`exit_plan_mode`'s status line is this port's own words",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/4/patch/0/value/display/",
        row: "3",
        reason: "`exit_plan_mode`'s call header, the same verbs over this port's own summary, \
                 message and status line",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/6/detail/request/",
        row: "3",
        reason: "the plan review asks its question, describes its four options and names the \
                 plan file in this port's own words; the option labels, which a client answers \
                 with, are the reference's",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/8/patch/0/value/output/result/answers/0/question",
        row: "3",
        reason: "the same question, as the answered callback repeats it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/10/patch/0/value/display/message",
        row: "3",
        reason: "the plan-review outcome, as the settled effect's header",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/10/patch/0/value/output/message",
        row: "3",
        reason: "the plan-review outcome, as the settled effect's output",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/5/message",
        row: "17",
        reason: "the `plan_review_started` notice carries this port's own message",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/11/message",
        row: "17",
        reason: "the `plan_review_ended` notice carries this port's own message",
    },
    Divergence {
        scenarios: SWITCHES,
        pointer: "/steps/1/turn/12/message",
        row: "17",
        reason: "the `agent_changed` notice carries this port's own message",
    },
    Divergence {
        scenarios: &["switch/exit-plan-clear"],
        pointer: "/steps/1/turn/13/message",
        row: "17",
        reason: "the `context_cleared` notice carries this port's own message",
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
        .expect("python3 runs the agents capture script")
}

#[test]
fn the_port_runs_every_agent_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("agents-parity-port.json");
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
        "agents parity: {conformant}/{} scenarios conformant, {} ledgered differences across \
         {} entries",
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
fn every_ledger_entry_names_another_scorecard_row() {
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
        assert_ne!(
            entry.row, "20",
            "row 20 is what this corpus measures, so a difference it keeps is a gap, not a \
             ledger entry: {}",
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
    if let Some(reason) = off_pin_reason(&root, "agents") {
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
