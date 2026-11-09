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
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), or a `FIELDS` entry for
//! a field that differs wherever it appears, and every entry and every debt
//! group has to still reproduce, so row 36 of `docs/parity.md` is a reading of
//! the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::debt::Debt;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Replay, assert_ledger, differences, entries, scenarios,
};

const CORPUS: &str = include_str!("unified-harness-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/unified-harness-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/unified_harness.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The accepted divergence every entry below stands on.
const OWN_PROSE: &str = "The Unified Harness's own prose";

const SELECTIONS: &[&str] = &["selection/default", "selection/experimental-flag"];

/// The unified system prompt states the harness's own sections in full,
/// about 3.9 times the reference's.
const SYSTEM_BAND: Band = Band {
    low: 0.5,
    high: 4.5,
};

/// The built-in skills' prompts are this port's shorter bodies, about a
/// quarter of the reference's.
const SKILL_PROMPT_BAND: Band = Band {
    low: 0.2,
    high: 2.0,
};

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: SELECTIONS,
        pointer: "/steps/2/response/result/runtime/skills/**",
        count: 7,
        band: SKILL_PROMPT_BAND,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "the built-in skills' prompts, as `runtime/read` publishes them",
    },
    OwnProse {
        scenarios: &["mcp/runtime"],
        pointer: "/steps/1/response/result/runtime/skills/**",
        count: 7,
        band: SKILL_PROMPT_BAND,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "the built-in skills' prompts, as a session with servers configured publishes them",
    },
    OwnProse {
        scenarios: &["mcp/runtime"],
        pointer: "/steps/1/response/result/runtime/issues/0/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "a plugin server the discovery cannot launch is reported in this port's own words",
    },
    OwnProse {
        scenarios: &["tools/skill"],
        pointer: "/requests/2/conversation/4/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "the skill tool's answer wraps the skill's content in the harness's own sentence",
    },
    OwnProse {
        scenarios: &["tools/skill"],
        pointer: "/steps/0/turn/9/params/patch/7/value",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "the same answer, as the settled effect's output",
    },
    OwnProse {
        scenarios: &["prepare/skills"],
        pointer: "/requests/*/conversation/*/content",
        count: 5,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "a skill a message invokes reaches the model inside the harness's own sentence, \
                 in that turn's request and every later one",
    },
    OwnProse {
        scenarios: &["prepare/skills"],
        pointer: "/steps/*/turn/*/params/entry/state/outputText",
        count: 2,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "the same sentence, as the invocation effect's output when it settles",
    },
    OwnProse {
        scenarios: &["prepare/skills"],
        pointer: "/steps/*/response/result/state/history/*/state/outputText",
        count: 2,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "the same sentence, as the session's history reads it back",
    },
    OwnProse {
        scenarios: &["loops/slash-loop"],
        pointer: "/requests/*/conversation/*/content",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "`/loop` hands the model the harness's own instructions",
    },
    OwnProse {
        scenarios: &["session/compact", "titles/after-compaction"],
        pointer: "/requests/*/conversation/*/content",
        count: 2,
        band: Band::DEFAULT,
        directives: None,
        row: "36",
        accepted: OWN_PROSE,
        reason: "a compaction asks for its summary and reopens the conversation in the \
                 harness's own words",
    },
];

/// The reference commit the system prompt's directives were last read at:
/// the v3.1.0 read is the one row 19 records, with the directives this port's
/// prompt does not cover yet listed as its open divergence. The tool, argument
/// and skill descriptions were not re-read at v3.1.0, so their differences are
/// gaps of this row, in the corpus's measured debt, until they are.
const SYSTEM_AUDITED: &str = "c96a6dbb34d3";

/// The prose every request carries, wherever it appears.
const FIELDS: &[OwnProse] = &[OwnProse {
    scenarios: &["*"],
    pointer: "/**/system/0",
    count: 213,
    band: SYSTEM_BAND,
    directives: Some(SYSTEM_AUDITED),
    row: "36",
    accepted: OWN_PROSE,
    reason: "the system prompt is this port's own wording of the same directives",
}];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the mcp/* fixture servers are #! scripts CreateProcess cannot launch"
)]
fn the_port_runs_every_unified_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "unified harness parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("unified-harness-parity-port.json"),
    }
    .run(entries(PROSE).chain(entries(FIELDS)));
}

/// The scenarios Windows replays: every one but the `mcp/*` family, whose
/// fixture servers are `#!` scripts `CreateProcess` cannot launch.
#[test]
#[cfg_attr(
    not(windows),
    ignore = "the replay above runs every scenario on this host"
)]
fn windows_runs_every_unified_scenario_but_the_mcp_family() {
    const LEFT_OUT: &str = "mcp/";
    /// The observations that carry this host's own paths in a form no
    /// normalization can restore, each as (scenario, pointer, why). Every one
    /// has to still differ here, so an entry that stops applying fails the run.
    const HOST_BOUND: &[(&str, &str, &str)] = &[(
        "prepare/skills",
        "/steps/5/persisted/unified/0/projection/snapshot/session/preview",
        "the persisted preview is the first 160 raw characters of a message that names the \
         skill's directory (reference `vibe/app_server/_projector.py`), so a longer temporary \
         root moves the cut before the root is replaced",
    )];
    let mut host_bound_seen = vec![0_usize; HOST_BOUND.len()];
    // The ledger's counts hold over the whole corpus, which the replay above
    // measures on Linux; with the MCP family left out they cannot hold here.
    // So this run asks of every difference what `Tally::record` asks first:
    // that the first entry covering it also admits it, or else that the
    // measured debt holds it. The debt counts per scenario, so its groups
    // outside the MCP family still hold exactly.
    let ledger = entries(PROSE).chain(entries(FIELDS)).collect::<Vec<_>>();
    let mut debt = Debt::load(LISTED, "unified harness parity:");
    let mut unexplained = Vec::new();
    let mut compared = 0_usize;
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let replayed = CAPTURE.port(
        &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
            "--exclude",
            LEFT_OUT,
        ],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("unified-harness-parity-windows.json"),
    );
    let replayed = scenarios(&replayed);
    for (name, reference) in scenarios(&corpus) {
        if name.starts_with(LEFT_OUT) {
            continue;
        }
        let port = replayed
            .get(&name)
            .expect("the port capture holds every scenario the corpus records");
        assert_eq!(
            reference["scenario"], port["scenario"],
            "the capture script changed {name} since the corpus was recorded"
        );
        compared += 1;
        for pointer in differences(&reference["observed"], &port["observed"]) {
            if let Some(index) = HOST_BOUND
                .iter()
                .position(|(scenario, bound, _)| *scenario == name && *bound == pointer)
            {
                host_bound_seen[index] += 1;
                continue;
            }
            let left = reference["observed"].pointer(&pointer);
            let right = port["observed"].pointer(&pointer);
            let verdict = ledger
                .iter()
                .find(|entry| entry.covers(&name, &pointer))
                .map(|entry| entry.admit(left, right));
            match verdict {
                Some(Ok(())) => {}
                _ if debt.absorb(&name, &pointer) => {}
                Some(Err(why)) => unexplained.push(format!("{name} {pointer} ({why})")),
                None => unexplained.push(format!("{name} {pointer}")),
            }
        }
    }
    println!(
        "unified harness parity on windows: {compared} scenarios compared outside {LEFT_OUT}*, \
         {} differences in the measured debt, {} no entry admits",
        debt.covered(),
        unexplained.len()
    );
    assert!(
        unexplained.is_empty(),
        "the port departs from the corpus where neither a ledger entry nor the measured debt \
         admits it:\n{}",
        unexplained.join("\n")
    );
    let stale = HOST_BOUND
        .iter()
        .zip(&host_bound_seen)
        .filter(|(_, seen)| **seen != 1)
        .map(|((scenario, pointer, why), seen)| {
            format!("{scenario} {pointer}: seen {seen} times ({why})")
        })
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these host-bound observations no longer differ exactly once:\n{}",
        stale.join("\n")
    );
    debt.retain_compared(|scenario| !scenario.starts_with(LEFT_OUT));
    debt.assert_settled();
}

#[test]
fn every_ledger_entry_stands_on_the_accepted_divergence_and_a_recorded_scenario() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(FIELDS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("unified harness");
}
