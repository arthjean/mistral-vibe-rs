//! Replays the committed ACP corpus against this port's `vibe-acp`.
//!
//! `scripts/parity/acp.py` captured the corpus from the pinned reference's own
//! `vibe-acp`: every scenario starts the agent over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, and records what
//! the agent writes. Driving this crate's binary through the same script with
//! `--agent` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their agent authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`), so two digests count
//! as equal wherever both sides hold one, and everything else is compared
//! exactly.
//!
//! The corpus also records how the entry point answers argument vectors that
//! exit before it serves anything, which is compared with no ledger at all.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 8 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("acp-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/acp.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 79;
const ARGV_FLOOR: usize = 23;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and agent process.
const JOBS: &str = "6";

/// A difference this port keeps, and the row of `docs/parity.md` that
/// answers for it.
struct Divergence {
    scenario: &'static str,
    /// Every difference under this JSON pointer, in that scenario, is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "initialize/terminal-auth",
        pointer: "/0/response/result/authMethods/1",
        row: "8",
        reason: "the reference runs under a Python interpreter, so its terminal sign-in \
                 relaunches `python3` with the script as the first argument; a native \
                 executable announces itself with `--setup`, which is the reference's own \
                 branch for one",
    },
    Divergence {
        scenario: "ext/config-schema",
        pointer: "/1/response/result",
        row: "30",
        reason: "`_config/schema` relays the app server's `config/schema`, whose content is \
                 the configuration model",
    },
    Divergence {
        scenario: "ext/project-links",
        pointer: "/2/response/result",
        row: "25",
        reason: "`_projectLinks/*` relays the app server's `projectLinks/*`, whose \
                 eligibility rules are the Vibe Code project's",
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

fn is_prose(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("prose") && object.contains_key("sha256")
    })
}

/// Every JSON pointer at which `port` departs from `reference`.
fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    if is_prose(reference) && is_prose(port) {
        return;
    }
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

fn covers(entry: &Divergence, scenario: &str, pointer: &str) -> bool {
    entry.scenario == scenario
        && (pointer == entry.pointer
            || pointer
                .strip_prefix(entry.pointer)
                .is_some_and(|rest| rest.starts_with('/')))
}

fn capture(arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(arguments)
        .args(["--jobs", JOBS])
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the ACP capture script")
}

#[test]
fn the_port_answers_every_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("acp-parity-port.json");
    let output = capture(&[
        "--agent".as_ref(),
        env!("CARGO_BIN_EXE_vibe-acp").as_ref(),
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
    // The command line answers before any scenario runs, and nothing about it
    // is ledgered: every vector exits the way the reference's argparse does.
    let argv = corpus["argv"]
        .as_array()
        .expect("the corpus records the command line");
    assert!(
        argv.len() >= ARGV_FLOOR,
        "the corpus holds {} argument vectors, below the floor of {ARGV_FLOOR}",
        argv.len()
    );
    let mut argv_differences = Vec::new();
    differences(
        &corpus["argv"],
        &replayed["argv"],
        "/argv",
        &mut argv_differences,
    );
    assert!(
        argv_differences.is_empty(),
        "the command line departs from the corpus at {argv_differences:?}"
    );
    let replayed = scenarios(&replayed);
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        replayed.keys().collect::<Vec<_>>(),
        "the capture script declares other scenarios than the corpus records; recapture the \
         reference with `{CAPTURE_SCRIPT}`"
    );

    let mut unexplained = Vec::new();
    let mut reproduced = vec![0_usize; LEDGER.len()];
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
            match LEDGER
                .iter()
                .position(|entry| covers(entry, name, &pointer))
            {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!("{name} {pointer}")),
            }
        }
    }
    let stale: Vec<String> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| format!("{} {}", entry.scenario, entry.pointer))
        .collect();
    println!(
        "acp parity: {}/{} argument vectors and {conformant}/{} scenarios conformant, {} \
         ledgered differences across {} entries",
        argv.len(),
        argv.len(),
        recorded.len(),
        reproduced.iter().sum::<usize>(),
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
fn every_ledger_entry_names_a_scorecard_row_and_a_recorded_scenario() {
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
            entry.scenario,
            entry.row
        );
        assert!(
            recorded.contains_key(entry.scenario),
            "the ledger entry names {}, which the corpus does not record",
            entry.scenario
        );
        assert!(
            entry.pointer.starts_with('/') && !entry.reason.is_empty(),
            "the ledger entry for {} needs a pointer and a reason",
            entry.scenario
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "ACP") {
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
