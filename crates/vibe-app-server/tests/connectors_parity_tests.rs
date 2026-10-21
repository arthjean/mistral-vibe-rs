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

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Divergence, Prose, Replay, Tally, differences, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("connectors-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/connectors.py",
    family: &[],
    arguments: JOBS,
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 43;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &[&str] = &["--jobs", "4"];

/// The accepted divergence the row 21 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "Connector call failures are this port's own prose";

const GATEWAY_FAILURES: &[&str] = &["call/gateway-401", "call/gateway-404", "call/gateway-500"];

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
];

#[test]
fn the_port_runs_every_connector_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "connectors parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("connectors-parity-port.json"),
        prose: Prose::Compared,
    }
    .run(entries(LEDGER));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    // Row 21 is what this corpus measures, so a difference it keeps is a gap
    // unless the scorecard decided to keep it.
    Divergence::assert_ledger(
        LEDGER,
        SCORECARD,
        &scenarios(&corpus),
        "21",
        Some(OWN_PROSE),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("connectors");
}

/// The Unified Harness family `--harness unified` captures: the same catalog
/// and gateway reached from `run_typescript`, row 36's measure of the
/// connector path.
const UNIFIED_CORPUS: &str = include_str!("connectors-parity/unified-corpus.json");

const UNIFIED_CAPTURE: Capture = Capture {
    script: "scripts/parity/connectors.py",
    family: &["--harness", "unified"],
    arguments: JOBS,
};

const UNIFIED_SCENARIO_FLOOR: usize = 9;

/// The accepted divergence the unified entries below stand on.
const REJECTED_MIDTURN: &str = "A connector the gateway rejects mid-turn keeps its turn";

const UNIFIED_LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: &["unified/gateway-401"],
        pointer: "/requests/1/conversation/2/content",
        row: "36",
        reason: "the reference suspends the rejected connector by reconfiguring its core while \
                 the turn runs, which its core refuses, so the program reads \
                 `effect_recovery_failed`; this port reads the gateway's own failure",
    },
    Divergence {
        scenarios: &["unified/gateway-401"],
        pointer: "/steps/1/turn/2/patch",
        row: "36",
        reason: "the same failure, as the settled effect and its approval record",
    },
];

#[test]
fn the_port_runs_every_unified_connector_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(UNIFIED_CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= UNIFIED_SCENARIO_FLOOR,
        "the unified corpus holds {} scenarios, below the floor of {UNIFIED_SCENARIO_FLOOR}",
        recorded.len()
    );
    let replayed = UNIFIED_CAPTURE.port(
        &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("connectors-unified-parity-port.json"),
    );
    let mut tally = Tally::new(entries(UNIFIED_LEDGER));
    tally.compare(
        &UNIFIED_CAPTURE,
        &recorded,
        &scenarios(&replayed),
        |reference, port| {
            let mut found = differences(reference, port, Prose::Compared);
            // A prose field is each side's own wording, as the Unified Harness
            // corpus accepts it (`FIELDS` in `unified_harness_parity_tests.rs`).
            found.retain(|pointer| {
                ![
                    "/system/0/prose",
                    "/system/0/sha256",
                    "/description/prose",
                    "/description/sha256",
                ]
                .iter()
                .any(|suffix| pointer.ends_with(suffix))
            });
            found
        },
    );
    tally.conclude("unified connectors parity:");
}

#[test]
fn every_unified_ledger_entry_stands_on_an_accepted_divergence() {
    assert!(
        SCORECARD.contains(&format!("| {REJECTED_MIDTURN} |")),
        "docs/parity.md no longer accepts the divergence the unified ledger stands on"
    );
    let corpus: Value = serde_json::from_str(UNIFIED_CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    for entry in UNIFIED_LEDGER {
        assert!(
            entry.row == "36"
                && entry
                    .scenarios
                    .iter()
                    .all(|scenario| recorded.contains_key(*scenario))
                && !entry.reason.is_empty(),
            "the unified ledger entry for {} needs row 36, recorded scenarios and a reason",
            entry.pointer
        );
    }
}

#[test]
fn the_committed_unified_corpus_still_matches_the_pinned_reference() {
    UNIFIED_CAPTURE.probe("unified connectors");
}
