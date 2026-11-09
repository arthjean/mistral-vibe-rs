//! Replays the committed system prompt corpus against this port.
//!
//! `scripts/parity/system_prompt.py` captured the corpus from the pinned
//! reference's `vibe -p`: every scenario lays a tree out (a home whose `.vibe`
//! is the vibe home, a workspace, Git repositories, skills, agents, prompt
//! files and `AGENTS.md` documents), runs a headless session there behind a
//! scripted chat-completions endpoint, and records what the system messages
//! of every request carry: how many there are, whether one leads, which of the
//! scenario's data they mention, and the prompt as a length and a SHA-256.
//! The replay runs this crate's `vibe` through the script's `--live-binary`
//! and records the same.
//!
//! The port writes its own prose on purpose (`NOTICE`): two digests that differ
//! are a difference unless a `PROSE` entry names their pointer, holds the count
//! of digests it covers, and finds the port's length within its band of the
//! reference's; everything else is compared exactly. A difference has to fall
//! under a ledger entry or the corpus's measured debt
//! (`vibe_core::parity::debt`), and every entry and every debt group has to
//! still reproduce, so row 19 of `docs/parity.md` is a reading of the summary
//! this file prints. Only the live probe at the end needs the reference
//! checkout.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Tally, assert_ledger, differences, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("system-prompt-parity/corpus.json");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/system_prompt.py",
    family: &[],
    arguments: &[],
};

/// The accepted divergence the prose below stands on.
const PROMPT_TEXT: &str = "System prompt text";

/// The reference commit whose prompt directives this port's prompt was last
/// read against; a re-pin re-reads them before restating it. At `c96a6db` the
/// read found four directives the prompt does not cover yet, which
/// `docs/parity.md` lists as an open divergence of row 19.
const AUDITED: &str = "c96a6dbb34d3";

const PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &["*"],
    pointer: "/requests/*/prompt",
    count: 45,
    band: Band::DEFAULT,
    directives: Some(AUDITED),
    row: "19",
    accepted: PROMPT_TEXT,
    reason: "the prompt a session sends, which this port writes in its own words over the same directives",
}];

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-cli/tests/system-prompt-parity/corpus.json";
/// The port's requests, captured by the script driving this crate's `vibe`.
fn replay_live() -> BTreeMap<String, Value> {
    let captured = CAPTURE.port(
        &["--live-binary", env!("CARGO_BIN_EXE_vibe")],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("system-prompt-live-port.json"),
    );
    scenarios(&captured)
        .into_iter()
        .map(|(name, entry)| (name, entry["observed"].clone()))
        .collect()
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the corpus was captured on Linux, and the reference prompt names the Windows platform and spells paths with backslashes"
)]
fn the_port_composes_every_system_prompt_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let mut tally = Tally::new(CORPUS, entries(PROSE));
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    let live = replay_live();
    for (name, reference) in &recorded {
        let port = live
            .get(name)
            .cloned()
            .expect("the live capture holds every scenario");
        let found = differences(&reference["observed"], &port);
        tally.record(name, &reference["observed"], &port, found);
    }
    tally.conclude("system prompt parity:");
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("system prompt");
}
