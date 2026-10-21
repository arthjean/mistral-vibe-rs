//! Replays the committed workspace trust corpus against this port's app server.
//!
//! `scripts/parity/workspace_trust.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, builds a workspace tree and, where it says so, a
//! `trusted_folders.toml` with a given content and mode, then drives
//! `workspace/trust/status`, `workspace/trust/decision`,
//! `workspace/trust/untrustedConfig`, `session/start` with and without
//! `trustWorkspace`, and `config/read` over a project file the trust store has
//! to allow. A scenario records every answer and refusal, the
//! `runtime/updated` a step raised, and the trust file's content and mode after
//! each step that asks for it. Driving the `vibe-app-server-stdio-fixture`
//! binary through the same script with `--server` yields observations
//! normalized the same way, and the replay compares the two scenario by
//! scenario. It needs no reference checkout; only the live probe at the end,
//! which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. This port writes its own prose on purpose (`NOTICE`), so two
//! digests count as equal wherever both sides hold one, and everything else,
//! the trust file's bytes included, is compared exactly. Every other difference
//! has to fall under a `LEDGER` entry, and every entry has to still reproduce,
//! so row 12 of `docs/parity.md` is a reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use vibe_core::parity::replay::{Capture, PatternDivergence, Prose, Replay, entries};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("workspace-trust/corpus.json");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/workspace_trust.py",
    family: &[],
    arguments: &[],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 29;

const LEDGER: &[PatternDivergence] = &[];

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the corpus records POSIX file modes and a directory name with a double quote, which NTFS forbids"
)]
fn the_port_answers_every_trust_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "workspace trust parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("workspace-trust-port.json"),
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
    CAPTURE.probe("workspace trust");
}
