//! Replays the committed telemetry session corpus against this port's `vibe`.
//!
//! `scripts/parity/telemetry_session.py` captured the corpus from the pinned
//! reference's own entry points: every scenario builds a fresh home and
//! workspace behind a scripted stand-in for the whole Mistral platform a
//! session reaches, runs one entry point against it, and records every event
//! the datalake received, the census every chat-completions request carried,
//! the attributes every evaluation request posted, and how often the identity,
//! the account and the managed configuration were asked for. This file drives
//! the two surfaces this binary serves, `vibe -p` and the interactive client on
//! a pseudo-terminal, through the same script and compares the normalized
//! observations scenario by scenario; `crates/vibe-acp/tests` replays the
//! editor surface. It needs no reference checkout; only the live probe at the
//! end, which recaptures the reference, does.
//!
//! Every difference has to fall under a `LEDGER` entry, and every entry has to
//! still reproduce, so row 27 of `docs/parity.md` is a reading of the summary
//! this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("telemetry-session/corpus.json");

const CAPTURE_SCRIPT: &str = "scripts/parity/telemetry_session.py";

/// The surfaces this binary serves.
const SURFACES: [&str; 2] = ["programmatic", "tui"];

/// The scenarios the corpus may not fall below on those surfaces, so a
/// recapture that lost coverage fails here and not only on the machine that
/// made it.
const SCENARIO_FLOOR: usize = 30;

/// A difference the replay accepts, and why.
struct Divergence {
    /// The scenarios it applies to: a name, or a prefix ending in `*`.
    scenario: &'static str,
    /// A JSON pointer inside the scenario's observation, where a `*` segment
    /// matches any one segment.
    pointer: &'static str,
    reason: &'static str,
}

/// The requests the terminal client makes besides its turns: row 10 for the
/// fast-model probe, row 16 for the title request. The oracle queues scripted
/// answers only once startup is quiet, so the probe takes the default answer
/// and shifts nothing else.
const BACKGROUND_REQUESTS: &str = "since v2.26.0 the terminal client generates session titles \
     by default (`vibe/core/config/models.py:112` at `376f6a3`), so opening a session first \
     probes once for a fast model to run them on, a request labeled `secondary_call` \
     (`vibe/app_server/_runtime.py:1951-1959`, `vibe/core/llm/model_probe.py:213-216`), and the \
     first answer is followed by a title request labeled `title_generation` \
     (`vibe/core/llm/utility_completion.py:212-213`); this port's client makes neither, so the \
     request count and the census compared position by position differ in every terminal run";

/// Row 16: the title request's own event.
const TITLE_EVENT: &str = "the title request is reported as its own `vibe.request_sent`, with \
     its call type and attachment counts, and the sorted events after it shift by one; this \
     port sends no title request";

const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/is_cold_start",
        reason: "the reference reports whether Python rebuilt its bytecode cache, false from a \
                 source checkout and null for its own frozen binaries; this port is a compiled \
                 binary and reports null",
    },
    Divergence {
        scenario: "tui/*",
        pointer: "/*/chatRequests",
        reason: BACKGROUND_REQUESTS,
    },
    Divergence {
        scenario: "tui/*",
        pointer: "/*/chatMetadata/*",
        reason: BACKGROUND_REQUESTS,
    },
    Divergence {
        scenario: "tui/*",
        pointer: "/*/chatMetadata/*/*",
        reason: BACKGROUND_REQUESTS,
    },
    Divergence {
        scenario: "tui/prompt*",
        pointer: "/*/events/*",
        reason: TITLE_EVENT,
    },
    Divergence {
        scenario: "tui/prompt*",
        pointer: "/*/events/*/event",
        reason: TITLE_EVENT,
    },
    Divergence {
        scenario: "tui/prompt*",
        pointer: "/*/events/*/properties/*",
        reason: TITLE_EVENT,
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
        .filter(|entry| SURFACES.contains(&entry["surface"].as_str().unwrap_or_default()))
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

fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let pointer = pointer.split('/').collect::<Vec<_>>();
    pattern.len() == pointer.len()
        && pattern
            .iter()
            .zip(&pointer)
            .all(|(pattern, segment)| *pattern == "*" || pattern == segment)
}

fn scenario_matches(pattern: &str, name: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == name, |prefix| name.starts_with(prefix))
}

fn capture(arguments: &[&OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(arguments)
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the telemetry session capture script")
}

#[test]
fn the_port_reports_every_session_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios on {SURFACES:?}, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("telemetry-session-port.json");
    let mut arguments: Vec<&OsStr> = vec![
        OsStr::new("--vibe"),
        OsStr::new(env!("CARGO_BIN_EXE_vibe")),
        OsStr::new("--output"),
        output_path.as_os_str(),
    ];
    for surface in SURFACES {
        arguments.extend([OsStr::new("--surface"), OsStr::new(surface)]);
    }
    let output = capture(&arguments);
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
        let mut found = Vec::new();
        differences(
            &reference["observed"],
            &replayed[name]["observed"],
            "",
            &mut found,
        );
        if found.is_empty() {
            conformant += 1;
        }
        for pointer in found {
            match LEDGER.iter().position(|entry| {
                scenario_matches(entry.scenario, name) && pointer_matches(entry.pointer, &pointer)
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
        "telemetry session parity: {conformant}/{} scenarios conformant, {} ledgered differences \
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

/// Recaptures the pinned reference on these surfaces and asserts the committed
/// corpus is still what it answers. The replay above runs regardless, which is
/// what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "telemetry sessions") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let mut arguments: Vec<&OsStr> = vec![
        OsStr::new("--check"),
        OsStr::new("--reference"),
        root.as_os_str(),
    ];
    for surface in SURFACES {
        arguments.extend([OsStr::new("--surface"), OsStr::new(surface)]);
    }
    let output = capture(&arguments);
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `{CAPTURE_SCRIPT}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
