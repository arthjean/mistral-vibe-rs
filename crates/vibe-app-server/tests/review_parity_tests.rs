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
    Capture, Class, FieldDivergence, OwnProse, PatternDivergence, Replay, assert_ledger, entries,
    scenarios,
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

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 reviews this otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences an own-words entry covers but whose length v3.1.0 moved out of
/// its band, one by one; chained first, so they are decided before it.
const V3_REFUSED: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "compaction/decisions-across-the-boundary",
        pointer: "/0/context/0/content",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "compaction/reads-across-the-boundary",
        pointer: "/0/context/0/content",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "refuse/decision-parameters",
        pointer: "/4/response/error/message",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "refuse/decision-parameters",
        pointer: "/5/response/error/message",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "refuse/decision-parameters",
        pointer: "/6/response/error/message",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/*/files/notes.txt",
        count: 14,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/files/other.txt",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/files/real.txt",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error",
        count: 188,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/code",
        count: 6,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/data",
        count: 3,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/message",
        count: 3,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result",
        count: 188,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/agent",
        count: 2,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/bumpedAt",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/contextUsage/cachedInputTokens",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/loops",
        count: 2,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/tokenUsage/cachedInputTokens",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/context/*",
        count: 12,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/changes",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/newString",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/oldString",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/replaceAll",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/toolName",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/outcome",
        count: 4,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/state/display/message",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/state/output/previousContent",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/title",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/state/output/occurrences/*/newStartLine",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

const PROSE: &[OwnProse] = &[];

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
    .run(
        entries(V3_REFUSED)
            .chain(entries(PROSE))
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(V3_REFUSED)
            .chain(entries(PROSE))
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("review");
}
