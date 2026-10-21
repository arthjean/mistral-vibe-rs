//! Where a delivery goes and the credential it presents.
//!
//! Reference `get_user_agent` (`vibe/utils/http.py`), and in
//! `vibe/core/telemetry/send.py` the endpoint `TelemetryClient._get_telemetry_url`
//! derives from the provider's `api_base` and `get_mistral_provider_and_api_key`,
//! which only ever resolves a Mistral provider, so no third-party key reaches a
//! Mistral-controlled endpoint.

use secrecy::SecretString;
use toml::Table;
use url::Url;

use super::{TELEMETRY_DEFAULT_BASE_URL, TELEMETRY_PATH, VERSION};
use crate::llm::mistral::server_url;

/// The backend value that makes a provider Mistral's. A provider entry that
/// declares none is not Mistral's, which is what keeps a third-party key away
/// from the datalake.
const MISTRAL_BACKEND: &str = "mistral";
/// Reference `get_user_agent`: every request identifies as the Vibe client, and
/// a Mistral backend prefixes the SDK marker. Reproduced verbatim so a datalake
/// consumer written against the reference reads this port's deliveries without
/// a translation layer.
const USER_AGENT_PRODUCT: &str = "Mistral-Vibe";
const USER_AGENT_MISTRAL_PREFIX: &str = "mistral-client-python/";
/// Reference `get_user_agent`.
#[must_use]
pub fn telemetry_user_agent(backend: Option<&str>) -> String {
    let agent = format!("{USER_AGENT_PRODUCT}/{VERSION}");
    if backend == Some(MISTRAL_BACKEND) {
        return format!("{USER_AGENT_MISTRAL_PREFIX}{agent}");
    }
    agent
}

/// Reference `TelemetryClient._get_telemetry_url`: the server derived from the
/// provider's `api_base`, or the default one, joined with the datalake path.
///
/// The credential travels as a bearer token, so a base that is neither HTTPS
/// nor plain HTTP to a loopback host, or that carries a credential of its own,
/// resolves to nothing rather than being sent to. The reference's regex admits
/// any `http://` host; this port admits the loopback one only, the rule its
/// model endpoint follows, which is recorded in the accepted-divergence table
/// of `docs/parity.md`.
#[must_use]
pub fn telemetry_endpoint(api_base: &str) -> Option<Url> {
    let base = server_url(api_base).unwrap_or_else(|| TELEMETRY_DEFAULT_BASE_URL.to_owned());
    let endpoint = Url::parse(base.trim_end_matches('/'))
        .ok()?
        .join(TELEMETRY_PATH)
        .ok()?;
    if !crate::text::is_secure_transport(&endpoint)
        || endpoint.host_str().is_none()
        || endpoint.username() != ""
        || endpoint.password().is_some()
    {
        return None;
    }
    Some(endpoint)
}

/// Reference `VibeConfigSchema.get_mistral_provider`: the active model's
/// provider when its backend is Mistral, and otherwise the first Mistral
/// provider configured.
#[must_use]
pub fn mistral_provider(effective: &Table) -> Option<Table> {
    let providers = provider_entries(effective);
    if let Some(active) = active_provider(effective, &providers)
        && is_mistral(&active)
    {
        return Some(active);
    }
    providers.into_iter().find(is_mistral)
}

fn provider_entries(effective: &Table) -> Vec<Table> {
    effective
        .get("providers")
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(toml::Value::as_table)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The provider entry serving the active model, in either persisted model
/// shape: an array of entries carrying their own alias, or a table keyed by it.
///
/// The alias is resolved rather than read, because a document that pins nothing
/// carries the reference's unpinned sentinel.
fn active_provider(effective: &Table, providers: &[Table]) -> Option<Table> {
    let alias = crate::config::active_model_alias(effective)?;
    let model = match effective.get("models") {
        Some(toml::Value::Table(models)) => models.get(alias)?.as_table()?.clone(),
        Some(toml::Value::Array(models)) => models
            .iter()
            .filter_map(toml::Value::as_table)
            .find(|entry| {
                ["alias", "name"]
                    .into_iter()
                    .any(|key| entry.get(key).and_then(toml::Value::as_str) == Some(alias))
            })?
            .clone(),
        _ => return None,
    };
    let provider = model.get("provider")?.as_str()?;
    providers
        .iter()
        .find(|entry| entry.get("name").and_then(toml::Value::as_str) == Some(provider))
        .cloned()
}

/// Reference `VibeConfigSchema.is_active_model_mistral`: the provider the
/// active model runs on has the Mistral backend.
#[must_use]
pub fn is_active_model_mistral(effective: &Table) -> bool {
    active_provider(effective, &provider_entries(effective))
        .is_some_and(|provider| is_mistral(&provider))
}

fn is_mistral(provider: &Table) -> bool {
    provider.get("backend").and_then(toml::Value::as_str) == Some(MISTRAL_BACKEND)
}

/// Where one delivery goes and how it identifies itself, resolved from the
/// merged configuration the way the reference resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryTarget {
    pub endpoint: Url,
    pub user_agent: String,
    /// The variable the credential is read from. The value is never held here:
    /// what names a credential and what carries one stay apart.
    pub credential_variable: String,
}

impl TelemetryTarget {
    /// Reference `get_mistral_provider_and_api_key` up to the key lookup: the
    /// provider decides the endpoint, the user agent and the variable, and the
    /// caller decides how a variable becomes a credential.
    #[must_use]
    pub fn resolve(effective: &Table) -> Option<Self> {
        let provider = mistral_provider(effective)?;
        let endpoint = telemetry_endpoint(
            provider
                .get("api_base")
                .and_then(toml::Value::as_str)
                .unwrap_or_default(),
        )?;
        let variable = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        if variable.is_empty() {
            // Reference `resolve_api_key` answers nothing for an empty
            // variable, so a provider that names none resolves no credential.
            return None;
        }
        Some(Self {
            endpoint,
            user_agent: telemetry_user_agent(provider.get("backend").and_then(toml::Value::as_str)),
            credential_variable: variable.to_owned(),
        })
    }
}

/// Whether telemetry is on for this process, and what it would deliver to.
///
/// Resolved on every send rather than once at startup, which is what makes a
/// document edited mid-session decide the next event.
pub struct TelemetryConfig {
    pub(super) enabled: bool,
    pub(super) target: Option<TelemetryTarget>,
    pub(super) credential: Option<SecretString>,
}

impl TelemetryConfig {
    /// Reference `TelemetryClient._is_enabled` and
    /// `get_mistral_provider_and_api_key`: `enable_telemetry` defaults to true,
    /// and a delivery still needs a Mistral provider whose variable resolves.
    #[must_use]
    pub fn resolve(effective: &Table, credentials: &dyn Fn(&str) -> Option<String>) -> Self {
        let enabled = effective
            .get("enable_telemetry")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        let target = TelemetryTarget::resolve(effective);
        let credential = target
            .as_ref()
            .and_then(|target| credentials(&target.credential_variable))
            .filter(|value| !value.is_empty())
            .map(SecretString::from);
        Self {
            enabled,
            target,
            credential,
        }
    }

    /// What an unreadable configuration resolves to. Reference `_is_enabled`
    /// swallows the failure and answers `False`, so a document that cannot be
    /// read silences telemetry instead of failing the run.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            target: None,
            credential: None,
        }
    }

    /// Reference `TelemetryClient.is_active`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.credential.is_some() && self.target.is_some()
    }

    #[must_use]
    pub fn target(&self) -> Option<&TelemetryTarget> {
        self.target.as_ref()
    }
}
