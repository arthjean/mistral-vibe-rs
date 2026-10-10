//! The session-less `setup/*` methods an onboarding client drives before any
//! session exists: the seed its wizard opens on, the key it stores, and the
//! choices it submits.
//!
//! Reference `vibe/app_server/_setup.py`. No session is opened, no trust gate
//! fires and no runtime is built: every answer reads the configuration the
//! process resolved, and every write goes through the same persistence the
//! terminal's own onboarding uses.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use toml::{Table, Value as TomlValue};
use vibe_core::auth::{
    PersistOutcome, ProviderCredentialsRequest, effective_browser_auth_url, persist_api_key,
    persist_provider_credentials, resolve_api_key, resolve_api_key_provider,
    supports_browser_sign_in,
};
use vibe_core::config::{ConfigSnapshot, DotenvValues, global_env_file};
use vibe_protocol::ProtocolErrorCode;

use super::{WorkspaceDispatch, WorkspaceService, WorkspaceServiceError};

/// The reason every onboarding write carries.
const ONBOARDING: &str = "onboarding";

/// Reference `SetupProviderView`: the six provider fields a wizard reads and
/// changes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderView {
    name: String,
    api_base: String,
    #[serde(default)]
    api_key_env_var: String,
    #[serde(default)]
    browser_auth_base_url: Option<String>,
    #[serde(default)]
    browser_auth_api_base_url: Option<String>,
    #[serde(default)]
    browser_auth_allow_origin_rewrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StatusParams {
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StoreCredentialParams {
    provider: String,
    api_key: String,
    #[serde(default)]
    custom_domain: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SubmitChoicesParams {
    #[serde(default)]
    provider: Option<ProviderView>,
    #[serde(default)]
    console_base_url: Option<String>,
    #[serde(default)]
    vibe_base_url: Option<String>,
    #[serde(default)]
    theme: Option<String>,
}

fn parse<T: serde::de::DeserializeOwned>(
    params: &BTreeMap<String, Value>,
) -> Result<T, WorkspaceServiceError> {
    serde_json::from_value(Value::Object(
        params.clone().into_iter().collect::<Map<_, _>>(),
    ))
    .map_err(|error| WorkspaceServiceError::InvalidParams(error.to_string()))
}

impl WorkspaceService {
    /// The process environment with the vibe home's dotenv filling in, and the
    /// keys this run stored on top, which is the environment the reference's
    /// own `os.environ` holds once its setup stored a key.
    pub(super) fn credential_environment(&self) -> BTreeMap<String, String> {
        let mut environ = DotenvValues::global(&self.paths.vibe_home).environment();
        if let Ok(stored) = self.stored_keys.lock() {
            environ.extend(
                stored
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        environ
    }

    fn setup_snapshot(&self) -> Result<ConfigSnapshot, WorkspaceServiceError> {
        self.config
            .load()
            .map_err(|error| WorkspaceServiceError::Config(error.to_string()))
    }

    /// Reference `SetupRequestHandler._status`: the seed a wizard opens on,
    /// for the provider a failed session named or else the active one.
    pub(super) fn setup_status(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let params: StatusParams = parse(params)?;
        let snapshot = self.setup_snapshot()?;
        let provider = match params.provider.as_deref() {
            None => seed_provider(&snapshot),
            Some(name) => named_provider(&snapshot, name)?,
        };
        let env_var = text(&provider, "api_key_env_var");
        // An empty variable names no secret to resolve, so it is not missing.
        let has_api_key = env_var.is_empty()
            || resolve_api_key(
                &env_var,
                &self.credential_environment(),
                &vibe_core::auth::KeyringStore::native(),
            )
            .is_some();
        let view = snapshot.config_view();
        let active_model = view["activeModel"]["alias"]
            .as_str()
            .filter(|alias| !alias.is_empty())
            .unwrap_or(vibe_core::config::registry::DEFAULT_ACTIVE_MODEL_ALIAS);
        Ok(WorkspaceDispatch::result([
            ("provider", provider_view(&provider)),
            (
                "consoleBaseUrl",
                json!(effective_text(
                    &snapshot,
                    "console_base_url",
                    vibe_core::auth::DEFAULT_CONSOLE_BASE_URL
                )),
            ),
            (
                "vibeBaseUrl",
                json!(effective_text(
                    &snapshot,
                    "vibe_base_url",
                    vibe_core::auth::DEFAULT_VIBE_BASE_URL
                )),
            ),
            ("activeModel", json!(active_model)),
            ("theme", view["theme"].clone()),
            (
                "supportsBrowserSignIn",
                json!(supports_browser_sign_in(&provider)),
            ),
            ("hasApiKey", json!(has_api_key)),
            (
                "enableSystemTrustStore",
                view["enableSystemTrustStore"].clone(),
            ),
        ]))
    }

    /// Reference `SetupRequestHandler._store_credential`: the key is stored
    /// for the provider the client names, resolved against this process's own
    /// configuration, with the persistence outcomes the terminal reports.
    pub(super) fn setup_store_credential(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let params: StoreCredentialParams = parse(params)?;
        let snapshot = self.setup_snapshot()?;
        let provider = resolve_api_key_provider(&named_provider(&snapshot, &params.provider)?);
        let mut stored = self
            .stored_keys
            .lock()
            .map_err(|_| WorkspaceServiceError::StatePoisoned)?;
        let report = persist_api_key(
            &text(&provider, "api_key_env_var"),
            text(&provider, "backend") == "mistral",
            &params.api_key,
            params.custom_domain,
            &mut stored,
            &global_env_file(&self.paths.vibe_home),
            &vibe_core::auth::KeyringStore::native(),
        );
        let (outcome, detail) = match report.outcome {
            PersistOutcome::Completed => ("completed", None),
            PersistOutcome::EnvVarError { detail } => ("env_var_error", Some(detail)),
            PersistOutcome::SaveError { detail } => ("save_error", Some(detail)),
        };
        Ok(WorkspaceDispatch::result([
            ("outcome", json!(outcome)),
            ("detail", json!(detail)),
        ]))
    }

    /// Reference `SetupRequestHandler._submit_choices`: only what the wizard
    /// moved is written, the provider merged onto the configured one of the
    /// same name so the settings the wizard never shows survive, then the
    /// theme when one was chosen.
    pub(super) fn setup_submit_choices(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let params: SubmitChoicesParams = parse(params)?;
        let snapshot = self.setup_snapshot()?;
        let (provider, provider_drift) = merged_provider(&snapshot, params.provider.as_ref());
        let drifted = |requested: &Option<String>, key: &str, default: &str| {
            requested
                .as_ref()
                .filter(|value| **value != effective_text(&snapshot, key, default))
                .cloned()
        };
        let console_base_url = drifted(
            &params.console_base_url,
            "console_base_url",
            vibe_core::auth::DEFAULT_CONSOLE_BASE_URL,
        );
        let vibe_base_url = drifted(
            &params.vibe_base_url,
            "vibe_base_url",
            vibe_core::auth::DEFAULT_VIBE_BASE_URL,
        );
        let mut failures = Vec::new();
        if provider_drift || console_base_url.is_some() || vibe_base_url.is_some() {
            let result = persist_provider_credentials(
                &self.config,
                &ProviderCredentialsRequest {
                    provider,
                    console_base_url,
                    vibe_base_url,
                },
            );
            if !result.provider {
                failures.push("provider");
            }
            if result.console_base_url == Some(false) {
                failures.push("console_base_url");
            }
            if result.vibe_base_url == Some(false) {
                failures.push("vibe_base_url");
            }
        }
        if let Some(theme) = params.theme {
            let written = self
                .config
                .persist_field("theme", TomlValue::String(theme), ONBOARDING);
            if let Err(error) = written {
                vibe_core::observability::log(
                    vibe_core::observability::LogLevel::Error,
                    &format!("Failed to persist theme to config: {error}"),
                );
                failures.push("theme");
            }
        }
        let outcome = if failures.is_empty() {
            "completed"
        } else {
            "provider_config_error"
        };
        Ok(WorkspaceDispatch::result([
            ("outcome", json!(outcome)),
            ("failures", json!(failures)),
        ]))
    }
}

/// Reference `_seed_provider`: the active provider, or the first shipped one
/// when the active model's provider does not resolve.
fn seed_provider(snapshot: &ConfigSnapshot) -> Table {
    snapshot
        .active_provider()
        .or_else(|| {
            vibe_core::config::registry::shipped_providers()
                .into_iter()
                .next()
        })
        .unwrap_or_else(vibe_core::auth::default_mistral_provider)
}

/// Reference `_named_provider`: the configured providers first, then the
/// shipped ones, and an unknown name refused.
fn named_provider(snapshot: &ConfigSnapshot, name: &str) -> Result<Table, WorkspaceServiceError> {
    snapshot
        .entries("providers")
        .into_iter()
        .chain(vibe_core::config::registry::shipped_providers())
        .find(|provider| provider.get("name").and_then(TomlValue::as_str) == Some(name))
        .ok_or_else(|| {
            WorkspaceServiceError::Refused(
                ProtocolErrorCode::InvalidParams,
                format!("Unknown setup provider: {name}"),
            )
        })
}

/// Reference `_merged_provider`: the provider to persist and whether the
/// wizard's view moved it. The view's fields land on the configured provider
/// of the same name; an unknown name starts a fresh one, and no view passes
/// the seed provider through unchanged.
fn merged_provider(snapshot: &ConfigSnapshot, view: Option<&ProviderView>) -> (Table, bool) {
    let Some(view) = view else {
        return (seed_provider(snapshot), false);
    };
    let current = snapshot
        .entries("providers")
        .into_iter()
        .find(|provider| provider.get("name").and_then(TomlValue::as_str) == Some(&view.name));
    let fields: [(&str, Option<TomlValue>); 5] = [
        ("api_base", Some(TomlValue::String(view.api_base.clone()))),
        (
            "api_key_env_var",
            Some(TomlValue::String(view.api_key_env_var.clone())),
        ),
        (
            "browser_auth_base_url",
            view.browser_auth_base_url.clone().map(TomlValue::String),
        ),
        (
            "browser_auth_api_base_url",
            view.browser_auth_api_base_url
                .clone()
                .map(TomlValue::String),
        ),
        (
            "browser_auth_allow_origin_rewrite",
            Some(TomlValue::Boolean(view.browser_auth_allow_origin_rewrite)),
        ),
    ];
    let Some(current) = current else {
        let mut fresh = Table::new();
        fresh.insert("name".to_owned(), TomlValue::String(view.name.clone()));
        for (key, value) in fields {
            if let Some(value) = value {
                fresh.insert(key.to_owned(), value);
            }
        }
        return (fresh, true);
    };
    // The configured provider as its validated model reads each field, so a
    // default the entry leaves implicit compares equal to the view spelling it.
    let drift = fields.iter().any(|(key, value)| {
        let held = match *key {
            "browser_auth_base_url" | "browser_auth_api_base_url" => {
                effective_browser_auth_url(&current, key).map(TomlValue::String)
            }
            "api_key_env_var" => Some(
                current
                    .get(*key)
                    .cloned()
                    .unwrap_or_else(|| TomlValue::String(String::new())),
            ),
            "browser_auth_allow_origin_rewrite" => Some(
                current
                    .get(*key)
                    .cloned()
                    .unwrap_or(TomlValue::Boolean(false)),
            ),
            _ => current.get(*key).cloned(),
        };
        held != *value
    });
    let mut merged = current;
    for (key, value) in fields {
        match value {
            Some(value) => {
                merged.insert(key.to_owned(), value);
            }
            None => {
                merged.remove(key);
            }
        }
    }
    (merged, drift)
}

/// Reference `_provider_view`, with the browser-auth bases the mistral
/// provider defaults to when its entry names none.
fn provider_view(provider: &Table) -> Value {
    json!({
        "name": text(provider, "name"),
        "apiBase": text(provider, "api_base"),
        "apiKeyEnvVar": text(provider, "api_key_env_var"),
        "browserAuthBaseUrl": effective_browser_auth_url(provider, "browser_auth_base_url"),
        "browserAuthApiBaseUrl": effective_browser_auth_url(provider, "browser_auth_api_base_url"),
        "browserAuthAllowOriginRewrite": provider
            .get("browser_auth_allow_origin_rewrite")
            .and_then(TomlValue::as_bool)
            .unwrap_or(false),
    })
}

fn text(table: &Table, key: &str) -> String {
    table
        .get(key)
        .and_then(TomlValue::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn effective_text(snapshot: &ConfigSnapshot, key: &str, default: &str) -> String {
    snapshot
        .effective
        .get(key)
        .and_then(TomlValue::as_str)
        .unwrap_or(default)
        .to_owned()
}

#[cfg(test)]
mod setup_tests;
