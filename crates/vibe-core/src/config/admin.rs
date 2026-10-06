//! What an organization enforces: the managed configuration a session fetches
//! once it starts, and the outcome it reports.
//!
//! The document is served by the Vibe server under `vibe_base_url` and loaded
//! into [`super::ConfigLayerKind::Admin`], the layer every other one is
//! composed under. Every failure is silent for the session: the layer stays
//! as it was and only the outcome is reported. A document that parses but
//! fails validation is rolled back, so it never stays in the live layer to
//! break every later load.
//!
//! Reference: `vibe/core/config/admin_config.py`,
//! `vibe/core/config/layers/admin.py` and `vibe/app_server/_admin_config.py` at
//! the pinned commit.

use std::time::Duration;

use serde::Deserialize;
use toml::Table;

use super::LayeredConfig;
use crate::telemetry::mistral_provider;
use crate::telemetry::records::AdminConfigOutcome;

/// Appended to `vibe_base_url`. Reference `MANAGED_CONFIG_PATH`.
pub const MANAGED_CONFIG_PATH: &str = "/api/v1/code/managed-config";

/// Per-request budget. Reference `MANAGED_CONFIG_TIMEOUT`.
pub const MANAGED_CONFIG_TIMEOUT: Duration = Duration::from_secs(2);

/// Attempts before giving up. Reference `MANAGED_CONFIG_RETRY_TRIES`.
pub const MANAGED_CONFIG_RETRY_TRIES: u32 = 3;

/// The first backoff and its growth. Reference `async_retry(delay_seconds=0.5,
/// backoff_factor=2.0)` on `_fetch_response`.
const RETRY_DELAY: Duration = Duration::from_millis(500);
const RETRY_BACKOFF: u32 = 2;

/// The statuses a fetch retries. Reference `_RETRYABLE_HTTP_STATUS_CODES`.
const RETRYABLE_STATUSES: [u16; 9] = [408, 409, 425, 429, 500, 502, 503, 504, 529];

/// The document the server answers. Reference `ManagedConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManagedConfig {
    pub state: String,
    #[serde(default)]
    pub toml: Option<String>,
}

impl ManagedConfig {
    /// Reference `ManagedConfig.is_enabled`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.state == "enabled" && self.toml.as_deref().is_some_and(|toml| !toml.is_empty())
    }
}

/// What loading the layer came to. Reference `AdminConfigApplyResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminConfigApplyResult {
    pub outcome: AdminConfigOutcome,
    /// The top-level keys the layer enforces, sorted.
    pub enforced_keys: Vec<String>,
    pub error: Option<String>,
}

impl AdminConfigApplyResult {
    fn outcome(outcome: AdminConfigOutcome) -> Self {
        Self {
            outcome,
            enforced_keys: Vec::new(),
            error: None,
        }
    }

    fn failed(outcome: AdminConfigOutcome, error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::outcome(outcome)
        }
    }

    /// The event this outcome reports, or [`None`] for one the reference keeps
    /// to itself: a disabled layer and a missing key are expected.
    ///
    /// Reference `report_admin_config_outcome`.
    #[must_use]
    pub fn report(&self) -> Option<crate::telemetry::TelemetryRecord> {
        let applied = self.outcome == AdminConfigOutcome::Applied;
        let failed = matches!(
            self.outcome,
            AdminConfigOutcome::FetchFailed
                | AdminConfigOutcome::ParseFailed
                | AdminConfigOutcome::ApplyFailed
        );
        if !applied && !failed {
            return None;
        }
        if failed {
            crate::observability::log(
                crate::observability::LogLevel::Warning,
                &format!(
                    "Admin-managed config not applied outcome={} error={}",
                    self.outcome.label(),
                    self.error.as_deref().unwrap_or("None")
                ),
            );
        }
        Some(crate::telemetry::TelemetryRecord::AdminConfigApplied {
            outcome: self.outcome,
            nb_enforced_fields: applied.then_some(self.enforced_keys.len() as u64),
            has_error: !applied && self.error.is_some(),
        })
    }
}

/// Fetches the managed document, retrying a transient failure.
///
/// Reference `fetch_managed_config`: the error is a message, the document is
/// absent on any failure.
pub async fn fetch_managed_config(base_url: &str, api_key: &str) -> Result<ManagedConfig, String> {
    let url = format!("{}{MANAGED_CONFIG_PATH}", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(MANAGED_CONFIG_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let mut delay = RETRY_DELAY;
    let mut attempt = 1;
    let body = loop {
        let outcome = client
            .get(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await;
        let retryable = match &outcome {
            Ok(response) => RETRYABLE_STATUSES.contains(&response.status().as_u16()),
            Err(error) => error.is_timeout() || error.is_connect() || error.is_request(),
        };
        if !retryable || attempt >= MANAGED_CONFIG_RETRY_TRIES {
            let response = outcome.map_err(|error| error.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            break response.text().await.map_err(|error| error.to_string())?;
        }
        tokio::time::sleep(delay).await;
        delay *= RETRY_BACKOFF;
        attempt += 1;
    };
    serde_json::from_str(&body).map_err(|error| error.to_string())
}

/// Fetches what the organization enforces and loads it into the admin layer.
///
/// Reference `refresh_admin_layer`: a Mistral provider whose key resolves is
/// required, a disabled or empty document leaves the layer as it was, and a
/// document the merged configuration refuses is rolled back.
pub async fn refresh_admin_layer(
    config: &LayeredConfig,
    credentials: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> AdminConfigApplyResult {
    let Ok(snapshot) = config.load() else {
        return AdminConfigApplyResult::outcome(AdminConfigOutcome::NoApiKey);
    };
    let effective = snapshot.effective;
    let api_key = mistral_provider(&effective)
        .and_then(|provider| {
            provider
                .get("api_key_env_var")
                .and_then(toml::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .filter(|variable| !variable.is_empty())
        .and_then(|variable| credentials(&variable))
        .filter(|key| !key.is_empty());
    let Some(api_key) = api_key else {
        return AdminConfigApplyResult::outcome(AdminConfigOutcome::NoApiKey);
    };
    let base_url = effective
        .get("vibe_base_url")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let managed = match fetch_managed_config(&base_url, &api_key).await {
        Ok(managed) => managed,
        Err(error) => {
            return AdminConfigApplyResult::failed(AdminConfigOutcome::FetchFailed, error);
        }
    };
    let Some(text) = managed.toml.as_deref().filter(|_| managed.is_enabled()) else {
        return AdminConfigApplyResult::outcome(AdminConfigOutcome::Disabled);
    };
    load_admin_layer(config, text)
}

/// Loads one document into the layer, rolling it back when the merged
/// configuration refuses it. Reference `_load_admin_layer`.
fn load_admin_layer(config: &LayeredConfig, text: &str) -> AdminConfigApplyResult {
    let values = match text.parse::<Table>() {
        Ok(values) => values,
        Err(error) => {
            return AdminConfigApplyResult::failed(
                AdminConfigOutcome::ParseFailed,
                error.to_string(),
            );
        }
    };
    let mut enforced_keys: Vec<String> = values.keys().cloned().collect();
    enforced_keys.sort();
    let previous = config.set_admin(values);
    if let Err(error) = config.load() {
        config.set_admin(previous);
        drop(config.load());
        return AdminConfigApplyResult::failed(AdminConfigOutcome::ApplyFailed, error.to_string());
    }
    AdminConfigApplyResult {
        outcome: AdminConfigOutcome::Applied,
        enforced_keys,
        error: None,
    }
}
