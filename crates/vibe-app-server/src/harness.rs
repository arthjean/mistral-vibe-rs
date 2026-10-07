//! Which harness a session runs on, and what `config/read` says about it.
//!
//! The reference serves a session from one of two backends: its legacy Python
//! harness, or the Unified Harness, a native core selected by
//! `--experimental-harness`, by `--smart-approve`, or by the
//! `vibe_cli_unified_harness_rollout` experiment
//! (`vibe/_experimental_harness.py:96-121`). This port selects the same way.
//! Its unified mode is the legacy runtime with the parts of the Unified
//! Harness ported so far switched on: plugin resolution (row 35) and the
//! `runtime.experimentalHarness` flag clients gate on. The rest of that
//! backend (its native core, todos, smart approve, child sessions) belongs to
//! row 36.

use std::path::Path;

use serde_json::{Value, json};
use vibe_core::experiments::{EvalCache, ExperimentName};

/// Where the harness choice came from, spelled as `harnessSelectionSource`
/// publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSelectionSource {
    /// `--experimental-harness`, directly or through `--smart-approve`.
    Flag,
    /// The `vibe_cli_unified_harness_rollout` experiment, read from the eval
    /// cache a previous session wrote.
    Rollout,
    Default,
    /// `--legacy-harness`, which wins over everything else.
    FlagLegacy,
}

impl HarnessSelectionSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Rollout => "rollout",
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

impl Default for HarnessSelection {
    fn default() -> Self {
        Self::resolve(false, false)
    }
}

impl HarnessSelection {
    /// Reference `resolve_harness_selection` without a rollout cache.
    #[must_use]
    pub fn resolve(experimental_harness: bool, legacy_harness: bool) -> Self {
        Self::resolve_with_rollout(experimental_harness, legacy_harness, None)
    }

    /// Reference `resolve_harness_selection`: `--legacy-harness` first, then
    /// `--experimental-harness`, then a rollout variant of `unified`, then the
    /// legacy default. The reference's fallback for a backend that cannot be
    /// created never applies here, since the unified mode is always present.
    #[must_use]
    pub fn resolve_with_rollout(
        experimental_harness: bool,
        legacy_harness: bool,
        rollout_variant: Option<&str>,
    ) -> Self {
        let (use_unified, source) = if legacy_harness {
            (false, HarnessSelectionSource::FlagLegacy)
        } else if experimental_harness {
            (true, HarnessSelectionSource::Flag)
        } else if rollout_variant == Some("unified") {
            (true, HarnessSelectionSource::Rollout)
        } else {
            (false, HarnessSelectionSource::Default)
        };
        Self {
            use_unified,
            source,
        }
    }

    /// The selection a launch resolves, the rollout read from the eval cache
    /// under `vibe_home` whatever the telemetry opt-in. Reference
    /// `HarnessProcess.__init__` with `_load_rollout_cache`.
    #[must_use]
    pub fn for_launch(experimental_harness: bool, legacy_harness: bool, vibe_home: &Path) -> Self {
        let rollout =
            EvalCache::new(vibe_home).rollout_variant(ExperimentName::UnifiedHarnessRollout.key());
        Self::resolve_with_rollout(experimental_harness, legacy_harness, rollout.as_deref())
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
