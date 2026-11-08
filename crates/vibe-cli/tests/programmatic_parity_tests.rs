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
//! From v3.0.0 the reference runs one backend, the Unified Harness, so one
//! corpus records it; the legacy backend's corpus went with the escape hatch.

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries,
    pinned_reference, scenarios,
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

/// What the port answers otherwise and the scorecard decided; none since
/// the v3.1.0 re-pin, whose gaps are `V3_GAPS`.
const LEDGER: &[PatternDivergence] = &[];

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
/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 runs a programmatic session from its Rust launcher on the Unified Harness, which this port does not reproduce yet";

/// Differences an own-words entry covers but whose length v3.1.0 moved out of
/// its band, one by one; chained first, so they are decided before it.
const V3_REFUSED: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "config/active-provider-missing-key",
        pointer: "/0/stderr/0/message",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "failure/missing-key-before-empty-prompt",
        pointer: "/0/stderr/0/message",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/*/exit",
        count: 13,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stderr",
        count: 8,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout",
        count: 5,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/requests/*",
        count: 5,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stderr/*",
        count: 19,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/detail/input/changes",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/detail/input/newString",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/detail/input/oldString",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/detail/input/replaceAll",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/detail/toolName",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/outcome",
        count: 23,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/state/display/message",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/state/output/previousContent",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/title",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/lines/*",
        count: 4,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/lines/*/json/outcome",
        count: 13,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/lines/*/pythonRendering",
        count: 14,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/requests/*/tools/*",
        count: 610,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/json/*/state/output/occurrences/*/newStartLine",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/layout/*/*",
        count: 23,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/lines/*/json/content/*/text",
        count: 2,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/lines/*/layout/*",
        count: 13,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/layout/*/*/*/*/*/*",
        count: 3,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/layout/*/*/*/*/*/*/*",
        count: 2,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/layout/*/*/*/*/*/*/*/*/*",
        count: 1,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stdout/layout/*/*/*/*/*/*/*/*/*/*",
        count: 2,
        row: "13",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

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
    .run(
        entries(V3_REFUSED)
            .chain(entries(PROSE))
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(V3_REFUSED)
            .chain(entries(PROSE))
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
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
