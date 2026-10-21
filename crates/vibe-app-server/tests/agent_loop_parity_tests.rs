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
    Capture, Divergence, FieldDivergence, Prose, Replay, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agent-loop-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/agent_loop.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 33;

/// The accepted divergence the row 34 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "The conversation loop's own sentences";

const PLAN_REVIEWS: &[&str] = &["plan/edited-during-review", "plan/unchanged-during-review"];

const LEDGER: &[Divergence] = &[
    // ---------------------------------------------------------- row 3
    Divergence {
        scenarios: &[
            "persist/tool-failure",
            "persist/reload",
            "persist/invalid-arguments",
            "approval/permanent",
        ],
        pointer: "/requests/1/conversation/2/content",
        row: "3",
        reason: "a failed call answers the model under the reference's error tag and tool name, \
                 with the tool's own sentence: `read_file`'s missing file, `web_search`'s HTTP \
                 failure, and an argument rejection rendered by this port's schema validator \
                 where the reference prints pydantic's report",
    },
    Divergence {
        scenarios: &["persist/tool-failure", "persist/invalid-arguments"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "3",
        reason: "the same failure, as the saved tool message",
    },
    Divergence {
        scenarios: &["persist/tool-failure"],
        pointer: "/steps/0/turn/9/params/patch/0/value/",
        row: "3",
        reason: "the same failure, as the settled effect's header and error",
    },
    Divergence {
        scenarios: &["persist/invalid-arguments"],
        pointer: "/steps/0/turn/8/params/patch/0/value/",
        row: "3",
        reason: "the same rejection, as the settled effect's header and error",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/0/turn/13/params/patch/0/value/",
        row: "3",
        reason: "the same failure, as the live effect's header and error",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/1/response/result/state/history/3/state/",
        row: "3",
        reason: "the same failure, as the effect the resumed session rebuilds from its log",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/2/response/result/history/3/state/",
        row: "3",
        reason: "the same failure, as `session/history/get` reads it back",
    },
    Divergence {
        scenarios: &["approval/permanent"],
        pointer: "/steps/0/turn/15/params/patch/0/value/",
        row: "3",
        reason: "the same `web_search` failure, as the settled effect's header and error",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/requests/1/conversation/3/content",
        row: "3",
        reason: "`exit_plan_mode` writes its own sentence for each plan-review outcome, as the \
                 model reads the result",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/8/params/entry/detail/display/",
        row: "3",
        reason: "`exit_plan_mode`'s status line is this port's own words",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/9/params/patch/0/value/display/",
        row: "3",
        reason: "`exit_plan_mode`'s call header, the same verbs over this port's own summary, \
                 message and status line",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/12/params/entry/detail/request/",
        row: "3",
        reason: "the plan review asks its question, describes its four options and names the \
                 plan file in this port's own words; the option labels, which a client answers \
                 with, are the reference's",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/15/params/patch/0/value/output/result/answers/0/question",
        row: "3",
        reason: "the same question, as the answered callback repeats it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/18/params/patch/0/value/",
        row: "3",
        reason: "the plan-review outcome, as the settled effect's header and output",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/persisted/0/messages/2/message/tool_calls/0/presentation/display/",
        row: "3",
        reason: "`exit_plan_mode`'s call display, as the saved call records it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/persisted/0/messages/3/message/",
        row: "3",
        reason: "the plan-review outcome, as the saved tool message, its result and its display",
    },
    // --------------------------------------------------------- row 13
    Divergence {
        scenarios: &["persist/denied"],
        pointer: "/steps/0/turn/15/params/patch/0/value/display/message",
        row: "13",
        reason: "a declined call's display is this port's own sentence",
    },
    Divergence {
        scenarios: &["persist/denied"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "13",
        reason: "a declined call answers the model under the reference's cancellation tag with \
                 this port's own sentence",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/1/response/result/state/history/5/message",
        row: "13",
        reason: "the checkpoint a resumed session closes its history on is this port's own \
                 sentence",
    },
    Divergence {
        scenarios: &["persist/resource-and-display"],
        pointer: "/steps/2/response/result/state/history/2/message",
        row: "13",
        reason: "the same resume checkpoint",
    },
    // --------------------------------------------------------- row 19
    Divergence {
        scenarios: &["titles/after-compaction"],
        pointer: "/requests/1/conversation/2/content",
        row: "19",
        reason: "the summarization request carries this port's own compaction instructions",
    },
    // --------------------------------------------------------- row 26
    Divergence {
        scenarios: &["titles/after-compaction"],
        pointer: "/steps/3/persisted/0/messages/2/message/content",
        row: "26",
        reason: "the compaction envelope's two prose runs are this port's own",
    },
    // --------------------------------------------------------- row 34
    Divergence {
        scenarios: &["interrupt/during-tool", "interrupt/before-run"],
        pointer: "/requests/1/conversation/2/content",
        row: "34",
        reason: "a call the interrupt cut short, and one left without an answer, reads the \
                 reference's cancellation tag around this port's own sentence",
    },
    Divergence {
        scenarios: &["interrupt/during-tool"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "34",
        reason: "the same interrupted answer, as the turn saves it",
    },
    Divergence {
        scenarios: &["interrupt/during-tool"],
        pointer: "/steps/3/persisted/0/messages/2/message/content",
        row: "34",
        reason: "the same interrupted answer, as the next turn leaves it",
    },
    Divergence {
        scenarios: &["interrupt/before-run"],
        pointer: "/steps/4/persisted/0/messages/2/message/content",
        row: "34",
        reason: "the answer the next turn fills a call left unanswered with",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/requests/0/conversation/1/content",
        row: "34",
        reason: "the reminder a plan session injects before its first request is this port's own \
                 prose, naming the same plan file, the same read-only rule and the same two \
                 tools as reference `ReadOnlyAgentMiddleware`",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/requests/1/conversation/1/content",
        row: "34",
        reason: "the same reminder, as the next request resends it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/persisted/0/messages/1/message/content",
        row: "34",
        reason: "the same reminder, as the session saves it",
    },
    Divergence {
        scenarios: &["plan/edited-during-review"],
        pointer: "/requests/1/conversation/4/content",
        row: "34",
        reason: "a plan edited by hand during its review reaches the model in a warning of this \
                 port's own words, under the reference's tag and over the same plan text",
    },
    Divergence {
        scenarios: &["plan/edited-during-review"],
        pointer: "/steps/1/persisted/0/messages/4/message/content",
        row: "34",
        reason: "the same warning, as the session saves it",
    },
    // ---------------------------------------------------------- row 12
    Divergence {
        scenarios: &["approval/path-scope-not-offered"],
        pointer: "/steps/0/turn/18/params/turn/error/message",
        row: "12",
        reason: "the refusal of a path scope the approval did not offer is this port's own \
                 sentence (`PathScopeRefusal`), which `NOTICE` keeps from reproducing \
                 `approval_grant_permissions` (`vibe/app_server/_approval_permissions.py:55` \
                 at 376f6a3); the turn fails with the same code",
    },
    Divergence {
        scenarios: &["approval/path-scope-not-offered"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "12",
        reason: "the same refusal, as the failed call's answer carries it inside the tool \
                 error tag the reference wraps it in",
    },
];

const FIELDS: &[FieldDivergence] = &[];

#[test]
fn the_port_runs_every_loop_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "agent loop parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-loop-parity-port.json"),
        prose: Prose::Compared,
    }
    .run(entries(LEDGER).chain(entries(FIELDS)));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    // Row 34 is what this corpus measures, so a difference it keeps is a gap
    // unless the scorecard decided to keep it.
    Divergence::assert_ledger(
        LEDGER,
        SCORECARD,
        &scenarios(&corpus),
        "34",
        Some(OWN_PROSE),
    );
    FieldDivergence::assert_ledger(FIELDS, SCORECARD, "34");
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("agent loop");
}
