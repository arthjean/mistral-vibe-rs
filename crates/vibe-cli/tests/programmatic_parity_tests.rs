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
//! exactly. Every other difference has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), and every entry and
//! every debt group has to still reproduce, so row 13 of `docs/parity.md` is a
//! reading of the summary this file prints.
//!
//! From v3.0.0 the reference runs one backend, the Unified Harness, so one
//! corpus records it; the legacy backend's corpus went with the escape hatch.

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Replay, assert_ledger, entries, pinned_reference, scenarios,
};

/// The corpora, compiled in so a moved file fails the build rather than the
/// run.
const CORPUS: &str = include_str!("programmatic-parity/corpus.json");

/// The corpora as `scripts/parity/manifest.json` lists them.
const LISTED: &str = "crates/vibe-cli/tests/programmatic-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/programmatic.py",
    family: &[],
    arguments: &[],
};

/// The accepted divergences the prose below stands on.
const OWN_SENTENCES: &str = "Programmatic mode's own sentences";

/// Every text this port words its own way, digest by digest. At v3.1.0 only
/// the warning about an untrusted folder's project configuration still falls
/// in its band; the other refusals moved, and are gaps below.
const PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &["trust/untrusted-project-config"],
    pointer: "/*/stderr/*/message",
    count: 1,
    band: Band::DEFAULT,
    directives: None,
    row: "13",
    accepted: OWN_SENTENCES,
    reason: "the warning `vibe -p` reports when it ignores an untrusted folder's configuration",
}];
/// Drives this port's `vibe` through every scenario the corpus records and
/// compares what it answers.
#[test]
fn the_port_answers_every_programmatic_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "programmatic parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &["--binary", env!("CARGO_BIN_EXE_vibe")],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("programmatic-port.json"),
    }
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    if let Some(root) = pinned_reference("programmatic mode") {
        CAPTURE.check(&root, &[]);
    }
}
