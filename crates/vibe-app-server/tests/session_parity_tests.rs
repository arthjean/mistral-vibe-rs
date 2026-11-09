//! Replays the committed session corpus against this port's app server.
//!
//! `scripts/parity/sessions.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, drives real turns
//! so sessions reach disk, and records each answer of the saved-session
//! surface (`session/list`, `session/read`, `session/history/*`,
//! `session/turns/list`, `session/rename`, `session/pin`, `session/relocate`,
//! `session/log/read`, `session/resume`, `session/continue`, `session/fork`,
//! `session/delete` and `session/history/clear`), the notifications it raised,
//! and the save directory afterward: the session directories, the metadata
//! keys and value types, the listing cache and the leases. Driving the
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
//! compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), and every entry and
//! every debt group has to still reproduce, so row 16 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{Capture, OwnProse, Replay, assert_ledger, entries, scenarios};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("session-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/session-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/sessions.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

const PROSE: &[OwnProse] = &[];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: on win32 the reference records a .last_session pointer (keyed by console window or parent pid) that a Linux capture never holds"
)]
fn the_port_answers_every_session_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "session parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("session-parity-port.json"),
    }
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("session");
}
