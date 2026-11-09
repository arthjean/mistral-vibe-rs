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
//! Every difference this surface keeps is a gap of row 27, so it falls under
//! the corpus's measured debt (`vibe_core::parity::debt`) and no ledger
//! entry; row 27 of `docs/parity.md` is a reading of the summary this file
//! prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use vibe_core::parity::debt::{Note, assert_notes};

#[path = "../../vibe-cli/tests/support/telemetry_session.rs"]
mod telemetry_session;
use telemetry_session::{LISTED, Surfaces};

/// The surface this binary serves.
const SURFACES: Surfaces = Surfaces {
    served: &["acp"],
    replayed: &["acp"],
    binary: ["--vibe-acp", env!("CARGO_BIN_EXE_vibe-acp")],
    floor: 10,
};
/// What the measured debt of this surface stands on.
const DEBT_NOTES: &[Note] = &[Note {
    scenarios: &["acp/*"],
    pointers: &["/**"],
    reason: "v3.1.0 reports an editor session otherwise on the Unified Harness the reference \
             now runs on alone, which this port does not reproduce yet",
}];

#[test]
fn the_port_reports_every_session_as_the_corpus_records_or_as_the_ledger_names() {
    SURFACES.replay(
        &[],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("telemetry-session-port.json"),
    );
}

#[test]
fn every_debt_note_explains_a_measured_difference() {
    assert_notes(LISTED, DEBT_NOTES);
}

/// Recaptures the pinned reference on this surface and asserts the committed
/// corpus is still what it answers. The replay above runs regardless, which is
/// what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    SURFACES.check("editor telemetry sessions");
}
