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
    Band, Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
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
/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 serves the MCP catalog otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/*/keyring/mcp-oauth:secure:client_info/application_type",
        count: 2,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/keyring/mcp-oauth:secure:client_info/issuer",
        count: 2,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error",
        count: 1,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error/data",
        count: 3,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result",
        count: 1,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/mcp/connectorError",
        count: 5,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/mcp/privateConnectorsHidden",
        count: 5,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/runtime/mcp/connectorError",
        count: 13,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/runtime/mcp/privateConnectorsHidden",
        count: 13,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*",
        count: 1,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*/mcp",
        count: 10,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*/mcp/connectorError",
        count: 13,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*/mcp/privateConnectorsHidden",
        count: 13,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*/method",
        count: 10,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*/params",
        count: 10,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/mcp/sources/*/tools/*/description",
        count: 6,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/mcp/sources/*/tools/*/name",
        count: 6,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/runtime/mcp/sources/*/tools/*",
        count: 6,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/notifications/*/mcp/sources/*/tools/*",
        count: 6,
        row: "11",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

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
    count: 21,
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
    .run(
        entries(PROSE)
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE)
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("MCP catalog");
}
