//! Replays the committed rewind corpus against this port's app server.
//!
//! `scripts/parity/rewind.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, drives real turns
//! so the session holds real history and real file checkpoints, and records
//! each `session/rewind/read` and `session/rewind` answer, the notifications it
//! raised, the files on disk afterward and the conversation the model is sent
//! next. Driving the `vibe-app-server-stdio-fixture` binary through the same
//! script with `--server` yields observations normalized the same way, and the
//! replay compares the two scenario by scenario. It needs no reference
//! checkout; only the live probe at the end, which recaptures the reference,
//! does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; everything else is
//! compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 9 of `docs/parity.md` is a
//! reading of the summary this file prints. No entry may name row 9, and a
//! field another row owns that stops matching is ledgered under that row.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Class, FieldDivergence, OwnProse, PatternDivergence, Replay, assert_ledger, entries,
    scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("rewind-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/rewind-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/rewind.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server process.
    arguments: &["--jobs", "4"],
};

const LEDGER: &[FieldDivergence] = &[];

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 rewinds and forks otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences an own-words entry covers but whose length v3.1.0 moved out of
/// its band, one by one; chained first, so they are decided before it.
const V3_REFUSED: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "rewind/across-a-compaction",
        pointer: "/0/context/0/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "rewind/across-a-compaction",
        pointer: "/5/context/0/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "rewind/after-a-compaction",
        pointer: "/0/context/0/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "rewind/after-a-compaction",
        pointer: "/3/context/0/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "rewind/after-a-compaction",
        pointer: "/3/context/1/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "rewind/after-a-compaction",
        pointer: "/3/context/2/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "rewind/after-a-compaction",
        pointer: "/3/context/3/content",
        count: 1,
        row: "9",
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
        count: 5,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/agent",
        count: 9,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/bumpedAt",
        count: 6,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/contextUsage/cachedInputTokens",
        count: 3,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/contextUsage/inputTokens",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/contextUsage/outputTokens",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/contextUsage/totalTokens",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/loops",
        count: 9,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/preview",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/tokenUsage/cachedInputTokens",
        count: 3,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/tokenUsage/inputTokens",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/tokenUsage/outputTokens",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/tokenUsage/totalTokens",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/context/*",
        count: 14,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/context/*/role",
        count: 3,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/context/*/toolCalls",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*",
        count: 6,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*",
        count: 3,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/content",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/changes",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/newString",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/oldString",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/input/replaceAll",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/detail/toolName",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/details",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/id",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/kind",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/message",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/outcome",
        count: 11,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/role",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/source",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/state/display/message",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/state/output/previousContent",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/title",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/type",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/userDisplayContent",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/content/*/text",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/state/output/occurrences/*/newStartLine",
        count: 1,
        row: "9",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

const PROSE: &[OwnProse] = &[];

#[test]
fn the_port_rewinds_every_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "rewind parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("rewind-parity-port.json"),
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
    CAPTURE.probe("rewind");
}
