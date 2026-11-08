//! Replays the committed session corpus against this port's app server.
//!
//! `scripts/parity/sessions.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, drives real turns
//! so sessions reach disk, and records each answer of the saved-session
//! surface (`session/list`, `session/read`, `session/history/*`,
//! `session/turns/list`, `session/rename`, `session/pin`, `session/relocate`,
//! `session/log/read`, `session/resume`, `session/continue`, `session/fork`,
//! `session/delete` and `session/history/clear`), the notifications it raised,
//! and the save directory afterward: the session directories, the metadata
//! keys and value types, the listing cache and the leases. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; everything else is
//! compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 16 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("session-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/session-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/sessions.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

const PROSE: &[OwnProse] = &[];

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 answers this session method otherwise: the Unified Harness the reference now runs on alone saves, lists and resumes sessions with fields and a store layout this port does not reproduce yet";

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/*",
        count: 4,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/failure",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/method",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error",
        count: 5,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/code",
        count: 3,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/data",
        count: 5,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/message",
        count: 3,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result",
        count: 5,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/continueSessionId",
        count: 31,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/lastEventId",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/nextCursor",
        count: 3,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/agent",
        count: 9,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/bumpedAt",
        count: 5,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/contextUsage/cachedInputTokens",
        count: 6,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/loops",
        count: 11,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/session/tokenUsage/cachedInputTokens",
        count: 6,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/leases/active~1<id-1>.lock.json",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-1>/meta/values/artifacts",
        count: 15,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-1>/meta/values/config",
        count: 15,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-1>/meta/values/environment/working_directory",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-1>/meta/values/project",
        count: 15,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-1>/meta/values/runtime_path",
        count: 15,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-2>/meta/values/artifacts",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-2>/meta/values/config",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-2>/meta/values/end_time",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-2>/meta/values/project",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-2>/meta/values/runtime_path",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-3>/meta/values/artifacts",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-3>/meta/values/config",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-3>/meta/values/end_time",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-3>/meta/values/project",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-3>/meta/values/runtime_path",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*",
        count: 9,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/history/*/outcome",
        count: 4,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*",
        count: 3,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/bumpedAt",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/contextUsage/cachedInputTokens",
        count: 25,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/cwd",
        count: 1,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/id",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/loops",
        count: 29,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/outcome",
        count: 17,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/preview",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/rootSessionId",
        count: 2,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/items/*/tokenUsage/cachedInputTokens",
        count: 25,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/state/history/*/outcome",
        count: 30,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-1>/meta/keys/*",
        count: 75,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-2>/meta/keys/*",
        count: 10,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/sessions/unified~1<id-3>/meta/keys/*",
        count: 10,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/store/tree/*",
        count: 970,
        row: "16",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

const LEDGER: &[PatternDivergence] = &[];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: on win32 the reference records a .last_session pointer (keyed by console window or parent pid) that a Linux capture never holds"
)]
fn the_port_answers_every_session_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "session parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("session-parity-port.json"),
    }
    .run(
        entries(PROSE)
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
        entries(PROSE)
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("session");
}
