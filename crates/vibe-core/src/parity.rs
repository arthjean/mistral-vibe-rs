//! The pinned Python reference every parity oracle in this workspace measures
//! against.
//!
//! Six test modules across three crates used to carry their own copy of the
//! commit hash and four of them their own copy of the checkout path. A re-pin
//! was therefore six edits that nothing forced to agree, and a corpus captured
//! from one commit could sit next to a constant naming another without any test
//! noticing. This module is the one place the pin is written; every oracle
//! cites it, and `scripts/parity/pin.py` mirrors it for the capture scripts
//! under a test that fails when the two drift.
//!
//! The module lives in production code rather than behind `#[cfg(test)]`
//! because the oracles that read it are unit tests in three separate crates,
//! and a `#[cfg(test)]` item is not visible across a crate boundary.
//!
//! Nothing at runtime reads any of this: the binaries never consult the
//! reference, which is a developer-workstation artifact.

use std::path::{Path, PathBuf};

/// The reference commit every committed corpus in this repository was captured
/// from. A checkout at any other revision is not an oracle for those corpora.
///
/// Re-pinning means changing this constant, the same value in
/// `scripts/parity/pin.py`, and regenerating every committed corpus in the same
/// change.
pub const REFERENCE_COMMIT: &str = "376f6a33413a3eec9b3795b0c0e004066c47b5c1";

/// The package version [`REFERENCE_COMMIT`] publishes, recorded so a reader can
/// match the pin against an upstream release without resolving the hash.
pub const REFERENCE_VERSION: &str = "2.26.0";

/// Where the read-only reference checkout lives by default. Machine-dependent:
/// [`REFERENCE_VARIABLE`] overrides it.
pub const REFERENCE_ROOT: &str = "/home/arthur/dev/mistral-vibe";

/// The environment variable that relocates the checkout, under the same name
/// the capture scripts read.
pub const REFERENCE_VARIABLE: &str = "VIBE_REFERENCE";

/// The single documented command that returns a checkout to the pin. Since
/// probes gate on a materialized environment ([`live`]) instead of the
/// checkout's `HEAD`, nothing requires it; US-029 of
/// `tasks/prd-trustworthy-parity-v3.md` decides whether it goes.
pub const RESTORE_COMMAND: &str =
    "git -C /home/arthur/dev/mistral-vibe checkout 376f6a33413a3eec9b3795b0c0e004066c47b5c1";

/// The checkout root this run should use: [`REFERENCE_VARIABLE`] when set,
/// otherwise [`REFERENCE_ROOT`]. The checkout is only the object store
/// `scripts/parity/repin.py materialize` reads with `git archive`: no probe
/// gates on its `HEAD` or runs its working tree (see [`live`]).
#[must_use]
pub fn reference_root() -> PathBuf {
    std::env::var_os(REFERENCE_VARIABLE)
        .map_or_else(|| PathBuf::from(REFERENCE_ROOT), PathBuf::from)
}

/// The interpreter of the environment at `root`, a materialized reference
/// environment (`.parity/reference-<sha12>/`, laid out like a checkout), in
/// its POSIX layout first and its Windows one second, or [`None`] when it has
/// neither.
///
/// No variable substitutes another interpreter: the environment at `root` is
/// the one [`live::gate`] checked against its attestation.
#[must_use]
pub fn pinned_interpreter(root: &Path) -> Option<PathBuf> {
    [
        root.join(".venv/bin/python"),
        root.join(".venv/Scripts/python.exe"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

#[cfg(any(test, feature = "parity-replay"))]
pub mod figures;
#[cfg(any(test, feature = "parity-replay"))]
pub mod live;
#[cfg(any(test, feature = "parity-replay"))]
pub mod manifest;
#[cfg(any(test, feature = "parity-replay"))]
pub mod replay;

#[cfg(test)]
mod capture_guard_tests;
#[cfg(test)]
mod census_tests;
#[cfg(test)]
mod ledger_tests;
#[cfg(test)]
mod licensing_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod manifest_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod repin_tests;
#[cfg(test)]
mod scorecard_tests;
#[cfg(test)]
mod workflow_tests;
