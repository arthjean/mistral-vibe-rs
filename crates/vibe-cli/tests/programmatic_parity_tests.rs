//! Replays the committed programmatic mode corpus against this port's `vibe`.
//!
//! `scripts/parity/programmatic.py` captured the corpus from the pinned
//! reference's own `vibe` entry point: every scenario builds a fresh home and
//! workspace behind a scripted stand-in for the chat-completions API (and, for
//! Teleport, the console's account endpoint), runs `vibe -p` one or more times
//! with the scenario's arguments and standard input, and records the exit
//! code, standard output, standard error, what each model request asked for
//! and the files the run left behind. `json` and `streaming` documents are
//! kept as values with the order of every object's keys and a flag saying
//! whether the bytes are exactly what Python's `json.dumps` writes. Driving
//! this crate's binary through the same script with `--binary` yields
//! observations normalized the same way, and the replay compares the two
//! scenario by scenario. It needs no reference checkout; only the live probe
//! at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string the program authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`), so two digests count
//! as equal wherever both sides hold one, and everything else is compared
//! exactly. Every other difference has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 13 of `docs/parity.md` is a
//! reading of the summary this file prints.
//!
//! The script captures each backend apart: `corpus.json` holds the legacy one
//! both sides reach through the escape hatch, `unified-corpus.json` the
//! Unified Harness every session opens on by default (`--harness unified`).

use std::path::Path;

use vibe_core::parity::replay::{
    Capture, PatternDivergence, Prose, Replay, entries, pinned_reference,
};

/// The corpora, compiled in so a moved file fails the build rather than the
/// run.
const CORPUS: &str = include_str!("programmatic-parity/corpus.json");
const UNIFIED_CORPUS: &str = include_str!("programmatic-parity/unified-corpus.json");

/// The script's two families, one per backend.
const LEGACY: Capture = Capture {
    script: "scripts/parity/programmatic.py",
    family: &["--harness", "legacy"],
    arguments: &[],
};
const UNIFIED: Capture = Capture {
    script: "scripts/parity/programmatic.py",
    family: &["--harness", "unified"],
    arguments: &[],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 56;

const LEDGER: &[PatternDivergence] = &[];

#[test]
fn the_port_answers_every_programmatic_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(CORPUS, &LEGACY, "legacy");
}

#[test]
fn the_port_answers_every_unified_programmatic_scenario_as_the_corpus_records() {
    replay(UNIFIED_CORPUS, &UNIFIED, "unified");
}

/// Drives this port's `vibe` through every scenario `capture` records on
/// `harness` and compares what it answers with `corpus`.
fn replay(corpus: &str, capture: &Capture, harness: &str) {
    Replay {
        heading: &format!("programmatic parity ({harness}):"),
        corpus,
        floor: SCENARIO_FLOOR,
        capture,
        port: &["--binary", env!("CARGO_BIN_EXE_vibe")],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("programmatic-port-{harness}.json")),
        prose: Prose::OwnWords,
    }
    .run(entries(LEDGER));
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    PatternDivergence::assert_ledger(LEDGER);
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    if let Some(root) = pinned_reference("programmatic mode") {
        LEGACY.check(&root, &[]);
        UNIFIED.check(&root, &[]);
    }
}
