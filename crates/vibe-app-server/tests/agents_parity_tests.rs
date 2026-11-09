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
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), and every entry and
//! every debt group has to still reproduce, so row 20 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{Capture, OwnProse, Replay, assert_ledger, entries, scenarios};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agents-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/agents-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/agents.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[];

#[test]
fn the_port_runs_every_agent_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "agents parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("agents-parity-port.json"),
    }
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("agents");
}
