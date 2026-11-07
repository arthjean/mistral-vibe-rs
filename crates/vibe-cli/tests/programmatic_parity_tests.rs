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

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries,
    pinned_reference, scenarios,
};

/// The corpora, compiled in so a moved file fails the build rather than the
/// run.
const CORPUS: &str = include_str!("programmatic-parity/corpus.json");
const UNIFIED_CORPUS: &str = include_str!("programmatic-parity/unified-corpus.json");

/// The corpora as `scripts/parity/manifest.json` lists them.
const LISTED: &str = "crates/vibe-cli/tests/programmatic-parity/corpus.json";
const UNIFIED_LISTED: &str = "crates/vibe-cli/tests/programmatic-parity/unified-corpus.json";

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

/// What the port answers otherwise on both backends.
const LEDGER: &[PatternDivergence] = &[PatternDivergence {
    scenario: "output/ndjson-is-no-format",
    pointer: "/*/stderr/usage",
    count: 1,
    row: "7",
    class: Class::Gap,
    reason: "an argument error prints this port's one-line usage where argparse prints the \
             whole synopsis, 474 code points of it, so the digest is not a rewording but a              shorter text",
}];

/// The accepted divergences the prose below stands on.
const OWN_SENTENCES: &str = "Programmatic mode's own sentences";
const CALL_FAILURES: &str = "The model call failure sentences are original prose";
const TOOL_RESULTS: &str = "Authored result and error text in six tools";

/// The checkpoint a continued session closes its history on names the
/// session in a full sentence where the reference writes a 15-character
/// marker, 2.4 times as long.
const CHECKPOINT_BAND: Band = Band {
    low: 0.5,
    high: 2.5,
};

/// Every text this port words its own way, digest by digest, per backend.
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &["approval/denied"],
        pointer: "/*/stdout/lines/*/json/state/display/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "13",
        accepted: OWN_SENTENCES,
        reason: "the display of a call the approval declined",
    },
    OwnProse {
        scenarios: &[
            "config/active-provider-missing-key",
            "failure/missing-key",
            "failure/missing-key-before-empty-prompt",
            "teleport/codestral-key",
            "teleport/non-mistral-model",
            "teleport/outside-a-repository",
            "teleport/unverified-key",
            "trust/untrusted-project-config",
        ],
        pointer: "/*/stderr/*/message",
        count: 8,
        band: Band::DEFAULT,
        directives: None,
        row: "13",
        accepted: OWN_SENTENCES,
        reason: "the warning or error `vibe -p` reports: an untrusted folder, a missing key, a refused Teleport",
    },
    OwnProse {
        scenarios: &[
            "failure/bad-request-json",
            "failure/bad-request-streaming",
            "failure/unauthorized",
        ],
        pointer: "/*/stderr/*/message",
        count: 3,
        band: Band::DEFAULT,
        directives: None,
        row: "10",
        accepted: CALL_FAILURES,
        reason: "the error a model call the provider refused reports",
    },
    OwnProse {
        scenarios: &["failure/bad-request-json", "failure/bad-request-streaming"],
        pointer: "/*/stderr/*",
        count: 16,
        band: Band::DEFAULT,
        directives: None,
        row: "10",
        accepted: CALL_FAILURES,
        reason: "the lines that detail a model call the provider refused",
    },
    OwnProse {
        scenarios: &["session/continue-json"],
        pointer: "/*/stdout/json/*/message",
        count: 1,
        band: CHECKPOINT_BAND,
        directives: None,
        row: "13",
        accepted: OWN_SENTENCES,
        reason: "the checkpoint a continued session closes its history on",
    },
    OwnProse {
        scenarios: &["session/continue-streaming"],
        pointer: "/*/stdout/lines/*/json/message",
        count: 1,
        band: CHECKPOINT_BAND,
        directives: None,
        row: "13",
        accepted: OWN_SENTENCES,
        reason: "the same checkpoint, as the stream carries it",
    },
    OwnProse {
        scenarios: &["tools/failed-read"],
        pointer: "/*/stdout/json/*/state/display/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "`read_file` reports a missing file in its own sentence, as the failed call's header",
    },
    OwnProse {
        scenarios: &["tools/failed-read"],
        pointer: "/*/stdout/json/*/state/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same sentence, as the failed call's error",
    },
];
const UNIFIED_PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &[
        "config/active-provider-missing-key",
        "failure/missing-key",
        "failure/missing-key-before-empty-prompt",
        "teleport/codestral-key",
        "teleport/non-mistral-model",
        "teleport/outside-a-repository",
        "teleport/unverified-key",
        "trust/untrusted-project-config",
    ],
    pointer: "/*/stderr/*/message",
    count: 8,
    band: Band::DEFAULT,
    directives: None,
    row: "13",
    accepted: OWN_SENTENCES,
    reason: "the warning or error `vibe -p` reports: an untrusted folder, a missing key, a refused Teleport",
}];

#[test]
fn the_port_answers_every_programmatic_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(CORPUS, &LEGACY, "legacy", PROSE);
}

#[test]
fn the_port_answers_every_unified_programmatic_scenario_as_the_corpus_records() {
    replay(UNIFIED_CORPUS, &UNIFIED, "unified", UNIFIED_PROSE);
}

/// Drives this port's `vibe` through every scenario `capture` records on
/// `harness` and compares what it answers with `corpus`.
fn replay(corpus: &str, capture: &Capture, harness: &str, prose: &[OwnProse]) {
    Replay {
        heading: &format!("programmatic parity ({harness}):"),
        corpus,
        capture,
        port: &["--binary", env!("CARGO_BIN_EXE_vibe")],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("programmatic-port-{harness}.json")),
    }
    .run(entries(prose).chain(entries(LEDGER)));
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    for (listed, corpus, prose) in [
        (LISTED, CORPUS, PROSE),
        (UNIFIED_LISTED, UNIFIED_CORPUS, UNIFIED_PROSE),
    ] {
        let corpus: Value = serde_json::from_str(corpus).expect("the corpus parses");
        assert_ledger(
            listed,
            &scenarios(&corpus),
            entries(prose).chain(entries(LEDGER)),
        );
    }
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
