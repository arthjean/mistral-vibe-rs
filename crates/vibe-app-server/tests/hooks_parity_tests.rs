//! Replays the committed hooks corpus against this port's app server.
//!
//! `scripts/parity/hooks.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, writes the
//! `hooks.toml` files it declares, and drives turns whose tool calls the hooks
//! guard. Each hook runs a recorder that keeps what it read on stdin and
//! answers as the scenario scripted. The corpus holds the hook notices each
//! turn raised among the transcript entries, every invocation a hook read, the
//! conversation the model was sent, the tool calls the session persisted, the
//! hooks that outlived their wait, and the count and issues `runtime/read`,
//! `diagnostics/list` and `config/read` report as the files change. Driving
//! the `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. Unlike the session corpus, two digests are compared exactly: the
//! notices, denials and retry messages a hook run produces are the contract
//! this part measures, and this port writes them as the reference does.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, or a
//! `FIELDS` entry for a field that differs wherever it appears, and every
//! entry has to still reproduce, so row 18 of `docs/parity.md` is a reading
//! of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, Divergence, OwnProse, PatternDivergence, Replay, assert_ledger, entries,
    scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("hooks-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/hooks-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/hooks.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

const TOOL_RESULTS: &str = "Authored result and error text in six tools";

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &["post_tool/tool-failure"],
    pointer: "/conversations/1/2/content",
    count: 1,
    band: Band::DEFAULT,
    directives: None,
    row: "3",
    accepted: TOOL_RESULTS,
    reason: "`read_file` reports a missing file in this port's own sentence (`NOTICE`), which \
                 the model reads under the reference's `<tool_error>read_file failed: ` frame",
}];

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 runs this hook scenario otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/after",
        count: 6,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/again",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/annotate",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/any",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/array",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/audit",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/bare",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/both",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/broken",
        count: 2,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/decision",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/early",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/explicit",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/extra_guard",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/glob",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/guard",
        count: 3,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/guard_project",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/noted",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/null",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/number",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/observe",
        count: 2,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/project_guard",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/reason",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/redact",
        count: 3,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/regex",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/review",
        count: 6,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/rewriter",
        count: 3,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/second",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/silent",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/slow",
        count: 2,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/slow_int",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/slow_strict",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/star",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/stderr",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/stdout",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/strict",
        count: 2,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/string",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/text",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/third",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/upper",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/user_guard",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/stdin/user_only",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/conversations/*",
        count: 9,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/failure",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error",
        count: 7,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result",
        count: 7,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/hooksCount",
        count: 2,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/reason",
        count: 4,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/hooksCount",
        count: 10,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/conversations/*/*",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/conversations/*/*/content",
        count: 35,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/conversations/*/*/role",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/conversations/*/*/toolCallId",
        count: 29,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*",
        count: 3,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/issues/*",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/issues/*/file",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/issues/*/message",
        count: 7,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*",
        count: 123,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/method",
        count: 78,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry",
        count: 71,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/content",
        count: 26,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail",
        count: 26,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/content",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/display",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/hookName",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/input",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/kind",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/scope",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/status",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/toolCallId",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/toolName",
        count: 19,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/generationStatus",
        count: 37,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/id",
        count: 49,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/inputEntryId",
        count: 42,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/level",
        count: 37,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/message",
        count: 37,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/outcome",
        count: 46,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/role",
        count: 26,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/source",
        count: 26,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/state",
        count: 27,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/title",
        count: 27,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/turnId",
        count: 43,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/type",
        count: 45,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/userDisplayContent",
        count: 26,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entryId",
        count: 62,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch",
        count: 54,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turn",
        count: 31,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turn/id",
        count: 3,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turn/inputEntryId",
        count: 5,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turnId",
        count: 118,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/conversations/*/*/toolCalls/*/id",
        count: 29,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/content/*/text",
        count: 1,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch/*",
        count: 69,
        row: "18",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

const LEDGER: &[Divergence] = &[];

#[test]
fn the_port_runs_every_hook_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "hooks parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("hooks-parity-port.json"),
    }
    .run(
        entries(PROSE)
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
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
    CAPTURE.probe("hooks");
}
