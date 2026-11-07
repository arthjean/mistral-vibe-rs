//! Replays the committed MCP catalog corpus against this port's app server.
//!
//! `scripts/parity/mcp_catalog.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, with `vibe-oauth-script-fixture` playing each configured MCP
//! server and its authorization server, a credential store kept in one JSON
//! file, and a simulated browser answering each authorization URL. A scenario
//! records every `mcp_catalog/*` and `mcp/*` answer with the notifications
//! around it, the `mcp_servers` the user and project files hold afterward, the
//! credential store, and every request the MCP servers received. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; everything else is
//! compared exactly. Every other difference has to fall under a `LEDGER`
//! entry, and every entry has to still reproduce, so row 11 of
//! `docs/parity.md` is a reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("mcp-catalog/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/mcp-catalog/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/mcp_catalog.py",
    family: &[],
    arguments: &["--fixture", env!("CARGO_BIN_EXE_vibe-oauth-script-fixture")],
};

const LEDGER: &[PatternDivergence] = &[];

/// Every text this port words its own way, digest by digest.
/// The accepted divergences the prose below stands on.
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";
const PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &[
        "session/auth-required-then-login",
        "session/sessionless-mutations-conflict",
        "session/toggle",
        "sessionless/add",
        "sessionless/add-refusals",
        "sessionless/login-refusals",
        "sessionless/toggle",
    ],
    pointer: "/*/response/error/message",
    count: 18,
    band: Band::DEFAULT,
    directives: None,
    row: "11",
    accepted: SERVER_PROSE,
    reason: "the refusal of a catalog change: a server to sign in first, a conflicting sessionless mutation, a malformed entry",
}];

#[test]
fn the_port_serves_every_catalog_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "MCP catalog parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("mcp-catalog-port.json"),
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
    CAPTURE.probe("MCP catalog");
}
