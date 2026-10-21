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
//! server authored to a length and a SHA-256. This port writes its own prose
//! on purpose (`NOTICE`), so two digests count as equal wherever both sides
//! hold one, and everything else is compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 17 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]

use std::path::Path;

use vibe_core::parity::replay::{Capture, Entry, Prose, Replay, entries};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("protocol-parity/corpus.json");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/app_server_protocol.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 344;

/// A pointer this port answers differently in some scenarios, and why.
struct Divergence {
    /// The scenario it applies to, `*` for every one, or `<prefix>/*` for every
    /// scenario one segment below the prefix, which names both probes of one
    /// method.
    scenario: &'static str,
    /// Every difference whose JSON pointer ends with this suffix is covered; an
    /// empty suffix covers every difference in the scenario.
    suffix: &'static str,
    reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        let scenario_matches = self.scenario == "*"
            || self.scenario == scenario
            || self.scenario.strip_suffix("/*").is_some_and(|prefix| {
                scenario
                    .rsplit_once('/')
                    .is_some_and(|(parent, _)| parent == prefix)
            });
        scenario_matches && pointer.ends_with(self.suffix)
    }

    fn label(&self) -> String {
        self.reason.to_owned()
    }
}

/// What the port still answers differently. The v2.26.0 re-pin (`376f6a3`)
/// opened eleven fields the reference's public models gained and seven
/// methods this port neither declared nor routed, and the protocol pass closed
/// all of them.
const LEDGER: &[Divergence] = &[];

#[test]
fn the_port_answers_every_protocol_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "protocol parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("protocol-parity-port.json"),
        prose: Prose::OwnWords,
    }
    .run(entries(LEDGER));
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("protocol");
}
