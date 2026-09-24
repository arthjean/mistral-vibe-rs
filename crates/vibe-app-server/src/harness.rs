//! Which harness a session runs on, and what `config/read` says about it.
//!
//! The reference serves a session from one of two backends: its legacy Python
//! harness, or the Unified Harness, a native core selected by
//! `--experimental-harness`, by `--smart-approve`, or by the
//! `vibe_cli_unified_harness_rollout` experiment
//! (`vibe/_experimental_harness.py:96-121`). This port is the legacy harness
//! only. It still resolves the selection the way the reference does, because
//! the answer is observable: `config/read` publishes where the choice came
//! from, and a request for the Unified Harness that cannot be honored lands on
//! the legacy one with a startup issue the terminal client shows
//! (`vibe/app_server/_runtime.py:1135-1167`).

use serde::Serialize;
use serde_json::{Value, json};

/// Where the harness choice came from, spelled as `harnessSelectionSource`
/// publishes it. The reference's fourth source, `rollout`, only ever selects
/// an installed Unified Harness, so it has no counterpart here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSelectionSource {
    /// `--experimental-harness`, directly or through `--smart-approve`.
    Flag,
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

/// A configuration issue raised before any session exists, in the
/// `ConfigIssue` shape: the input it concerns and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartupIssue {
    pub file: String,
    pub message: String,
}

/// The resolved harness decision for one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSelection {
    pub source: HarnessSelectionSource,
    pub startup_issue: Option<StartupIssue>,
}

impl Default for HarnessSelection {
    fn default() -> Self {
        Self::resolve(false, false)
    }
}

impl HarnessSelection {
    /// Reference `resolve_harness_selection` followed by the fallback
    /// `HarnessProcess` applies when the Unified Harness cannot be created.
    ///
    /// The precedence is the reference's: `--legacy-harness` first, then
    /// `--experimental-harness`, then the rollout, then the default. The
    /// rollout step needs the Unified Harness installed, which it never is
    /// here, so it always falls through; the flag step selects it anyway and
    /// then falls back, keeping `flag` as the source and raising the issue.
    #[must_use]
    pub fn resolve(experimental_harness: bool, legacy_harness: bool) -> Self {
        if legacy_harness {
            return Self {
                source: HarnessSelectionSource::FlagLegacy,
                startup_issue: None,
            };
        }
        if experimental_harness {
            return Self {
                source: HarnessSelectionSource::Flag,
                startup_issue: Some(StartupIssue {
                    file: "--experimental-harness".to_owned(),
                    message: "This build carries no Unified Harness backend, so the session \
                              runs on the legacy harness."
                        .to_owned(),
                }),
            };
        }
        Self {
            source: HarnessSelectionSource::Default,
            startup_issue: None,
        }
    }

    /// The two `ConfigReadResponse` fields this decision answers.
    #[must_use]
    pub fn config_read_fields(&self) -> [(&'static str, Value); 2] {
        [
            ("startupIssue", json!(self.startup_issue)),
            ("harnessSelectionSource", json!(self.source.as_str())),
        ]
    }
}

#[cfg(test)]
mod harness_tests;
