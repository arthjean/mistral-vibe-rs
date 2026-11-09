//! Replays the committed hooks corpus against this port's app server.
//!
//! `scripts/parity/hooks.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, writes the
//! `hooks.toml` files it declares, and drives turns whose tool calls the hooks
//! guard. Each hook runs a recorder that keeps what it read on stdin and
//! answers as the scenario scripted. The corpus holds the hook notices each
//! turn raised among the transcript entries, every invocation a hook read, the
//! conversation the model was sent, the tool calls the session persisted, the
//! hooks that outlived their wait, and the count and issues `runtime/read`,
//! `diagnostics/list` and `config/read` report as the files change. Driving
//! the `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. Unlike the session corpus, two digests are compared exactly: the
//! notices, denials and retry messages a hook run produces are the contract
//! this part measures, and this port writes them as the reference does.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry or the
//! corpus's measured debt (`vibe_core::parity::debt`), or a `FIELDS` entry for
//! a field that differs wherever it appears, and every entry and every debt
//! group has to still reproduce, so row 18 of `docs/parity.md` is a reading of
//! the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, OwnProse, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("hooks-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/hooks-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/hooks.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

const TOOL_RESULTS: &str = "Authored result and error text in six tools";

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &["post_tool/tool-failure"],
    pointer: "/conversations/1/2/content",
    count: 1,
    band: Band::DEFAULT,
    directives: None,
    row: "3",
    accepted: TOOL_RESULTS,
    reason: "`read_file` reports a missing file in this port's own sentence (`NOTICE`), which \
                 the model reads under the reference's `<tool_error>read_file failed: ` frame",
}];

#[test]
fn the_port_runs_every_hook_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "hooks parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("hooks-parity-port.json"),
    }
    .run(entries(PROSE));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(LISTED, &scenarios(&corpus), entries(PROSE));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("hooks");
}
