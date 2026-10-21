//! Replays the committed `projectLinks/*` corpus against this port's app server.
//!
//! `scripts/parity/project_links.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, beside plain directories, Git checkouts in several states, a
//! bare repository and a file, behind a scripted stand-in for the Vibe Code
//! projects endpoint, and drives the nine `projectLinks/*` methods row 25 of
//! `docs/parity.md` is about. `store` records the link file byte for byte and
//! its mode; `list`, `resolve` and `picker` what the reads answer; `create`,
//! `link`, `save` and `unlink` what each mutation checks, sends and persists;
//! `teleport` how a link reads to the session picker sharing the store.
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: the sentences a refusal or a
//! failure carries. Every difference has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 25 is a reading of the summary
//! this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Entry, Prose, Replay, entries, scenarios, scorecard_rows,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("project-links-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/project_links.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server process.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 120;

/// The accepted divergence the entries below stand on: a difference the
/// measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "Teleport and Vibe Code refusals are this port's own prose";

/// A difference this port keeps in the scenarios named, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer contains this fragment is covered.
    fragment: &'static str,
    row: &'static str,
    reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenarios.contains(&scenario) && pointer.contains(self.fragment)
    }

    fn label(&self) -> String {
        self.fragment.to_owned()
    }

    fn scenarios(&self) -> &[&str] {
        self.scenarios
    }
}

/// Every scenario answered with a refusal or a failure: a root the picker
/// cannot use, no key, a Vibe Code answer that failed, a project the link
/// cannot take, a remote that moved.
const REFUSED: &[&str] = &[
    "picker/plain-directory",
    "picker/no-github-remote",
    "picker/no-commits",
    "picker/missing-root",
    "picker/without-key",
    "picker/without-key-plain",
    "picker/list-401",
    "picker/list-403",
    "picker/list-404",
    "picker/list-500",
    "picker/list-body-names-key",
    "picker/list-invalid-json",
    "picker/list-invalid-schema",
    "picker/list-dropped",
    "picker/load-more-fails-late",
    "create/blank-name",
    "create/blank-branch",
    "create/listing-fails-first",
    "create/refused-401",
    "create/refused-409",
    "create/refused-500",
    "create/invalid-answer",
    "create/plain-directory",
    "link/unknown",
    "link/read-only",
    "link/other-repository",
    "link/listing-fails",
    "link/plain-directory",
    "save/plain-with-expected",
    "save/repo-mismatch",
    "save/repo-without-expected",
    "save/missing-root",
    "save/file-root",
];

const LEDGER: &[Divergence] = &[Divergence {
    scenarios: REFUSED,
    fragment: "/response/error/message/",
    row: "25",
    reason: "the sentence a refusal or a failure carries; the code, and the absence of data, \
             are the reference's",
}];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the corpus records the store's POSIX mode and a 0o444 store's write error, and the reference resolves a rootless /l onto the current drive"
)]
fn the_port_answers_every_project_links_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "project links parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("project-links-parity-port.json"),
        prose: Prose::Compared,
    }
    .run(entries(LEDGER));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let rows = scorecard_rows(SCORECARD);
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    for entry in LEDGER {
        assert!(
            rows.contains(entry.row),
            "the ledger entry for {} names row {}, which docs/parity.md does not carry",
            entry.fragment,
            entry.row
        );
        // Row 25 is what this corpus measures, so a difference it keeps is a
        // gap unless the scorecard decided to keep it.
        assert!(
            entry.row != "25" || SCORECARD.contains(&format!("| {OWN_PROSE} |")),
            "the ledger entry for {} keeps a row 25 difference that no accepted divergence \
             decides",
            entry.fragment
        );
        assert!(
            !entry.scenarios.is_empty()
                && entry
                    .scenarios
                    .iter()
                    .all(|scenario| recorded.contains_key(*scenario))
                && entry.fragment.starts_with('/')
                && !entry.reason.is_empty(),
            "the ledger entry for {} needs recorded scenarios, a pointer fragment and a reason",
            entry.fragment
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("project links");
}
