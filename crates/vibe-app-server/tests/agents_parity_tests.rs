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
use vibe_core::parity::replay::{Capture, Divergence, Prose, Replay, entries, scenarios};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agents-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/agents.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 38;

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

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-clear",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/0/conversation/1/content",
        row: "34",
        reason: "the reminder a plan session injects before its first request is this port's own \
                 prose (`NOTICE`), naming the same plan file, the same read-only rule and the same \
                 two tools as reference `ReadOnlyAgentMiddleware`",
    },
    Divergence {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/1/conversation/1/content",
        row: "34",
        reason: "the same reminder, as the next request resends it",
    },
    Divergence {
        scenarios: &["profile/builtin-tools"],
        pointer: "/requests/2/conversation/1/content",
        row: "34",
        reason: "the same reminder, in the third request",
    },
    Divergence {
        scenarios: &[
            "profile/builtin-tools",
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
        ],
        pointer: "/requests/1/conversation/4/content",
        row: "34",
        reason: "the reminder a session reads once it leaves plan mode is this port's own prose",
    },
    Divergence {
        scenarios: &["profile/builtin-tools"],
        pointer: "/requests/2/conversation/4/content",
        row: "34",
        reason: "the same leaving reminder, as the third request resends it",
    },
    Divergence {
        scenarios: &[
            "switch/exit-plan-auto",
            "switch/exit-plan-manual",
            "switch/exit-plan-stay",
        ],
        pointer: "/requests/1/conversation/3/content",
        row: "3",
        reason: "`exit_plan_mode` writes its own sentence for each plan-review outcome, as the \
                 model reads the result",
    },
    Divergence {
        scenarios: TASK_REFUSALS,
        pointer: "/requests/1/conversation/2/content",
        row: "3",
        reason: "`task` refuses an agent the session is not offered and a primary agent in its \
                 own sentences, as the model reads them",
    },
    Divergence {
        scenarios: TASK_REFUSALS,
        pointer: "/steps/1/turn/4/patch/0/value/display/message",
        row: "3",
        reason: "the same refusal, as the failed effect's header",
    },
    Divergence {
        scenarios: TASK_REFUSALS,
        pointer: "/steps/1/turn/4/patch/0/value/error/message",
        row: "3",
        reason: "the same refusal, as the failed effect's error",
    },
    Divergence {
        scenarios: &["delegation/depth"],
        pointer: "/requests/2/conversation/2/content",
        row: "3",
        reason: "a subagent that asks for `task` is refused in this port's own sentence",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/3/detail/display/statusText",
        row: "3",
        reason: "`exit_plan_mode`'s status line is this port's own words",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/4/patch/0/value/display/",
        row: "3",
        reason: "`exit_plan_mode`'s call header, the same verbs over this port's own summary, \
                 message and status line",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/6/detail/request/",
        row: "3",
        reason: "the plan review asks its question, describes its four options and names the \
                 plan file in this port's own words; the option labels, which a client answers \
                 with, are the reference's",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/8/patch/0/value/output/result/answers/0/question",
        row: "3",
        reason: "the same question, as the answered callback repeats it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/10/patch/0/value/display/message",
        row: "3",
        reason: "the plan-review outcome, as the settled effect's header",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/turn/10/patch/0/value/output/message",
        row: "3",
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
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("agents-parity-port.json"),
        prose: Prose::Compared,
    }
    .run(entries(LEDGER));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    Divergence::assert_ledger(LEDGER, SCORECARD, &scenarios(&corpus), "20", None);
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("agents");
}
