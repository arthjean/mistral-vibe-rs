//! The telemetry session replay `vibe` and `vibe-acp` share.
//!
//! `scripts/parity/telemetry_session.py` records every surface in one corpus:
//! `vibe -p` and the interactive client, which
//! `crates/vibe-cli/tests/telemetry_session_parity_tests.rs` replays, and the
//! editor connection, which `crates/vibe-acp/tests/telemetry_session_parity_tests.rs`
//! replays. Each test binary drives the surfaces its own binary serves.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Entry, PatternDivergence, Prose, Tally, differences, pinned_reference,
    scenario_matches, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("../telemetry-session/corpus.json");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/telemetry_session.py",
    family: &[],
    arguments: &[],
};

/// The surfaces one binary serves, and how the capture script drives it.
pub struct Surfaces {
    /// The surfaces the binary serves.
    pub served: &'static [&'static str],
    /// The surfaces replayed on this host, all or some of `served`.
    pub replayed: &'static [&'static str],
    /// The flag that hands the capture script the binary, and the binary.
    pub binary: [&'static str; 2],
    /// The scenarios the corpus may not fall below on `served`, so a
    /// recapture that lost coverage fails here and not only on the machine
    /// that made it.
    pub floor: usize,
}

impl Surfaces {
    /// Replays every scenario recorded on the replayed surfaces and compares
    /// it with the corpus under `ledger`.
    pub fn replay(&self, ledger: &[PatternDivergence], output: &Path) {
        let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
        let served = on_surfaces(&corpus, self.served);
        assert!(
            served.len() >= self.floor,
            "the corpus holds {} scenarios on {:?}, below the floor of {}",
            served.len(),
            self.served,
            self.floor
        );
        let recorded = on_surfaces(&corpus, self.replayed);
        let mut arguments = self.binary.to_vec();
        arguments.extend(self.surface_arguments());
        let replayed = CAPTURE.port(&arguments, output);
        // An entry whose scenarios this host does not replay cannot reproduce.
        let mut tally = Tally::new(
            ledger
                .iter()
                .filter(|entry| {
                    recorded
                        .keys()
                        .any(|name| scenario_matches(entry.scenario, name))
                })
                .map(|entry| entry as &dyn Entry),
        );
        tally.compare(
            &CAPTURE,
            &recorded,
            &on_surfaces(&replayed, self.served),
            |reference, port| differences(reference, port, Prose::Compared),
        );
        tally.conclude("telemetry session parity:");
    }

    /// Asserts every entry of `ledger` names a scenario the corpus records on
    /// the served surfaces, a pointer and a reason.
    pub fn assert_ledger(&self, ledger: &[PatternDivergence]) {
        PatternDivergence::assert_ledger(ledger);
        let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
        let served = on_surfaces(&corpus, self.served);
        for entry in ledger {
            assert!(
                served
                    .keys()
                    .any(|name| scenario_matches(entry.scenario, name)),
                "the ledger entry for {} names {}, which the corpus does not record on {:?}",
                entry.pointer,
                entry.scenario,
                self.served
            );
        }
    }

    /// Recaptures the pinned reference on the replayed surfaces and asserts
    /// the committed corpus is still what it answers.
    pub fn check(&self, probe: &str) {
        if let Some(root) = pinned_reference(probe) {
            CAPTURE.check(&root, &self.surface_arguments());
        }
    }

    /// `--surface` for every surface replayed on this host.
    fn surface_arguments(&self) -> Vec<&'static str> {
        self.replayed
            .iter()
            .flat_map(|surface| ["--surface", surface])
            .collect()
    }
}

/// The scenarios `corpus` records on `surfaces`.
fn on_surfaces<'c>(corpus: &'c Value, surfaces: &[&str]) -> BTreeMap<String, &'c Value> {
    scenarios(corpus)
        .into_iter()
        .filter(|(_, scenario)| {
            surfaces.contains(&scenario["surface"].as_str().unwrap_or_default())
        })
        .collect()
}
