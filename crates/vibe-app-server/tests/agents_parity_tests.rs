//! Replays the committed agents corpus against this port's app server.
//!
//! `scripts/parity/agents.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, lays out the agent
//! files it declares and drives the methods and turns row 20 of
//! `docs/parity.md` is about. `registry` records which profiles a server
//! discovers and publishes on `agents/list` and `runtime/read`, and what a
//! legacy profile file reads as once migrated; `selection` which profiles a
//! session may start under or switch to, and the refusals it answers with;
//! `profile` what a profile changes in the requests a session sends;
//! `delegation` a `task` call as the parent's client sees it, as the child
//! runs it and as both are saved; `switch` the profile `exit_plan_mode` moves
//! a session to and when the model starts running under it. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: the plan-mode reminders, the
//! sentences `task` and `exit_plan_mode` answer with, the plan review's
//! question and headers, and the notices a turn publishes.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 20 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agents-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/agents-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/agents.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

const PLAN_REVIEWS: &[&str] = &[
    "switch/exit-plan-auto",
    "switch/exit-plan-manual",
    "switch/exit-plan-clear",
    "switch/exit-plan-stay",
];

const TASK_REFUSALS: &[&str] = &[
    "delegation/unknown-agent",
    "delegation/primary-agent",
    "delegation/disabled-subagent",
];

/// The accepted divergence the row 34 sentences stand on.
const LOOP_SENTENCES: &str = "The conversation loop's own sentences";
/// The accepted divergence the row 3 tool results stand on.
const TOOL_RESULTS: &str = "Authored result and error text in six tools";
/// The accepted divergence the plan review's call display stands on.
const CALL_DISPLAY: &str = "Authored call-display text in the plan and fetch tools";

/// A subagent that asks for `task` is refused in one short sentence where
/// the reference explains the depth limit at 2.7 times the length.
const DEPTH_BAND: Band = Band {
    low: 0.3,
    high: 2.0,
};

/// `task` names the agent it refuses and the agents a session may delegate
/// to, about 2.3 times the reference's sentence, as the failed effect shows.
const REFUSAL_BAND: Band = Band {
    low: 0.5,
    high: 2.5,
};

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-clear",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/0/conversation/1/content",
        count: 5,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the reminder a plan session injects before its first request is this port's own \
                 prose (`NOTICE`), naming the same plan file, the same read-only rule and the same \
                 two tools as reference `ReadOnlyAgentMiddleware`",
    },
    OwnProse {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/1/conversation/1/content",
        count: 4,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the same reminder, as the next request resends it",
    },
    OwnProse {
        scenarios: &["profile/builtin-tools"],
        pointer: "/requests/2/conversation/1/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the same reminder, in the third request",
    },
    OwnProse {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
        ],
        pointer: "/requests/1/conversation/4/content",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the reminder a session reads once it leaves plan mode is this port's own prose",
    },
    OwnProse {
        scenarios: &["profile/builtin-tools"],
        pointer: "/requests/2/conversation/4/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the same leaving reminder, as the third request resends it",
    },
    OwnProse {
        scenarios: &[
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/1/conversation/3/content",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "`exit_plan_mode` writes its own sentence for each plan-review outcome, as the \
                 model reads the result",
    },
    OwnProse {
        scenarios: TASK_REFUSALS,
        pointer: "/requests/1/conversation/2/content",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "`task` refuses an agent the session is not offered and a primary agent in its \
                 own sentences, as the model reads them",
    },
    OwnProse {
        scenarios: TASK_REFUSALS,
        pointer: "/steps/1/turn/4/patch/0/value/display/message",
        count: 3,
        band: REFUSAL_BAND,
        directives: None,
        row: "3",
        accepted: CALL_DISPLAY,
        reason: "the same refusal, as the failed effect's header",
    },
    OwnProse {
        scenarios: TASK_REFUSALS,
        pointer: "/steps/1/turn/4/patch/0/value/error/message",
        count: 3,
        band: REFUSAL_BAND,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same refusal, as the failed effect's error",
    },
    OwnProse {
        scenarios: &["delegation/depth"],
        pointer: "/requests/2/conversation/2/content",
        count: 1,
        band: DEPTH_BAND,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "a subagent that asks for `task` is refused in this port's own sentence",
    },
    OwnProse {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/3/detail/display/statusText",
        count: 4,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: CALL_DISPLAY,
        reason: "`exit_plan_mode`'s status line is this port's own words",
    },
    OwnProse {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/4/patch/0/value/display/**",
        count: 16,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: CALL_DISPLAY,
        reason: "`exit_plan_mode`'s call header, the same verbs over this port's own summary, \
                 message and status line",
    },
    OwnProse {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/6/detail/request/**",
        count: 24,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: CALL_DISPLAY,
        reason: "the plan review asks its question, describes its four options and names the \
                 plan file in this port's own words; the option labels, which a client answers \
                 with, are the reference's",
    },
    OwnProse {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/8/patch/0/value/output/result/answers/0/question",
        count: 4,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same question, as the answered callback repeats it",
    },
    OwnProse {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/10/patch/0/value/display/message",
        count: 4,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: CALL_DISPLAY,
        reason: "the plan-review outcome, as the settled effect's header",
    },
    OwnProse {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/10/patch/0/value/output/message",
        count: 4,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the plan-review outcome, as the settled effect's output",
    },
];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: on win32 the reference records a .last_session pointer (switch/exit-plan-clear) that a Linux capture never holds"
)]
fn the_port_runs_every_agent_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "agents parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("agents-parity-port.json"),
    }
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("agents");
}
