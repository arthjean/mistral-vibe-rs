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
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 17 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
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

/// What the port still answers differently. The v2.26.0 re-pin (`376f6a3`)
/// opened eleven fields the reference's public models gained and seven
/// methods this port neither declared nor routed, and the protocol pass closed
/// all of them.
///
/// The v3.1.0 re-pin (`c96a6db`) measures both servers on their Unified
/// Harness only and opened the gaps below, every one of row 17: the fields
/// v3.0.0 added to the configuration and runtime views and the ones it
/// removed with the harness selection, matched wherever they appear; the 33
/// methods it added, which this port answers as unknown; and, scenario by
/// scenario, the turn notifications it reshaped and the answers it changed.
/// A protocol follow-on closes them; each count moves only when the port or
/// the corpus does.
const LEDGER: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/**/activeModel/autoCompactThreshold",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/activeModel/source",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/activeModel/maxContextLength",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/models/<list>/autoCompactThreshold",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/models/<list>/source",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/models/<list>/maxContextLength",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/config/cursorBlink",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/config/enableFileAutocomplete",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/config/enableMouseCapture",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/config/experimentalEnableLocalProjects",
        count: 55,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/stats/modelCalls",
        count: 62,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/mcp/privateConnectorsHidden",
        count: 54,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/privateConnectorsHidden",
        count: 8,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/runtime/experimentalHarness",
        count: 52,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 removed this field with the legacy backend's harness selection or the \
                 tool rows' custom flag; this port still publishes it because it keeps \
                 `--legacy-harness` (an open divergence of row 8)",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/tools/<list>/isCustom",
        count: 52,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 removed this field with the legacy backend's harness selection or the \
                 tool rows' custom flag; this port still publishes it because it keeps \
                 `--legacy-harness` (an open divergence of row 8)",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/harnessSelectionSource",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 removed this field with the legacy backend's harness selection or the \
                 tool rows' custom flag; this port still publishes it because it keeps \
                 `--legacy-harness` (an open divergence of row 8)",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/startupIssue",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 removed this field with the legacy backend's harness selection or the \
                 tool rows' custom flag; this port still publishes it because it keeps \
                 `--legacy-harness` (an open divergence of row 8)",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/continueSessionId",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 removed this field with the legacy backend's harness selection or the \
                 tool rows' custom flag; this port still publishes it because it keeps \
                 `--legacy-harness` (an open divergence of row 8)",
    },
    PatternDivergence {
        scenario: "probe/artifacts/*",
        pointer: "/*/response/**",
        count: 49,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/projects/*",
        pointer: "/*/response/**",
        count: 58,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/setup/oidc-*",
        pointer: "/*/response/**",
        count: 50,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/imports/*",
        pointer: "/*/response/**",
        count: 41,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/skills/local/*",
        pointer: "/*/response/**",
        count: 50,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/session/subagents/*",
        pointer: "/*/response/**",
        count: 37,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/session/turn/queue/edit/*",
        pointer: "/*/response/**",
        count: 52,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/session/context/read/*",
        pointer: "/*/response/**",
        count: 11,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/session/lease/subscribe/*",
        pointer: "/*/response/**",
        count: 10,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/config/effective/read/*",
        pointer: "/*/response/**",
        count: 11,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/connector_catalog/connect/*",
        pointer: "/*/response/**",
        count: 15,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "probe/platformCredentials/headers/*",
        pointer: "/*/response/**",
        count: 15,
        row: "17",
        class: Class::Gap,
        reason: "a method v3.0.0 added (UNDECLARED_METHODS of the app-server surface replay), \
                 which this port answers as an unknown method",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/activeCredentialSource",
        count: 7,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/mistralActiveCredentialSource",
        count: 7,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/mistralProvider",
        count: 7,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/pinnedConsoleUrl",
        count: 7,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/auth/apiKeyEnvVar",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/account/apiKey",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/reason",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/waits",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/<list>/outcome",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/entry/outcome",
        count: 7,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/processId",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/params/warning",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/session/loops",
        count: 6,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/catalog/catalogRevision",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/session/bumpedAt",
        count: 6,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/session/agent",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/result/lastEventId",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/discoveryErrors/*",
        count: 6,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 added this field to the configuration and runtime views the app server \
                 publishes (`vibe/app_server/config.py`), which this port does not publish yet",
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/**/mcp/connectorError",
        count: 48,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reports no connector error for a provider that cannot list connectors, \
                 where this port publishes the transport failure it met",
    },
    PatternDivergence {
        scenario: "flows/callback-approval",
        pointer: "/*/notifications/**",
        count: 78,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/callback-refused-delivery",
        pointer: "/*/notifications/**",
        count: 78,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/callback-reject",
        pointer: "/*/notifications/**",
        count: 78,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/context-inject",
        pointer: "/*/notifications/**",
        count: 11,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/session-mutations",
        pointer: "/*/notifications/**",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/session-start",
        pointer: "/*/notifications/**",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/session-start-refusals",
        pointer: "/*/notifications/**",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/shell-command",
        pointer: "/*/notifications/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/skills-registry",
        pointer: "/*/notifications/**",
        count: 6,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/turn-lifecycle",
        pointer: "/*/notifications/**",
        count: 56,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/turn-queue-idle",
        pointer: "/*/notifications/**",
        count: 10,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/turn-queue-interrupt",
        pointer: "/*/notifications/**",
        count: 11,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/turn-queue-refusals",
        pointer: "/*/notifications/**",
        count: 20,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "flows/turn-tools",
        pointer: "/*/notifications/**",
        count: 78,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 reshaped the notifications that open, carry and close a turn: \
                 `runtime/updated` and a session patch on `/agent` lead it, \
                 `session/statsUpdated` precedes the end and the history entries land in another \
                 order; this port publishes the v2.26.0 Unified sequence",
    },
    PatternDivergence {
        scenario: "probe/connector_catalog/refresh/rooted",
        pointer: "/*/notifications/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    PatternDivergence {
        scenario: "probe/plugin/reload/rooted",
        pointer: "/*/notifications/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    PatternDivergence {
        scenario: "probe/session/compact/rooted",
        pointer: "/*/notifications/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    PatternDivergence {
        scenario: "probe/session/history/clear/rooted",
        pointer: "/*/notifications/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    PatternDivergence {
        scenario: "probe/session/resume/rooted",
        pointer: "/*/notifications/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    PatternDivergence {
        scenario: "probe/session/start/bare",
        pointer: "/*/notifications/**",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 answers this method with another set of notifications (a `warning` at \
                 session start, no session patch after a compaction)",
    },
    PatternDivergence {
        scenario: "flows/worktree-reap",
        pointer: "/*/response/**",
        count: 12,
        row: "17",
        class: Class::Gap,
        reason: "the reap methods were removed at v3.0.0 and this port still serves them \
                 (PORT_ONLY_METHODS of the app-server surface replay)",
    },
    PatternDivergence {
        scenario: "flows/worktree-reap-dirty",
        pointer: "/*/response/**",
        count: 8,
        row: "17",
        class: Class::Gap,
        reason: "the reap methods were removed at v3.0.0 and this port still serves them \
                 (PORT_ONLY_METHODS of the app-server surface replay)",
    },
    PatternDivergence {
        scenario: "flows/worktree-reap-idle",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "the reap methods were removed at v3.0.0 and this port still serves them \
                 (PORT_ONLY_METHODS of the app-server surface replay)",
    },
    PatternDivergence {
        scenario: "flows/config-writes",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/connector-catalog",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/session-mutations",
        pointer: "/*/response/**",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/session-start",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/shell-command",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/skills-registry",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/turn-lifecycle",
        pointer: "/*/response/**",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/turn-queue-busy",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "flows/workspace-prompt",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/account/read/rooted",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/agents/list/rooted",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/connector_catalog/refresh/bare",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/connector_catalog/refresh/rooted",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/mcp/add/bare",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/mcp/add/rooted",
        pointer: "/*/response/**",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/mcp_catalog/add/bare",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/mcp_catalog/add/rooted",
        pointer: "/*/response/**",
        count: 5,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/providerAuth/read/rooted",
        pointer: "/*/response/**",
        count: 3,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/review/state/rooted",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/runtime/read/rooted",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/compact/rooted",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/continue/bare",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/continue/rooted",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/history/clear/bare",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/ready/read/rooted",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/ready/wait/rooted",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/resume/rooted",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/start/rooted",
        pointer: "/*/response/**",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/turn/queue/read/rooted",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/session/turn/queue/resume/rooted",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/skills/installed/rooted",
        pointer: "/*/response/**",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 changed how this method answers or refuses (error code, refusal data, \
                 result shape) and this port answers as v2.26.0 did",
    },
    PatternDivergence {
        scenario: "probe/skills/list/bare",
        pointer: "/*/response/**",
        count: 4,
        row: "17",
        class: Class::Gap,
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
    .run(
        entries(PROSE)
            .chain(entries(PLATFORM))
            .chain(entries(LEDGER)),
    );
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE)
            .chain(entries(PLATFORM))
            .chain(entries(LEDGER)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("protocol");
}
