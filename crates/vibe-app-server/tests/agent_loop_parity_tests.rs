//! Replays the committed agent loop corpus against this port's app server.
//!
//! `scripts/parity/agent_loop.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the chat-completions API, and
//! drives the turns row 34 of `docs/parity.md` is about. `persist` records the
//! lines a turn appends to `messages.jsonl`, key order included, the
//! statistics `meta.json` keeps and what a reload shows; `stats` the cached
//! tokens and the session cost; `interrupt` a turn cut short while a tool
//! runs, before it runs and while it waits on its approval; `approval` the
//! session and permanent grants, and the path scope an approval of an
//! outside path offers; `steer` and `inject` the context a running
//! or idle session takes; `plan` a plan reviewed and edited by hand; `titles`
//! the background title, its cadence and the snapshots that announce it;
//! `relocate` a session moved to another worktree; `children` a delegated
//! subagent's saved session. Driving the `vibe-app-server-stdio-fixture`
//! binary through the same script with `--server` yields observations
//! normalized the same way, and the replay compares the two scenario by
//! scenario. It needs no reference checkout; only the live probe at the end,
//! which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: tool errors and displays, the
//! plan review, the sentences the loop answers an interrupted call and a
//! reviewed plan with, the plan-mode reminder and the checkpoints a reload
//! closes on.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, or a
//! `FIELDS` entry for a field that differs wherever it appears, and every
//! entry has to still reproduce, so row 34 of `docs/parity.md` is a reading
//! of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agent-loop-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/agent-loop-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/agent_loop.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The accepted divergence the row 34 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "The conversation loop's own sentences";

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[
    // ---------------------------------------------------------- row 3
    OwnProse {
        scenarios: &[
            "persist/tool-failure",
            "persist/reload",
            "persist/invalid-arguments",
            "approval/permanent",
        ],
        pointer: "/requests/1/conversation/2/content",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "a failed call answers the model under the reference's error tag and tool name, \
                 with the tool's own sentence: `read_file`'s missing file, `web_search`'s HTTP \
                 failure, and an argument rejection rendered by this port's schema validator \
                 where the reference prints pydantic's report",
    },
    // --------------------------------------------------------- row 13
    // --------------------------------------------------------- row 19
    // --------------------------------------------------------- row 26
    // --------------------------------------------------------- row 34
    OwnProse {
        scenarios: &["interrupt/during-tool", "interrupt/before-run"],
        pointer: "/requests/1/conversation/2/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: OWN_PROSE,
        reason: "a call the interrupt cut short, and one left without an answer, reads the \
                 reference's cancellation tag around this port's own sentence",
    },
    // ---------------------------------------------------------- row 12
];

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 runs this loop otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences an own-words entry covers but whose length v3.1.0 moved out of
/// its band, one by one; chained first, so they are decided before it.
const V3_REFUSED: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "interrupt/during-tool",
        pointer: "/requests/1/conversation/2/content",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "titles/after-compaction",
        pointer: "/requests/1/conversation/2/content",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*",
        count: 4,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/failure",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error/code",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error/data",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error/message",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/state/session/agent",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/state/session/contextUsage/cachedInputTokens",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/state/session/loops",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/state/session/tokenUsage/cachedInputTokens",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/requests/*/conversation/*",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/requests/*/conversation/*/content",
        count: 59,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/requests/*/conversation/*/role",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/requests/*/conversation/*/toolCallId",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/requests/*/tools/*",
        count: 140,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/params/contextWindow",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/params/stats/modelCalls",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/state/history/*/outcome",
        count: 7,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/method",
        count: 12,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/contextWindow",
        count: 142,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry",
        count: 5,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/content",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/content",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/display",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/hookName",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/input",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/kind",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/scope",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/status",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/toolCallId",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/detail/toolName",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/generationStatus",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/id",
        count: 4,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/inputEntryId",
        count: 6,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/level",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/message",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/outcome",
        count: 70,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/role",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/source",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/state",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/title",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/turnId",
        count: 4,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/type",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/userDisplayContent",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entryId",
        count: 7,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch",
        count: 7,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/stats",
        count: 8,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/stats/lastTurnCachedTokens",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/stats/modelCalls",
        count: 134,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/stats/sessionCachedTokens",
        count: 3,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turn",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turn/id",
        count: 2,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turn/inputEntryId",
        count: 6,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/turnId",
        count: 14,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/params/patch/*",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/params/patch/*/path",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/params/patch/*/value",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/state/history/*/content/*/resource/mediaType",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/entry/content/*/resource/mediaType",
        count: 1,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch/*",
        count: 69,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch/*/op",
        count: 60,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch/*/path",
        count: 97,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch/*/value",
        count: 98,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params/patch/*/value/cachedInputTokens",
        count: 58,
        row: "34",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

/// The accepted divergences the other rows' entries stand on.
const TOOL_RESULTS: &str = "Authored result and error text in six tools";

#[test]
fn the_port_runs_every_loop_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "agent loop parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-loop-parity-port.json"),
    }
    .run(
        entries(V3_REFUSED)
            .chain(entries(PROSE))
            .chain(entries(V3_GAPS)),
    );
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    // Row 34 is what this corpus measures, so a difference it keeps has to
    // stand on a divergence the scorecard decided to keep.
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(V3_REFUSED)
            .chain(entries(PROSE))
            .chain(entries(V3_GAPS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("agent loop");
}
