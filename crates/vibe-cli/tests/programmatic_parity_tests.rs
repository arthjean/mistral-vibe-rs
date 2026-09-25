//! Replays the committed programmatic mode corpus against this port's `vibe`.
//!
//! `scripts/parity/programmatic.py` captured the corpus from the pinned
//! reference's own `vibe` entry point: every scenario builds a fresh home and
//! workspace behind a scripted stand-in for the chat-completions API (and, for
//! Teleport, the console's account endpoint), runs `vibe -p` one or more times
//! with the scenario's arguments and standard input, and records the exit
//! code, standard output, standard error, what each model request asked for
//! and the files the run left behind. `json` and `streaming` documents are
//! kept as values with the order of every object's keys and a flag saying
//! whether the bytes are exactly what Python's `json.dumps` writes. Driving
//! this crate's binary through the same script with `--binary` yields
//! observations normalized the same way, and the replay compares the two
//! scenario by scenario. It needs no reference checkout; only the live probe
//! at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string the program authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`), so two digests count
//! as equal wherever both sides hold one, and everything else is compared
//! exactly. Every other difference has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 13 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("programmatic-parity/corpus.json");

const CAPTURE_SCRIPT: &str = "scripts/parity/programmatic.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 56;

/// A difference the replay accepts, and why.
struct Divergence {
    /// The scenario it applies to, or `*` for every one.
    scenario: &'static str,
    /// A JSON pointer inside the scenario's observation, where a `*` segment
    /// matches any one segment.
    pointer: &'static str,
    reason: &'static str,
}

const LEDGER: &[Divergence] = &[];

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

fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let pointer = pointer.split('/').collect::<Vec<_>>();
    pattern.len() == pointer.len()
        && pattern
            .iter()
            .zip(&pointer)
            .all(|(pattern, segment)| *pattern == "*" || pattern == segment)
}

fn capture(arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(arguments)
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the programmatic capture script")
}

#[test]
fn the_port_answers_every_programmatic_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("programmatic-port.json");
    let output = capture(&[
        "--binary".as_ref(),
        env!("CARGO_BIN_EXE_vibe").as_ref(),
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
            match LEDGER.iter().position(|entry| {
                (entry.scenario == "*" || entry.scenario == name)
                    && pointer_matches(entry.pointer, &pointer)
            }) {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!("{name} {pointer}")),
            }
        }
    }
    let stale: Vec<&str> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| entry.pointer)
        .collect();
    println!(
        "programmatic parity: {conformant}/{} scenarios conformant, {} ledgered differences \
         across {} entries",
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
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    for entry in LEDGER {
        assert!(
            entry.pointer.starts_with('/')
                && !entry.reason.is_empty()
                && !entry.scenario.is_empty(),
            "the ledger entry for {} needs a scenario, a pointer and a reason",
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
    if let Some(reason) = off_pin_reason(&root, "programmatic mode") {
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
