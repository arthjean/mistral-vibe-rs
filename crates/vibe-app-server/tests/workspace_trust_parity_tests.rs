//! Replays the committed workspace trust corpus against this port's app server.
//!
//! `scripts/parity/workspace_trust.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, builds a workspace tree and, where it says so, a
//! `trusted_folders.toml` with a given content and mode, then drives
//! `workspace/trust/status`, `workspace/trust/decision`,
//! `workspace/trust/untrustedConfig`, `session/start` with and without
//! `trustWorkspace`, and `config/read` over a project file the trust store has
//! to allow. A scenario records every answer and refusal, the
//! `runtime/updated` a step raised, and the trust file's content and mode after
//! each step that asks for it. Driving the `vibe-app-server-stdio-fixture`
//! binary through the same script with `--server` yields observations
//! normalized the same way, and the replay compares the two scenario by
//! scenario. It needs no reference checkout; only the live probe at the end,
//! which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; everything else,
//! the trust file's bytes included, is compared exactly. Every other difference
//! has to fall under a `LEDGER` entry, and every entry has to still reproduce,
//! so row 12 of `docs/parity.md` is a reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("workspace-trust/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/workspace-trust/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/workspace_trust.py",
    family: &[],
    arguments: &[],
};

const LEDGER: &[PatternDivergence] = &[];

/// Every text this port words its own way, digest by digest.
/// The accepted divergence the prose below stands on.
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";

const PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &[
        "decision/refusals",
        "decision/trust-repository",
        "session/decision",
    ],
    pointer: "/*/response/error/message",
    count: 3,
    band: Band::DEFAULT,
    directives: None,
    row: "12",
    accepted: SERVER_PROSE,
    reason: "the refusal of a trust decision the folder or the session cannot take",
}];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the corpus records POSIX file modes and a directory name with a double quote, which NTFS forbids"
)]
fn the_port_answers_every_trust_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "workspace trust parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("workspace-trust-port.json"),
    }
    .run(entries(PROSE).chain(entries(LEDGER)));
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(LEDGER)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("workspace trust");
}
