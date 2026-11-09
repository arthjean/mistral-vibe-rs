//! Replays the committed protocol corpus against this port's app server.
//!
//! `scripts/parity/app_server_protocol.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the provider and console APIs,
//! and records the shape of every answer, notification and server request the
//! app-server protocol publishes, from a probe per declared method to the
//! flows that chain them (turns, callbacks, the queue, manual shells,
//! compaction, the skills registry, the connector catalog, git checkouts).
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! The script records shapes rather than values, and reduces the prose a
//! server authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; everything else is compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), and every entry and
//! every debt group has to still reproduce, so row 17 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::debt::{Note, assert_notes};
use vibe_core::parity::replay::{
    Band, Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("protocol-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/protocol-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/app_server_protocol.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// What the re-pin's measured gaps of this corpus are known to stand on:
/// each note explains groups of its measured debt
/// (`vibe_core::parity::debt`) and counts nothing.
const DEBT_NOTES: &[Note] = &[
    Note {
        scenarios: &["*"],
        pointers: &[
            "/**/activeModel/autoCompactThreshold",
            "/**/activeModel/source",
            "/**/activeModel/maxContextLength",
            "/**/models/<list>/autoCompactThreshold",
            "/**/models/<list>/source",
            "/**/models/<list>/maxContextLength",
            "/**/config/cursorBlink",
            "/**/config/enableFileAutocomplete",
            "/**/config/enableMouseCapture",
            "/**/config/experimentalEnableLocalProjects",
            "/**/stats/modelCalls",
            "/**/mcp/privateConnectorsHidden",
            "/**/result/privateConnectorsHidden",
            "/**/result/activeCredentialSource",
            "/**/result/mistralActiveCredentialSource",
            "/**/result/mistralProvider",
            "/**/result/pinnedConsoleUrl",
            "/**/auth/apiKeyEnvVar",
            "/**/account/apiKey",
            "/**/result/reason",
            "/**/result/waits",
            "/**/<list>/outcome",
            "/**/entry/outcome",
            "/**/result/processId",
            "/**/params/warning",
            "/**/session/loops",
            "/**/catalog/catalogRevision",
            "/**/session/bumpedAt",
            "/**/session/agent",
            "/**/result/lastEventId",
            "/**/discoveryErrors/*",
        ],
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    Note {
        scenarios: &["*"],
        pointers: &[
            "/**/runtime/experimentalHarness",
            "/**/tools/<list>/isCustom",
            "/**/result/harnessSelectionSource",
            "/**/result/startupIssue",
            "/**/result/continueSessionId",
        ],
        reason: "v3.0.0 removed this field with the legacy backend's harness selection or the \
                 tool rows' custom flag; this port still publishes it because it keeps \
                 `--legacy-harness` (an open divergence of row 8)",
    },
    Note {
        scenarios: &["probe/artifacts/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/projects/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/setup/oidc-*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/imports/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/skills/local/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/session/subagents/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/session/turn/queue/edit/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/session/context/read/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/session/lease/subscribe/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/config/effective/read/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/connector_catalog/connect/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["probe/platformCredentials/headers/*"],
        pointers: &["/*/response/**"],
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    Note {
        scenarios: &["*"],
        pointers: &["/**/mcp/connectorError"],
        reason: "v3.0.0 reports no connector error for a provider that cannot list connectors, \
                 where this port publishes the transport failure it met",
    },
    Note {
        scenarios: &["flows/callback-approval"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/callback-refused-delivery"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/callback-reject"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/context-inject"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/session-mutations"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/session-start"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/session-start-refusals"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/shell-command"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/skills-registry"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/turn-lifecycle"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/turn-queue-idle"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/turn-queue-interrupt"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/turn-queue-refusals"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["flows/turn-tools"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    Note {
        scenarios: &["probe/connector_catalog/refresh/rooted"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    Note {
        scenarios: &["probe/plugin/reload/rooted"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    Note {
        scenarios: &["probe/session/compact/rooted"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    Note {
        scenarios: &["probe/session/history/clear/rooted"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    Note {
        scenarios: &["probe/session/resume/rooted"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    Note {
        scenarios: &["probe/session/start/bare"],
        pointers: &["/*/notifications/**"],
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    Note {
        scenarios: &["flows/worktree-reap"],
        pointers: &["/*/response/**"],
        reason: "the reap methods were removed at v3.0.0 and this port still serves them \
                 (PORT_ONLY_METHODS of the app-server surface replay)",
    },
    Note {
        scenarios: &["flows/worktree-reap-dirty"],
        pointers: &["/*/response/**"],
        reason: "the reap methods were removed at v3.0.0 and this port still serves them \
                 (PORT_ONLY_METHODS of the app-server surface replay)",
    },
    Note {
        scenarios: &["flows/worktree-reap-idle"],
        pointers: &["/*/response/**"],
        reason: "the reap methods were removed at v3.0.0 and this port still serves them \
                 (PORT_ONLY_METHODS of the app-server surface replay)",
    },
    Note {
        scenarios: &["flows/config-writes"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/connector-catalog"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/session-mutations"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/session-start"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/shell-command"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/skills-registry"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/turn-lifecycle"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/turn-queue-busy"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["flows/workspace-prompt"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/account/read/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/agents/list/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/connector_catalog/refresh/bare"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/connector_catalog/refresh/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/mcp/add/bare"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/mcp/add/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/mcp_catalog/add/bare"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/mcp_catalog/add/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/providerAuth/read/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/review/state/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/runtime/read/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/compact/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/continue/bare"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/continue/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/history/clear/bare"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/ready/read/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/ready/wait/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/resume/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/start/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/turn/queue/read/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/session/turn/queue/resume/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/skills/installed/rooted"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    Note {
        scenarios: &["probe/skills/list/bare"],
        pointers: &["/*/response/**"],
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
];

/// Every text this port words its own way, digest by digest.
/// The accepted divergence the prose below stands on.
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";

const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &["probe/vibeCode/projects/open/rooted"],
        pointer: "/*/response/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "17",
        accepted: SERVER_PROSE,
        reason: "the refusal of a project open the session's state does not allow",
    },
    OwnProse {
        scenarios: &[
            "flows/shell-command",
            "flows/turn-queue-refusals",
            "probe/session/settings/update/rooted",
            "probe/session/shellCommand/rooted",
        ],
        pointer: "/*/response/error/data/issues/*/message",
        count: 9,
        band: Band::DEFAULT,
        directives: None,
        row: "17",
        accepted: SERVER_PROSE,
        reason: "the message of a validation issue, under the reference's issue path and kind",
    },
];

/// What only a Windows host answers otherwise: the corpus is captured on
/// Linux.
const PLATFORM: &[PatternDivergence] = if cfg!(windows) {
    &[PatternDivergence {
        scenario: "flows/shell-command",
        pointer: "/9/response/error/message",
        count: 1,
        row: "6",
        class: Class::Gap,
        reason: "the refusal of a working directory outside the workspace spells the directory \
                 with the host's own separators and drive, so its digest differs from the POSIX                  path the corpus records",
    }]
} else {
    &[]
};

#[test]
fn the_port_answers_every_protocol_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "protocol parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("protocol-parity-port.json"),
    }
    .run(entries(PROSE).chain(entries(PLATFORM)));
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(PLATFORM)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("protocol");
}

#[test]
fn every_debt_note_explains_a_measured_difference() {
    assert_notes(LISTED, DEBT_NOTES);
}
