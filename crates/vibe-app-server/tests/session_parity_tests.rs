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
//! SHA-256. This port writes its own prose on purpose (`NOTICE`), so two
//! digests count as equal wherever both sides hold one, and everything else is
//! compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 16 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use vibe_core::parity::replay::{Capture, Entry, Prose, Replay, entries, scorecard_rows};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("session-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/sessions.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 25;

/// A field this port answers differently wherever it appears, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    /// Every difference whose JSON pointer ends with this suffix is covered,
    /// in any scenario, and so is every element of a list the suffix names.
    suffix: &'static str,
    row: &'static str,
    reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, _scenario: &str, pointer: &str) -> bool {
        pointer.ends_with(self.suffix)
            || pointer.rsplit_once('/').is_some_and(|(list, index)| {
                list.ends_with(self.suffix) && index.parse::<usize>().is_ok()
            })
    }

    fn label(&self) -> String {
        self.suffix.to_owned()
    }
}

const LEDGER: &[Divergence] = &[];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: on win32 the reference records a .last_session pointer (keyed by console window or parent pid) that a Linux capture never holds"
)]
fn the_port_answers_every_session_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "session parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("session-parity-port.json"),
        prose: Prose::OwnWords,
    }
    .run(entries(LEDGER));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let rows = scorecard_rows(SCORECARD);
    for entry in LEDGER {
        assert!(
            rows.contains(entry.row),
            "the ledger entry for {} names row {}, which docs/parity.md does not carry",
            entry.suffix,
            entry.row
        );
        assert_ne!(
            entry.row, "16",
            "row 16 is what this corpus measures, so a difference it keeps is a gap, not a \
             ledger entry: {}",
            entry.suffix
        );
        assert!(
            entry.suffix.starts_with('/') && !entry.reason.is_empty(),
            "the ledger entry for {} needs a pointer suffix and a reason",
            entry.suffix
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("session");
}
