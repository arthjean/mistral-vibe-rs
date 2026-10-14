//! Which harness a session runs on, and what `config/read` says about it.
//!
//! Since v2.26.0 the reference serves every session from its Unified Harness
//! and keeps the legacy backend behind `--legacy-harness`, an escape hatch it
//! removes at a cutover it announces; `--experimental-harness` (and
//! `--smart-approve`, which implies it) is now redundant, and the
//! `vibe_cli_unified_harness_rollout` experiment is no longer read
//! (`vibe/_experimental_harness.py:96-137`). This port selects the same way.

use serde_json::{Value, json};

/// Where the harness choice came from, spelled as `harnessSelectionSource`
/// publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSelectionSource {
    /// `--experimental-harness`, directly or through `--smart-approve`.
    Flag,
    /// No flag: the Unified Harness.
    Default,
    /// `--legacy-harness`, which wins over everything else.
    FlagLegacy,
}

impl HarnessSelectionSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Default => "default",
            Self::FlagLegacy => "flag-legacy",
        }
    }
}

/// The resolved harness decision for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessSelection {
    pub use_unified: bool,
    pub source: HarnessSelectionSource,
}

/// The server a test or an in-process adapter builds without naming a
/// harness keeps the legacy backend: a launch always resolves its selection
/// from its own flags ([`HarnessSelection::resolve`]).
impl Default for HarnessSelection {
    fn default() -> Self {
        Self {
            use_unified: false,
            source: HarnessSelectionSource::Default,
        }
    }
}

impl HarnessSelection {
    /// Reference `resolve_harness_selection`: `--legacy-harness` first, and
    /// the Unified Harness otherwise.
    #[must_use]
    pub fn resolve(experimental_harness: bool, legacy_harness: bool) -> Self {
        if legacy_harness {
            return Self {
                use_unified: false,
                source: HarnessSelectionSource::FlagLegacy,
            };
        }
        Self {
            use_unified: true,
            source: if experimental_harness {
                HarnessSelectionSource::Flag
            } else {
                HarnessSelectionSource::Default
            },
        }
    }

    /// The two `ConfigReadResponse` fields this decision answers. The
    /// startup issue is the reference's report of a unified backend that could
    /// not be created, which this port never lacks.
    #[must_use]
    pub fn config_read_fields(&self) -> [(&'static str, Value); 2] {
        [
            ("startupIssue", Value::Null),
            ("harnessSelectionSource", json!(self.source.as_str())),
        ]
    }
}

#[cfg(test)]
mod harness_tests;
