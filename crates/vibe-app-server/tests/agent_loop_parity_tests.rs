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
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), or a `FIELDS` entry for
//! a field that differs wherever it appears, and every entry and every debt
//! group has to still reproduce, so row 34 of `docs/parity.md` is a reading of
//! the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Replay, assert_ledger, entries, scenarios,
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
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    // Row 34 is what this corpus measures, so a difference it keeps has to
    // stand on a divergence the scorecard decided to keep.
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("agent loop");
}
