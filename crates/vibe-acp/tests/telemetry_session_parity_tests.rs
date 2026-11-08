//! Replays the committed telemetry session corpus against this port's
//! `vibe-acp`.
//!
//! `scripts/parity/telemetry_session.py` captured the corpus from the pinned
//! reference's own entry points: every scenario builds a fresh home and
//! workspace behind a scripted stand-in for the whole Mistral platform a
//! session reaches, runs one entry point against it, and records every event
//! the datalake received, the census every chat-completions request carried,
//! the attributes every evaluation request posted, and how often the identity,
//! the account and the managed configuration were asked for. This file drives
//! the editor surface, a scripted editor connection to `vibe-acp`, through the
//! same script and compares the normalized observations scenario by scenario;
//! `crates/vibe-cli/tests/telemetry_session_parity_tests.rs` replays the two
//! surfaces `vibe` serves. It needs no reference checkout; only the live probe
//! at the end, which recaptures the reference, does.
//!
//! Every difference has to fall under a `LEDGER` entry, and every entry has to
//! still reproduce, so row 27 of `docs/parity.md` is a reading of the summary
//! this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use vibe_core::parity::replay::{Class, PatternDivergence};

#[path = "../../vibe-cli/tests/support/telemetry_session.rs"]
mod telemetry_session;
use telemetry_session::Surfaces;

/// The surface this binary serves.
const SURFACES: Surfaces = Surfaces {
    served: &["acp"],
    replayed: &["acp"],
    binary: ["--vibe-acp", env!("CARGO_BIN_EXE_vibe-acp")],
    floor: 10,
};
/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 reports an editor session otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/*/accountRequests",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/chatRequests",
        count: 10,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/identityRequests",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/managedConfigRequests",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/chatMetadata/*",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/chatMetadata/*/call_type",
        count: 2,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/chatMetadata/*/message_id",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/chatMetadata/*/user_plan",
        count: 14,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/evaluations/*",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*",
        count: 28,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/event",
        count: 13,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/action",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/attachment_counts",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/call_source",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/call_type",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/command",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/command_type",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/entrypoint",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/has_agents_md",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/has_outside_workdir_deny",
        count: 9,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/host_kind",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/init_duration_ms",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/message_id",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/model",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_context_chars",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_context_messages",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_mcp_servers",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_models",
        count: 10,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_post_agent_main_hooks",
        count: 9,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_post_agent_scoped_hooks",
        count: 9,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_prompt_chars",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/nb_skills",
        count: 10,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/events/*/properties/session_id",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

#[test]
fn the_port_reports_every_session_as_the_corpus_records_or_as_the_ledger_names() {
    SURFACES.replay(
        V3_GAPS,
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("telemetry-session-port.json"),
    );
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    SURFACES.assert_ledger(V3_GAPS);
}

/// Recaptures the pinned reference on this surface and asserts the committed
/// corpus is still what it answers. The replay above runs regardless, which is
/// what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    SURFACES.check("editor telemetry sessions");
}
