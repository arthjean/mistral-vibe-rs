//! Replays the committed connectors corpus against this port's app server.
//!
//! `scripts/parity/connectors.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the Mistral API that answers the
//! connector bootstrap, the authorization pages and the connector gateway, and
//! drives the methods and turns row 21 of `docs/parity.md` is about. `catalog`
//! records what `connector_catalog/*` answers with and without a session;
//! `cache` how the bootstrap cache is read, trusted and rewritten; `publish`
//! which connector tools a session publishes to its runtime and to the model;
//! `call` a connector tool call as the gateway, the model and the client see
//! it; `lifecycle` how a session's catalog moves under refreshes, toggles,
//! running turns and authorization requests. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing, the sentences a failed gateway
//! call tells the model, and the turn identifier a refusal names.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 21 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Class, Divergence, OwnProse, PatternDivergence, Replay, Tally, assert_ledger,
    differences, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("connectors-parity/corpus.json");

/// The corpora as `scripts/parity/manifest.json` lists them.
const LISTED: &str = "crates/vibe-app-server/tests/connectors-parity/corpus.json";
const UNIFIED_LISTED: &str = "crates/vibe-app-server/tests/connectors-parity/unified-corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/connectors.py",
    family: &[],
    arguments: JOBS,
};

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &[&str] = &["--jobs", "4"];

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 serves connectors otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/bootstraps/*",
        count: 14,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<cache-key-1>",
        count: 8,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>",
        count: 5,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/other-stale",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error",
        count: 5,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/error/message",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result",
        count: 5,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/acceptedSelectionRevision",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/catalogRevision",
        count: 12,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/disposition",
        count: 10,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalogRevision",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/counts/connected",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/pendingSelection",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/privateConnectorsHidden",
        count: 34,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/routeRevision",
        count: 10,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/connectors/connected",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/discoveryErrors/Diag",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/discoveryErrors/Diag_text",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/discoveryErrors/Sheets",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/privateConnectorsHidden",
        count: 28,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/selectionRevision",
        count: 8,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/acceptedCatalogRevision",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/acceptedSelectionRevision",
        count: 7,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/routeRevision",
        count: 13,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*/display_name",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*/id",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*/name",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/runtime/connectors/connected",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/runtime/mcp/privateConnectorsHidden",
        count: 12,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/connectors/*",
        count: 19,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/connectors/*/alias",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/connectors/*/displayName",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/sources/*/status",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/selections/*/disabled",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/selections/*/state",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/alias",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/displayName",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/status",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/siblings/*",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/entryId",
        count: 8,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/method",
        count: 6,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/patch",
        count: 6,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime",
        count: 3,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime/mcp/discoveryErrors/Diag",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime/mcp/discoveryErrors/Diag_text",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime/mcp/discoveryErrors/Sheets",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime/mcp/privateConnectorsHidden",
        count: 20,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/sessionId",
        count: 3,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/status",
        count: 3,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*/tools/*",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*/tools/*/description",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/cacheFile/<fingerprint>/payload/connectors/*/tools/*/name",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/runtime/mcp/sources/*/status",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/connectors/*/tools/*",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/connectors/*/tools/*/description",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/catalog/connectors/*/tools/*/name",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/sources/*/tools/*/enabled",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/tools/*",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/tools/*/description",
        count: 10,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/tools/*/enabled",
        count: 13,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/session/sources/*/tools/*/name",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/patch/*",
        count: 24,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/notifications/*/runtime/mcp/sources/*/tools/*/enabled",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const UNIFIED_V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/authUrls/*",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/callbacks/*/effect/mcpApp",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/response/result/runtime/mcp/privateConnectorsHidden",
        count: 4,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/requests/*/conversation/*/content",
        count: 3,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*",
        count: 3,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/entry/detail/mcpApp",
        count: 9,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/entryId",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/method",
        count: 3,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/params",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/patch",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/runtime/mcp/privateConnectorsHidden",
        count: 9,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/sessionId",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/status",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/patch/*",
        count: 8,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/patch/*/value/error/message",
        count: 1,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/steps/*/turn/*/patch/*/value/message",
        count: 2,
        row: "21",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

const PROSE: &[OwnProse] = &[];

#[test]
fn the_port_runs_every_connector_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "connectors parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("connectors-parity-port.json"),
    }
    .run(entries(PROSE).chain(entries(V3_GAPS)));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    // Row 21 is what this corpus measures, so a difference it keeps has to
    // stand on a divergence the scorecard decided to keep.
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(V3_GAPS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("connectors");
}

/// The `--family programs` scenarios: the same catalog
/// and gateway reached from `run_typescript`, row 36's measure of the
/// connector path.
const UNIFIED_CORPUS: &str = include_str!("connectors-parity/unified-corpus.json");

const UNIFIED_CAPTURE: Capture = Capture {
    script: "scripts/parity/connectors.py",
    family: &["--family", "programs"],
    arguments: JOBS,
};

/// The accepted divergence the unified entries below stand on.
const REJECTED_MIDTURN: &str = "A connector the gateway rejects mid-turn keeps its turn";

const UNIFIED_LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: &["unified/gateway-401"],
        pointer: "/requests/1/conversation/2/content",
        within: &[],
        count: 1,
        row: "36",
        class: Class::Accepted(REJECTED_MIDTURN),
        reason: "the reference suspends the rejected connector by reconfiguring its core while \
                 the turn runs, which its core refuses, so the program reads                  `effect_recovery_failed`; this port reads the gateway's own failure",
    },
    Divergence {
        scenarios: &["unified/gateway-401"],
        pointer: "/steps/1/turn/2/patch",
        within: &[],
        count: 1,
        row: "36",
        class: Class::Accepted(REJECTED_MIDTURN),
        reason: "the same failure, as the settled effect and its approval record",
    },
];

#[test]
fn the_port_runs_every_unified_connector_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let mut tally = Tally::new(
        UNIFIED_CORPUS,
        entries(UNIFIED_LEDGER).chain(entries(UNIFIED_V3_GAPS)),
    );
    let corpus: Value = serde_json::from_str(UNIFIED_CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    let replayed = UNIFIED_CAPTURE.port(
        &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("connectors-unified-parity-port.json"),
    );
    tally.compare(
        &UNIFIED_CAPTURE,
        &recorded,
        &scenarios(&replayed),
        differences,
    );
    tally.conclude("unified connectors parity:");
}

#[test]
fn every_unified_ledger_entry_stands_on_an_accepted_divergence() {
    let corpus: Value = serde_json::from_str(UNIFIED_CORPUS).expect("the corpus parses");
    assert_ledger(
        UNIFIED_LISTED,
        &scenarios(&corpus),
        entries(UNIFIED_LEDGER).chain(entries(UNIFIED_V3_GAPS)),
    );
}

#[test]
fn the_committed_unified_corpus_still_matches_the_pinned_reference() {
    UNIFIED_CAPTURE.probe("unified connectors");
}
