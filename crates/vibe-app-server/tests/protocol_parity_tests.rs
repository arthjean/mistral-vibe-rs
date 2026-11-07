//! Replays the committed protocol corpus against this port's app server.
//!
//! `scripts/parity/app_server_protocol.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the provider and console APIs,
//! and records the shape of every answer, notification and server request the
//! app-server protocol publishes, from a probe per declared method to the
//! flows that chain them (turns, callbacks, the queue, manual shells,
//! compaction, the skills registry, the connector catalog, git checkouts).
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! The script records shapes rather than values, and reduces the prose a
//! server authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`): two digests that
//! differ are a difference unless a `PROSE` entry names their pointer, holds
//! the count of digests it covers, and finds the port's length within its band
//! of the reference's; everything else is compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 17 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("protocol-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/protocol-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/app_server_protocol.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// What the port still answers differently. The v2.26.0 re-pin (`376f6a3`)
/// opened eleven fields the reference's public models gained and seven
/// methods this port neither declared nor routed, and the protocol pass closed
/// all of them.
const LEDGER: &[PatternDivergence] = &[];

/// Every text this port words its own way, digest by digest.
/// The accepted divergence the prose below stands on.
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";

const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &[
            "flows/loops",
            "flows/turn-queue-refusals",
            "probe/session/turn/queue/steer/rooted",
            "probe/vibeCode/projects/open/rooted",
        ],
        pointer: "/*/response/error/message",
        count: 7,
        band: Band::DEFAULT,
        directives: None,
        row: "17",
        accepted: SERVER_PROSE,
        reason: "the refusal of a loop, a queue steer or a project open the session's state does \
                 not allow",
    },
    OwnProse {
        scenarios: &[
            "flows/shell-command",
            "flows/turn-queue-refusals",
            "flows/worktree-reap",
            "probe/session/settings/update/rooted",
            "probe/session/shellCommand/rooted",
        ],
        pointer: "/*/response/error/data/issues/*/message",
        count: 11,
        band: Band::DEFAULT,
        directives: None,
        row: "17",
        accepted: SERVER_PROSE,
        reason: "the message of a validation issue, under the reference's issue path and kind",
    },
];

/// What only a Windows host answers otherwise: the corpus is captured on
/// Linux.
const PLATFORM: &[PatternDivergence] = if cfg!(windows) {
    &[PatternDivergence {
        scenario: "flows/shell-command",
        pointer: "/9/response/error/message",
        count: 1,
        row: "6",
        class: Class::Gap,
        reason: "the refusal of a working directory outside the workspace spells the directory \
                 with the host's own separators and drive, so its digest differs from the POSIX                  path the corpus records",
    }]
} else {
    &[]
};

#[test]
fn the_port_answers_every_protocol_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "protocol parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("protocol-parity-port.json"),
    }
    .run(
        entries(PROSE)
            .chain(entries(PLATFORM))
            .chain(entries(LEDGER)),
    );
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE)
            .chain(entries(PLATFORM))
            .chain(entries(LEDGER)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("protocol");
}
