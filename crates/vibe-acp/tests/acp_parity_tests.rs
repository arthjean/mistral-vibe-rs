//! Replays the committed ACP corpus against this port's `vibe-acp`.
//!
//! `scripts/parity/acp.py` captured the corpus from the pinned reference's own
//! `vibe-acp`: every scenario starts the agent over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, and records what
//! the agent writes. Driving this crate's binary through the same script with
//! `--agent` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their agent authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`), so two digests count
//! as equal wherever both sides hold one, and everything else is compared
//! exactly.
//!
//! The corpus also records how the entry point answers argument vectors that
//! exit before it serves anything, which is compared with no ledger at all.
//!
//! The script captures each harness backend into its own corpus: `corpus.json`
//! runs both sides on the legacy backend through the escape hatch, and
//! `unified-corpus.json` on the Unified Harness every session opens on by
//! default. Both replay against the same ledger.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce on each backend, so rows 8 and 36 of
//! `docs/parity.md` are readings of the summaries this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Entry, Prose, Tally, differences, entries, pinned_reference, scenarios, scorecard_rows,
};

/// The corpora, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("acp-parity/corpus.json");
const UNIFIED_CORPUS: &str = include_str!("acp-parity/unified-corpus.json");

/// The script's two families, one per backend.
const LEGACY: Capture = Capture {
    script: "scripts/parity/acp.py",
    family: &["--harness", "legacy"],
    arguments: JOBS,
};
const UNIFIED: Capture = Capture {
    script: "scripts/parity/acp.py",
    family: &["--harness", "unified"],
    arguments: JOBS,
};

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 81;
const ARGV_FLOOR: usize = 23;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and agent process.
const JOBS: &[&str] = &["--jobs", "6"];

/// A difference this port keeps, and the row of `docs/parity.md` that
/// answers for it.
struct Divergence {
    scenario: &'static str,
    /// Every difference under this JSON pointer, in that scenario, is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenario == scenario
            && (pointer == self.pointer
                || pointer
                    .strip_prefix(self.pointer)
                    .is_some_and(|rest| rest.starts_with('/')))
    }

    fn label(&self) -> String {
        self.pointer.to_owned()
    }

    fn scenarios(&self) -> &[&str] {
        std::slice::from_ref(&self.scenario)
    }
}

const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "initialize/terminal-auth",
        pointer: "/0/response/result/authMethods/1",
        row: "8",
        reason: "the reference runs under a Python interpreter, so its terminal sign-in \
                 relaunches `python3` with the script as the first argument; a native \
                 executable announces itself with `--setup`, which is the reference's own \
                 branch for one",
    },
    Divergence {
        scenario: "ext/config-schema",
        pointer: "/1/response/result",
        row: "30",
        reason: "`_config/schema` relays the app server's `config/schema`, whose content is \
                 the configuration model",
    },
];

#[test]
fn the_port_answers_every_legacy_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(CORPUS, &LEGACY, "legacy");
}

#[test]
fn the_port_answers_every_unified_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(UNIFIED_CORPUS, &UNIFIED, "unified");
}

fn replay(corpus: &str, capture: &Capture, harness: &str) {
    let corpus: Value = serde_json::from_str(corpus).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let replayed = capture.port(
        &["--agent", env!("CARGO_BIN_EXE_vibe-acp")],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("acp-parity-port-{harness}.json")),
    );
    // The command line answers before any scenario runs, and nothing about it
    // is ledgered: every vector exits the way the reference's argparse does.
    let argv = corpus["argv"]
        .as_array()
        .expect("the corpus records the command line");
    assert!(
        argv.len() >= ARGV_FLOOR,
        "the corpus holds {} argument vectors, below the floor of {ARGV_FLOOR}",
        argv.len()
    );
    let argv_differences: Vec<String> =
        differences(&corpus["argv"], &replayed["argv"], Prose::OwnWords)
            .into_iter()
            .map(|pointer| format!("/argv{pointer}"))
            .collect();
    assert!(
        argv_differences.is_empty(),
        "the command line departs from the corpus at {argv_differences:?}"
    );

    let mut tally = Tally::new(entries(LEDGER));
    tally.compare(
        capture,
        &recorded,
        &scenarios(&replayed),
        |reference, port| differences(reference, port, Prose::OwnWords),
    );
    tally.conclude(&format!(
        "acp parity ({harness}): {}/{} argument vectors and",
        argv.len(),
        argv.len()
    ));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_and_a_recorded_scenario() {
    let rows = scorecard_rows(SCORECARD);
    for corpus in [CORPUS, UNIFIED_CORPUS] {
        let corpus: Value = serde_json::from_str(corpus).expect("the corpus parses");
        let recorded = scenarios(&corpus);
        for entry in LEDGER {
            assert!(
                recorded.contains_key(entry.scenario),
                "the ledger entry names {}, which a corpus does not record",
                entry.scenario
            );
        }
    }
    for entry in LEDGER {
        assert!(
            rows.contains(entry.row),
            "the ledger entry for {} names row {}, which docs/parity.md does not carry",
            entry.scenario,
            entry.row
        );
        assert!(
            entry.pointer.starts_with('/') && !entry.reason.is_empty(),
            "the ledger entry for {} needs a pointer and a reason",
            entry.scenario
        );
    }
}

/// Recaptures the pinned reference on each backend and asserts the committed
/// corpora are still what it answers. The replay above runs regardless, which
/// is what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    if let Some(root) = pinned_reference("ACP") {
        LEGACY.check(&root, &[]);
        UNIFIED.check(&root, &[]);
    }
}
