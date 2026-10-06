//! Rollout enrollment: which experiments this build knows, where a variant is
//! fetched from, and what a caller is allowed to read out of one.
//!
//! The evaluation is remote. In remote evaluation mode the GrowthBook proxy
//! performs the bucketing and rewrites every feature as a pre-resolved `force`
//! rule carrying the exposure metadata in `tracks`, so this client performs no
//! hashing, matches no condition and knows no namespace. What it does is
//! entirely local: build a URL, post a set of attributes, fail open, resolve a
//! value, drop the features it does not know, and keep configuration and
//! telemetry reading the same response differently.
//!
//! [`json`] holds the ordered JSON value the variants are carried as,
//! [`models`] the payload shapes, [`client`] the lookup and its fail-open
//! contract, [`manager`] what a resolved response means, and [`cache`] the
//! last response a user resolved, which the next session applies before its
//! own lookup answers.
//!
//! Reference: `vibe/core/experiments/` at the pinned commit.

use std::time::Duration;

mod cache;
mod client;
mod json;
mod manager;
mod models;
mod session;

pub use cache::{EVAL_CACHE_FILE_NAME, EVAL_CACHE_TTL, EvalCache};
pub use client::{EvalPayload, RemoteEvalClient};
pub use json::{JsonValue, OrderedMap};
pub use manager::{BUCKETING_KEY_LENGTH, ExperimentManager, hash_api_key};
pub use models::{
    EvalResponse, ExperimentAttributes, FeatureDefinition, FeatureRule, TrackData,
    TrackedExperiment, TrackedExperimentResult,
};
pub use session::{
    CredentialSource, EXPERIMENT_IDENTITY_TIMEOUT, ExperimentStateSink, PlanSources,
    build_attributes, hydrate_experiments_from_session, initialize_experiments,
    resolve_plan_attributes,
};

/// The transport the unit tests and the parity replay stand one call before a
/// connection.
#[cfg(test)]
pub mod recorder;

#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod experiments_tests;
#[cfg(test)]
mod json_tests;
#[cfg(test)]
mod manager_tests;
#[cfg(test)]
mod models_tests;
#[cfg(test)]
mod session_tests;

/// The eval path, under whichever host the configuration names.
///
/// Reference `GROWTHBOOK_EVAL_PATH_TEMPLATE`.
pub const EVAL_PATH_TEMPLATE: &str = "/api/eval/{client_key}";

/// How long one eval request is given, in seconds.
///
/// Reference `EVAL_REQUEST_TIMEOUT_SECONDS`. A lookup runs off the startup path
/// and fails open, so this bounds a cost rather than a correctness window.
pub const EVAL_REQUEST_TIMEOUT_SECONDS: f64 = 5.0;

/// [`EVAL_REQUEST_TIMEOUT_SECONDS`] as the duration the HTTP client is built
/// with.
pub const EVAL_REQUEST_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The backend a session runs on, which an experiment either has a consumer on
/// or does not.
///
/// Reference `ExperimentSurface`. This port serves every session from the
/// legacy backend, so `Unified` is spelled here only because the declaration
/// below names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExperimentSurface {
    Legacy,
    Unified,
}

impl ExperimentSurface {
    /// Every surface, in the order the reference declares them.
    pub const ALL: [Self; 2] = [Self::Legacy, Self::Unified];

    /// The value the `harness` attribute is posted as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Unified => "unified",
        }
    }
}

/// The experiments this build knows how to read.
///
/// A rollout keyed on anything else resolves to nothing here: the manager drops
/// unknown keys on the way in, so a feature defined for a newer build reaches
/// neither configuration nor telemetry.
///
/// Reference `ExperimentName`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExperimentName {
    /// Which system prompt the session runs.
    SystemPrompt,
    /// Whether the managed shell family replaces the one-shot shell tool.
    ManagedShellTools,
    /// Which model routing answers with, carried as a JSON payload.
    CliModelRouting,
    /// Whether smart approve is offered in the mode picker.
    SmartApprove,
    /// Whether smart approve is the mode a session starts in.
    SmartApproveDefault,
    /// Models added to the picker without changing the default.
    CliExtraModels,
    /// Whether registry skills are synced and loaded.
    RegistrySkills,
    /// Which harness a launch selects.
    UnifiedHarnessRollout,
}

impl ExperimentName {
    /// Every name, in the order the reference declares them.
    pub const ALL: [Self; 8] = [
        Self::SystemPrompt,
        Self::ManagedShellTools,
        Self::CliModelRouting,
        Self::SmartApprove,
        Self::SmartApproveDefault,
        Self::CliExtraModels,
        Self::RegistrySkills,
        Self::UnifiedHarnessRollout,
    ];

    /// The feature key a rollout is defined under.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::SystemPrompt => "vibe_cli_system_prompt",
            Self::ManagedShellTools => "vibe_cli_managed_shell_tools",
            Self::CliModelRouting => "vibe_cli_default_routing_model",
            Self::SmartApprove => "vibe_cli_smart_approve",
            Self::SmartApproveDefault => "vibe_cli_smart_approve_default",
            Self::CliExtraModels => "vibe_cli_extra_models",
            Self::RegistrySkills => "vibe_cli_registry_skills",
            Self::UnifiedHarnessRollout => "vibe_cli_unified_harness_rollout",
        }
    }

    /// What this build resolves to when the rollout says nothing.
    ///
    /// The value is typed as the reference types it: a text, an object or a
    /// flag, so the configuration variants can drop a value equal to it by
    /// comparing values rather than spellings. The reference pairs its names
    /// and its defaults in a dictionary held together by a module-level
    /// assertion; an exhaustive match makes a name added without a default
    /// fail to compile instead.
    ///
    /// Reference `DEFAULT_VARIANTS`.
    #[must_use]
    pub fn default_variant(self) -> JsonValue {
        match self {
            Self::SystemPrompt => JsonValue::String("cli".to_owned()),
            Self::ManagedShellTools | Self::UnifiedHarnessRollout => {
                JsonValue::String("legacy".to_owned())
            }
            Self::CliModelRouting | Self::CliExtraModels => JsonValue::Object(OrderedMap::new()),
            Self::SmartApprove | Self::SmartApproveDefault | Self::RegistrySkills => {
                JsonValue::Bool(false)
            }
        }
    }

    /// The surfaces this experiment has a consumer on, declared rather than
    /// inferred: the managed shell family is read by the legacy tool manager
    /// alone, and smart approve's classifier lives in the Unified Harness.
    ///
    /// Reference `EXPERIMENT_SURFACES`.
    #[must_use]
    pub const fn surfaces(self) -> &'static [ExperimentSurface] {
        match self {
            Self::ManagedShellTools => &[ExperimentSurface::Legacy],
            Self::SmartApprove | Self::SmartApproveDefault => &[ExperimentSurface::Unified],
            Self::SystemPrompt
            | Self::CliModelRouting
            | Self::CliExtraModels
            | Self::RegistrySkills
            | Self::UnifiedHarnessRollout => &ExperimentSurface::ALL,
        }
    }

    /// Whether an exposure to this experiment may be reported from `surface`.
    /// An inert variant still resolves; only the claim that it was served is
    /// withheld.
    ///
    /// Reference `is_exposure_eligible`.
    #[must_use]
    pub fn is_exposure_eligible(self, surface: ExperimentSurface) -> bool {
        self.surfaces().contains(&surface)
    }

    /// The name one feature key stands for, or [`None`] when this build does
    /// not know it.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|name| name.key() == key)
    }
}

/// The eval endpoint one host and key resolve to, or [`None`] when either is
/// blank.
///
/// The host is trimmed and stripped of every trailing slash, so a host written
/// with one, without one or with three all address the same endpoint, and a
/// host that is nothing but slashes is as empty as the empty string. An unset
/// URL is not an error: it is a client that issues no request at all.
///
/// Reference `build_eval_url`.
#[must_use]
pub fn build_eval_url(api_host: &str, client_key: &str) -> Option<String> {
    let api_host = api_host.trim().trim_end_matches('/');
    let client_key = client_key.trim();
    if api_host.is_empty() || client_key.is_empty() {
        return None;
    }
    Some(format!(
        "{api_host}{}",
        EVAL_PATH_TEMPLATE.replace("{client_key}", client_key)
    ))
}
