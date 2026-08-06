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
use std::process::Command;

/// The reference commit every committed corpus in this repository was captured
/// from. A checkout at any other revision is not an oracle for those corpora.
///
/// Re-pinning means changing this constant, the same value in
/// `scripts/parity/pin.py`, and regenerating every committed corpus in the same
/// change.
pub const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";

/// The package version [`REFERENCE_COMMIT`] publishes, recorded so a reader can
/// match the pin against an upstream release without resolving the hash.
pub const REFERENCE_VERSION: &str = "2.23.3";

/// Where the read-only reference checkout lives by default. Machine-dependent:
/// [`REFERENCE_VARIABLE`] overrides it.
pub const REFERENCE_ROOT: &str = "/home/arthur/dev/mistral-vibe";

/// The environment variable that relocates the checkout, under the same name
/// the capture scripts read.
pub const REFERENCE_VARIABLE: &str = "VIBE_REFERENCE";

/// The single documented command that returns a checkout to the pin. Quoted by
/// `docs/parity.md` and by the skip message below, so an operator whose probe
/// went quiet reads the fix where the problem is reported.
pub const RESTORE_COMMAND: &str =
    "git -C /home/arthur/dev/mistral-vibe checkout 68ff32e6a92e80a874c8153312f0aa8ae4955477";

/// The checkout root this run should use: [`REFERENCE_VARIABLE`] when set,
/// otherwise [`REFERENCE_ROOT`].
#[must_use]
pub fn reference_root() -> PathBuf {
    std::env::var_os(REFERENCE_VARIABLE)
        .map_or_else(|| PathBuf::from(REFERENCE_ROOT), PathBuf::from)
}

/// Why the local checkout cannot act as the oracle for `probe`, or [`None`]
/// when it can.
///
/// A missing checkout is the ordinary CI case and says so plainly. A checkout
/// sitting at another commit is the case that used to skip into silence, so its
/// message names the expected commit, the one actually found, and the command
/// that reconciles them.
#[must_use]
pub fn off_pin_reason(root: &Path, probe: &str) -> Option<String> {
    if !root.is_dir() {
        return Some(format!(
            "skipping the live {probe} probe: no reference checkout at {}",
            root.display()
        ));
    }
    let revision = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !revision.status.success() {
        return Some(format!(
            "skipping the live {probe} probe: {} is not a readable git checkout",
            root.display()
        ));
    }
    let head = String::from_utf8(revision.stdout).ok()?;
    let head = head.trim();
    if head == REFERENCE_COMMIT {
        return None;
    }
    Some(format!(
        "skipping the live {probe} probe: {} is at {head}, not the pinned {REFERENCE_COMMIT} \
         (v{REFERENCE_VERSION}); restore it with `{RESTORE_COMMAND}`",
        root.display()
    ))
}

/// The pinned checkout together with an interpreter that can import the
/// reference package, or [`None`] when this machine cannot act as the oracle.
///
/// `VIBE_PARITY_PYTHON` names an interpreter for a checkout whose virtual
/// environment sits elsewhere; otherwise the in-tree one is used, POSIX layout
/// first and Windows second.
#[must_use]
pub fn pinned_interpreter(root: &Path) -> Option<PathBuf> {
    std::env::var_os("VIBE_PARITY_PYTHON")
        .map(PathBuf::from)
        .into_iter()
        .chain([
            root.join(".venv/bin/python"),
            root.join(".venv/Scripts/python.exe"),
        ])
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod parity_tests;
