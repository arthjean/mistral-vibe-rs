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
//! [`json`] holds the ordered JSON value the variant strings are produced from,
//! [`models`] the payload shapes, [`client`] the lookup and its fail-open
//! contract, and [`manager`] what a resolved response means.
//!
//! Reference: `vibe/core/experiments/` at the pinned commit.

use std::time::Duration;

mod client;
mod json;
mod manager;
mod models;
mod session;

pub use client::{
    EvalFailure, EvalFuture, EvalHttpResponse, EvalPayload, EvalRequest, EvalTransport,
    EvalTransportError, RemoteEvalClient, ReqwestEvalTransport,
};
pub use json::{JsonValue, OrderedMap};
pub use manager::{BUCKETING_KEY_LENGTH, ExperimentManager, hash_api_key};
pub use models::{
    EvalResponse, ExperimentAttributes, FeatureDefinition, FeatureRule, TrackData,
    TrackedExperiment, TrackedExperimentResult,
};
pub use session::{
    CredentialSource, EXPERIMENT_IDENTITY_TIMEOUT, ExperimentStateSink, build_attributes,
    experiments_allowed, hydrate_experiments_from_session, initialize_experiments,
    mistral_provider_and_api_key,
};

/// The transport the unit tests and the parity replay stand one call before a
/// connection.
#[cfg(test)]
pub mod recorder;

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
}

impl ExperimentName {
    /// Every name, in the order the reference declares them.
    pub const ALL: [Self; 3] = [
        Self::SystemPrompt,
        Self::ManagedShellTools,
        Self::CliModelRouting,
    ];

    /// The feature key a rollout is defined under.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::SystemPrompt => "vibe_cli_system_prompt",
            Self::ManagedShellTools => "vibe_cli_managed_shell_tools",
            Self::CliModelRouting => "vibe_cli_default_routing_model",
        }
    }

    /// What this build resolves to when the rollout says nothing.
    ///
    /// The reference pairs its names and its defaults in a dictionary and holds
    /// them together with a module-level assertion. Here the pairing is an
    /// exhaustive match, so a name added without a default does not compile
    /// rather than failing at import.
    ///
    /// Reference `DEFAULT_VARIANTS`.
    #[must_use]
    pub const fn default_variant(self) -> &'static str {
        match self {
            Self::SystemPrompt => "cli",
            Self::ManagedShellTools => "legacy",
            // The routing variant is a JSON payload, so its default is an empty
            // object rather than an empty string.
            Self::CliModelRouting => "{}",
        }
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
