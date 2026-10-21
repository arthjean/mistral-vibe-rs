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

use vibe_core::parity::replay::PatternDivergence;

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

const LEDGER: &[PatternDivergence] = &[PatternDivergence {
    scenario: "tui/*",
    pointer: "/*/events/*/properties/is_cold_start",
    reason: "the reference reports whether Python rebuilt its bytecode cache, false from a \
                 source checkout and null for its own frozen binaries; this port is a compiled \
                 binary and reports null",
}];

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
