//! Replays the committed Unified Harness corpus against this port's app server.
//!
//! `scripts/parity/unified_harness.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`, which since v2.26.0 opens every session
//! on its Unified Harness: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, and records what
//! row 36 of `docs/parity.md` is about. The harness selection and its flags;
//! turns, tool calls and their approvals, smart approve, rewinds, background
//! titles, questions, steering and the queue; the turn preparation (skills,
//! mentioned files, the scratchpad); images for a model that reads them, one
//! that is described them and one that cannot; provider failures; code mode,
//! background processes and subagents with their saved sessions; sessions an
//! older Vibe saved, listed, read and imported; scheduled loops; compaction,
//! manual and automatic; and the unified session store across a restart.
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: the system prompt, the tool and
//! argument descriptions, the built-in skills, and the sentences the harness
//! writes into the conversation around a skill, the scratchpad, `/loop` and a
//! compaction.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, or a
//! `FIELDS` entry for a field that differs wherever it appears, and every
//! entry has to still reproduce, so row 36 of `docs/parity.md` is a reading
//! of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{Capture, Entry, Prose, Replay, entries, scenarios};

const CORPUS: &str = include_str!("unified-harness-parity/corpus.json");

/// The scorecard the accepted divergence below is decided in.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/unified_harness.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 91;

/// The accepted divergence every entry below stands on.
const OWN_PROSE: &str = "The Unified Harness's own prose";

/// A difference this port keeps in the scenarios named.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenarios.contains(&scenario) && pointer.starts_with(self.pointer)
    }

    fn label(&self) -> String {
        self.pointer.to_owned()
    }

    fn scenarios(&self) -> &[&str] {
        self.scenarios
    }
}

const SELECTIONS: &[&str] = &["selection/default", "selection/experimental-flag"];
const AUTO_COMPACTIONS: &[&str] = &[
    "compact/auto-empty",
    "compact/auto-failed",
    "compact/auto-midturn",
    "compact/auto-next-turn",
];

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: SELECTIONS,
        pointer: "/steps/2/response/result/runtime/skills/",
        reason: "the built-in skills' prompts, as `runtime/read` publishes them",
    },
    Divergence {
        scenarios: &["selection/legacy-flag"],
        pointer: "/steps/2/response/result/runtime/skills/",
        reason: "the built-in skills' prompts, as the legacy runtime publishes them",
    },
    Divergence {
        scenarios: &["mcp/runtime"],
        pointer: "/steps/1/response/result/runtime/skills/",
        reason: "the built-in skills' prompts, as a session with servers configured publishes them",
    },
    Divergence {
        scenarios: &["mcp/runtime"],
        pointer: "/steps/1/response/result/runtime/issues/0/message",
        reason: "a plugin server the discovery cannot launch is reported in this port's own words",
    },
    Divergence {
        scenarios: &["tools/skill"],
        pointer: "/requests/2/conversation/4/content",
        reason: "the skill tool's answer wraps the skill's content in the harness's own sentence",
    },
    Divergence {
        scenarios: &["tools/skill"],
        pointer: "/steps/0/turn/9/params/patch/7/value",
        reason: "the same answer, as the settled effect's output",
    },
    Divergence {
        scenarios: &["prepare/skills"],
        pointer: "/requests/",
        reason: "a skill a message invokes reaches the model inside the harness's own sentence, \
                 in that turn's request and every later one",
    },
    Divergence {
        scenarios: &["prepare/skills"],
        pointer: "/steps/",
        reason: "the same sentence, as the invocation effect's output, live and read back",
    },
    Divergence {
        scenarios: &["prepare/scratchpad"],
        pointer: "/requests/",
        reason: "the scratchpad restatement a turn opens with is the harness's own sentence",
    },
    Divergence {
        scenarios: &["prepare/scratchpad"],
        pointer: "/steps/",
        reason: "the same restatement, as the user entry carries it",
    },
    Divergence {
        scenarios: &["loops/slash-loop"],
        pointer: "/requests/",
        reason: "`/loop` hands the model the harness's own instructions",
    },
    Divergence {
        scenarios: &["session/compact", "titles/after-compaction"],
        pointer: "/requests/",
        reason: "a compaction asks for its summary and reopens the conversation in the \
                 harness's own words",
    },
    Divergence {
        scenarios: AUTO_COMPACTIONS,
        pointer: "/requests/",
        reason: "the same summary request and reopening, after an automatic compaction",
    },
];

/// A field this port answers differently wherever it appears, in any scenario.
struct FieldDivergence {
    /// Every difference whose JSON pointer ends with this suffix is covered.
    suffix: &'static str,
    reason: &'static str,
}

impl Entry for FieldDivergence {
    fn covers(&self, _scenario: &str, pointer: &str) -> bool {
        pointer.ends_with(self.suffix)
    }

    fn label(&self) -> String {
        self.suffix.to_owned()
    }
}

const FIELDS: &[FieldDivergence] = &[
    FieldDivergence {
        suffix: "/system/0/prose",
        reason: "the system prompt is this port's own wording of the same directives",
    },
    FieldDivergence {
        suffix: "/system/0/sha256",
        reason: "the same system prompt",
    },
    FieldDivergence {
        suffix: "/description/prose",
        reason: "a tool's, an argument's and a built-in skill's description",
    },
    FieldDivergence {
        suffix: "/description/sha256",
        reason: "the same descriptions",
    },
];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the mcp/* fixture servers are #! scripts CreateProcess cannot launch"
)]
fn the_port_runs_every_unified_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "unified harness parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("unified-harness-parity-port.json"),
        prose: Prose::Compared,
    }
    .run(entries(LEDGER).chain(entries(FIELDS)));
}

#[test]
fn every_ledger_entry_stands_on_the_accepted_divergence_and_a_recorded_scenario() {
    assert!(
        SCORECARD.contains(&format!("| {OWN_PROSE} |")),
        "docs/parity.md no longer accepts the divergence every ledger entry here stands on"
    );
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    for entry in LEDGER {
        assert!(
            !entry.scenarios.is_empty()
                && entry
                    .scenarios
                    .iter()
                    .all(|scenario| recorded.contains_key(*scenario))
                && entry.pointer.starts_with('/')
                && !entry.reason.is_empty(),
            "the ledger entry for {} needs recorded scenarios, a pointer prefix and a reason",
            entry.pointer
        );
    }
    for field in FIELDS {
        assert!(
            field.suffix.starts_with('/') && !field.reason.is_empty(),
            "the field entry for {} needs a pointer suffix and a reason",
            field.suffix
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("unified harness");
}
