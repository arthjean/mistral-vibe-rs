//! Replays the committed plugins server corpus against this port's app server.
//!
//! `scripts/parity/plugins.py --family server` captured the corpus from the
//! pinned reference's own `vibe-app-server`: every scenario serves it over
//! stdio in a fresh home, selecting the unified harness with
//! `--experimental-harness` or with no flag over the rollout in the eval cache,
//! or the legacy one with `--legacy-harness`, and records what `plugin_catalog/read`, `plugins/read`, `plugin/info`,
//! `plugin/reload`, the plugin parts of `runtime/read` and `mcp_catalog/read`,
//! and the MCP mutations a plugin server refuses answer around a session start
//! and a reload. One scenario's plugin declares MCP servers: the
//! `vibe-mcp-stdio-fixture` binary found on `PATH`, a script the plugin ships,
//! and a command that does not exist. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 35 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, OwnProse, PatternDivergence, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("plugins-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/plugins-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/plugins.py",
    family: &["--family", "server"],
    arguments: &[
        "--mcp-fixture",
        env!("CARGO_BIN_EXE_vibe-mcp-stdio-fixture"),
    ],
};

const LEDGER: &[PatternDivergence] = &[];

/// Every text this port words its own way, digest by digest.
const PROSE: &[OwnProse] = &[];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: unified/mcp-servers ships a #!/bin/sh script as an MCP server, which CreateProcess cannot launch (os error 193)"
)]
fn the_port_serves_every_plugin_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "plugins parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("plugins-parity-port.json"),
    }
    .run(entries(PROSE).chain(entries(LEDGER)));
}

#[test]
fn every_ledger_entry_is_declared_soundly() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(LEDGER)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("plugins");
}
