//! Replays the committed review corpus against this port's app server.
//!
//! `scripts/parity/review.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, drives real turns,
//! hand edits, shell commands, a compaction and a rewind so the session's
//! checkpoint log holds real regions, and records every `review/*` answer, the
//! files on disk each decision leaves, and the refusals of malformed requests.
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; file content, paths,
//! identities, codes and issue paths are compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 15 of `docs/parity.md` is a
//! reading of the summary this file prints. No entry may name row 15, and a
//! field another row owns that stops matching is ledgered under that row.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, FieldDivergence, OwnProse, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("review-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/review-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/review.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server process.
    arguments: &["--jobs", "4"],
};

const LEDGER: &[FieldDivergence] = &[];

/// Every text this port words its own way, digest by digest.
/// The accepted divergences the prose below stands on.
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";
const ENVELOPE: &str = "The compaction envelope's two prose runs";
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &[
            "compaction/decisions-across-the-boundary",
            "compaction/reads-across-the-boundary",
        ],
        pointer: "/*/context/*/content",
        count: 2,
        band: Band::DEFAULT,
        directives: None,
        row: "26",
        accepted: ENVELOPE,
        reason: "the compaction envelope a context read shows across the boundary",
    },
    OwnProse {
        scenarios: &["refuse/decision-parameters"],
        pointer: "/*/response/error/message",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "15",
        accepted: SERVER_PROSE,
        reason: "the refusal of a decision whose parameters are malformed",
    },
];

#[test]
fn the_port_reviews_every_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "review parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("review-parity-port.json"),
    }
    .run(entries(PROSE).chain(entries(LEDGER)));
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(LEDGER)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("review");
}
