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
//! SHA-256. This port writes its own prose on purpose (`NOTICE`), so two
//! digests count as equal wherever both sides hold one; file content, paths,
//! identities, codes and issue paths are compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 15 of `docs/parity.md` is a
//! reading of the summary this file prints. No entry may name row 15, and a
//! field another row owns that stops matching is ledgered under that row.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use vibe_core::parity::replay::{Capture, FieldDivergence, Prose, Replay, entries};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("review-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/review.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server process.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 29;

const LEDGER: &[FieldDivergence] = &[];

#[test]
fn the_port_reviews_every_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "review parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("review-parity-port.json"),
        prose: Prose::OwnWords,
    }
    .run(entries(LEDGER));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    FieldDivergence::assert_ledger(LEDGER, SCORECARD, "15");
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("review");
}
