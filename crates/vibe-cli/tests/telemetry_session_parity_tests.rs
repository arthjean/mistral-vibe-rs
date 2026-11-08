//! Replays the committed telemetry session corpus against this port's `vibe`.
//!
//! `scripts/parity/telemetry_session.py` captured the corpus from the pinned
//! reference's own entry points: every scenario builds a fresh home and
//! workspace behind a scripted stand-in for the whole Mistral platform a
//! session reaches, runs one entry point against it, and records every event
//! the datalake received, the census every chat-completions request carried,
//! the attributes every evaluation request posted, and how often the identity,
//! the account and the managed configuration were asked for. This file drives
//! the two surfaces this binary serves, `vibe -p` and the interactive client on
//! a pseudo-terminal, through the same script and compares the normalized
//! observations scenario by scenario (`support/telemetry_session.rs`);
//! `crates/vibe-acp/tests` replays the editor surface. It needs no reference
//! checkout; only the live probe at the end, which recaptures the reference,
//! does.
//!
//! Every difference has to fall under a `LEDGER` entry, and every entry has to
//! still reproduce, so row 27 of `docs/parity.md` is a reading of the summary
//! this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use vibe_core::parity::replay::{Class, PatternDivergence};

#[path = "support/telemetry_session.rs"]
mod telemetry_session;
use telemetry_session::Surfaces;

const SURFACES: Surfaces = Surfaces {
    served: &["programmatic", "tui"],
    // The capture script drives the `tui` surface through a POSIX
    // pseudo-terminal, which Windows lacks.
    replayed: if cfg!(windows) {
        &["programmatic"]
    } else {
        &["programmatic", "tui"]
    },
    binary: ["--vibe", env!("CARGO_BIN_EXE_vibe")],
    floor: 30,
};
/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 reports this session otherwise from its Rust terminal client and Unified Harness, which this port does not reproduce yet";

/// The accepted divergence first, then the gaps the v3.1.0 re-pin revealed,
/// by pointer with its indices generalized, most specific first.
const LEDGER: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/is_cold_start",
        // One event per `tui` scenario; a host that does not replay the surface
        // drops the entry (`support/telemetry_session.rs`).
        count: 5,
        row: "27",
        class: Class::Accepted("`is_cold_start` is null"),
        reason: "the reference reports whether Python rebuilt its bytecode cache, false from a \
                     source checkout and null for its own frozen binaries; this port is a compiled \
                     binary and reports null",
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/accountRequests",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/chatRequests",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/exit",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/identityRequests",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/chatMetadata/*",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/chatMetadata/*/client_name",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/chatMetadata/*/user_plan",
        count: 29,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/chatMetadata/*/user_plan",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/evaluations/*",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/evaluations/*",
        count: 2,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/evaluations/*/client_name",
        count: 5,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*",
        count: 36,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/event",
        count: 29,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/event",
        count: 12,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/action",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/agent_ready_duration_ms",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/attachment_counts",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/attachment_counts",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/call_source",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/call_source",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/call_type",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/call_type",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/client_name",
        count: 37,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/command",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/command_type",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/entrypoint",
        count: 4,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/experiment_assignments",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/experiment_attributes",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/experiment_attributes/client_name",
        count: 32,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/experiments",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/first_frame_duration_ms",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/harness_selection_source",
        count: 5,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/has_agents_md",
        count: 4,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/has_initial_prompt",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/has_outside_workdir_deny",
        count: 23,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/has_outside_workdir_deny",
        count: 7,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/host_kind",
        count: 4,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/init_duration_ms",
        count: 4,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/is_resuming_session",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/message_id",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/message_id",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/model",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/model",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/mouse_capture",
        count: 2,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_context_chars",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_context_chars",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_context_messages",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_context_messages",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_enforced_fields",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_mcp_servers",
        count: 4,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_models",
        count: 26,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_models",
        count: 7,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_post_agent_main_hooks",
        count: 23,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_post_agent_main_hooks",
        count: 7,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_post_agent_scoped_hooks",
        count: 23,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_post_agent_scoped_hooks",
        count: 7,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_prompt_chars",
        count: 25,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_prompt_chars",
        count: 6,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/nb_skills",
        count: 26,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/nb_skills",
        count: 7,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/outcome",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/prompt_for_workspace_trust",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/session_id",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/session_init_duration_ms",
        count: 5,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/show_resume_picker",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "tui/*",
        pointer: "/*/events/*/properties/teleport_on_start",
        count: 3,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "programmatic/*",
        pointer: "/*/events/*/properties/user_plan",
        count: 1,
        row: "27",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

#[test]
fn the_port_reports_every_session_as_the_corpus_records_or_as_the_ledger_names() {
    SURFACES.replay(
        LEDGER,
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("telemetry-session-port.json"),
    );
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    SURFACES.assert_ledger(LEDGER);
}

/// Recaptures the pinned reference on these surfaces and asserts the committed
/// corpus is still what it answers. The replay above runs regardless, which is
/// what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    SURFACES.check("telemetry sessions");
}
