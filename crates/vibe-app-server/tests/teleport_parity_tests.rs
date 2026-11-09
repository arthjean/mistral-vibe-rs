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
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), and every entry and
//! every debt group has to still reproduce, so row 22 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("teleport-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/teleport-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/teleport.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The accepted divergence the row 22 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "Teleport and Vibe Code refusals are this port's own prose";

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

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &["run/new-branch-without-origin-head"],
        pointer: "/steps/4/sequence/2/event/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the failure a run reports when no base tells how far the branch is ahead",
    },
    OwnProse {
        scenarios: START_FAILURES,
        pointer: "/steps/4/sequence/3/event/error/message",
        count: 8,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the failure a refused, malformed, exhausted or oversized start reports",
    },
    OwnProse {
        scenarios: &["run/push-denied", "telemetry/push-denied"],
        pointer: "/steps/5/sequence/1/event/error/message",
        count: 2,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the failure a run reports when the operator declines the push",
    },
    OwnProse {
        scenarios: &["run/push-rejected"],
        pointer: "/steps/5/sequence/2/event/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the failure a run reports when the remote rejects the push",
    },
    OwnProse {
        scenarios: OPEN_REFUSALS,
        pointer: "/steps/1/response/error/message",
        count: 21,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the sentence refusing the picker: a non-Mistral model, an ineligible or \
                 missing key, no history, no GitHub checkout, or a project listing that failed",
    },
    OwnProse {
        scenarios: &[
            "picker/create-blank",
            "picker/create-refused",
            "picker/select-other-repository",
            "picker/select-read-only",
            "picker/select-unknown",
        ],
        pointer: "/steps/2/response/error/message",
        count: 5,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the sentence refusing a project the picker cannot create or select",
    },
    OwnProse {
        scenarios: &["picker/cancel", "picker/create-blank", "picker/stale-id"],
        pointer: "/steps/3/response/error/message",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the sentence refusing a call on a picker that is gone or was never opened",
    },
    OwnProse {
        scenarios: &[
            "run/configure-picker",
            "run/project-id-spaces",
            "run/project-mismatch",
        ],
        pointer: "/steps/4/sequence/0/response/error/message",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the sentence refusing a start the picker did not prepare",
    },
    OwnProse {
        scenarios: &["run/detached-head"],
        pointer: "/steps/4/sequence/1/event/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the failure a run on a detached HEAD reports",
    },
    OwnProse {
        scenarios: &["lifecycle/respond-without-push"],
        pointer: "/steps/5/sequence/0/response/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the sentence refusing a push answer no run is waiting for",
    },
    OwnProse {
        scenarios: &["lifecycle/respond-without-push"],
        pointer: "/steps/6/response/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the same refusal, for an operation that already ended",
    },
    OwnProse {
        scenarios: &["lifecycle/cancel-while-push-pending"],
        pointer: "/steps/6/sequence/0/response/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "22",
        accepted: OWN_PROSE,
        reason: "the sentence refusing a push answer for a run that was canceled",
    },
];

#[test]
fn the_port_runs_every_teleport_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "teleport parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("teleport-parity-port.json"),
    }
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    // Row 22 is what this corpus measures, so a difference it keeps has to
    // stand on a divergence the scorecard decided to keep.
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("teleport");
}
