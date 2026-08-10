//! What the onboarding flow starts from, and how it treats a typed console
//! domain.
//!
//! Reference `vibe/setup/onboarding/context.py`: the flow resolves the
//! provider it authenticates against from the effective configuration,
//! validates a typed domain by normalizing it into an HTTP origin, derives
//! the browser and API base URLs from that origin, and warns on a host shaped
//! like a Mistral private-cloud console without blocking it.

use serde_json::Value as JsonValue;
use toml::{Table, Value};
use vibe_core::auth::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, browser_sign_in_bases,
};

/// Reference `DEFAULT_VIBE_BASE_URL`: the base the API key screen derives its
/// console link from when the configuration carries no other.
pub const DEFAULT_VIBE_BASE_URL: &str = "https://chat.mistral.ai";

/// Reference `DEFAULT_THEME`, the catalog entry the picker starts on when the
/// configuration names no theme the catalog knows.
pub const DEFAULT_THEME: &str = "auto";

/// Reference `DEFAULT_ACTIVE_MODEL_CONFIG.alias`: the alias the provider
/// resolution falls back to when the configured active model names no entry.
const DEFAULT_ACTIVE_MODEL_ALIAS: &str = "mistral-medium-3.5";

/// Reference `_normalize_origin`: a scheme-less value is upgraded to
/// `https://`, an explicit scheme is respected, and trailing slashes drop.
fn normalize_origin(value: &str) -> String {
    let origin = value.trim();
    let origin = if origin.contains("://") {
        origin.to_owned()
    } else {
        format!("https://{origin}")
    };
    origin.trim_end_matches('/').to_owned()
}

/// Reference `is_valid_custom_domain`. `http://` is accepted on purpose so a
/// local auth gateway such as `http://localhost:8080` stays reachable; only a
/// value with a broken scheme separator (`:/` without `://`) or one that does
/// not normalize into an absolute HTTP origin with a host is refused.
#[must_use]
pub fn is_valid_custom_domain(value: &str) -> bool {
    let origin = value.trim();
    if !origin.contains("://") && origin.contains(":/") {
        return false;
    }
    let Ok(parsed) = url::Url::parse(&normalize_origin(origin)) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some_and(|host| !host.is_empty())
}

/// Reference `resolve_browser_auth_urls`: the browser base is the normalized
/// origin as typed, and the API base is that origin with `/api` appended.
#[must_use]
pub fn resolve_browser_auth_urls(domain: &str) -> (String, String) {
    let base = normalize_origin(domain);
    let api = format!("{base}/api");
    (base, api)
}

/// Reference `is_likely_mistral_private_cloud_domain`: a Mistral-hosted
/// console subdomain that is not the default auth host. Private-cloud Studio
/// hands users a custom `console.*.mistral.ai` URL while Mistral-hosted
/// accounts sign in through `console.mistral.ai`, so the wizard warns without
/// blocking.
#[must_use]
pub fn is_likely_mistral_private_cloud_domain(domain: &str) -> bool {
    let Ok(parsed) = url::Url::parse(&normalize_origin(domain)) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    host != "console.mistral.ai" && host.starts_with("console.") && host.ends_with(".mistral.ai")
}

/// The validation class pair the domain input carries, named as the reference
/// names its box classes; the feedback label is derived per class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainFeedback {
    /// Box `valid`, feedback `success`.
    Valid,
    /// Box `warning`, feedback `warning`: accepted, but private-cloud shaped.
    Warning,
    /// Box `invalid`, feedback `error`.
    Invalid,
}

impl DomainFeedback {
    /// The reference's input-box class for this state.
    #[must_use]
    pub const fn box_class(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Warning => "warning",
            Self::Invalid => "invalid",
        }
    }

    /// The reference's feedback-label class for this state.
    #[must_use]
    pub const fn feedback_class(self) -> &'static str {
        match self {
            Self::Valid => "success",
            Self::Warning => "warning",
            Self::Invalid => "error",
        }
    }
}

/// Reference `_render_domain_feedback`: invalid wins, then the private-cloud
/// warning, then the plain valid class.
#[must_use]
pub fn domain_feedback(value: &str) -> DomainFeedback {
    if !is_valid_custom_domain(value) {
        return DomainFeedback::Invalid;
    }
    if !value.trim().is_empty() && is_likely_mistral_private_cloud_domain(value) {
        return DomainFeedback::Warning;
    }
    DomainFeedback::Valid
}

/// The custom domain the configuration already carries: the provider's
/// browser base URL when it is present, non-empty, and not the shipped
/// default. Reference `OnboardingApp.configured_custom_domain`.
#[must_use]
pub fn configured_custom_domain(provider: &Table) -> Option<&str> {
    provider
        .get("browser_auth_base_url")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty() && *base != DEFAULT_BROWSER_AUTH_BASE_URL)
}

/// Whether the provider entry can browser sign-in at all, which is the
/// reference predicate that gates four of the seven screens.
#[must_use]
pub fn supports_browser_sign_in(provider: &Table) -> bool {
    browser_sign_in_bases(provider).is_some()
}

/// Reference `resolve_api_key_provider`: a provider whose key variable is
/// empty cannot store a key, so the shipped Mistral entry answers instead.
#[must_use]
pub fn resolve_api_key_provider(provider: &Table) -> Table {
    let has_env_key = provider
        .get("api_key_env_var")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    if has_env_key {
        provider.clone()
    } else {
        default_mistral_provider()
    }
}

/// The shipped Mistral provider entry, mirroring the first entry of the
/// registry's `DEFAULT_PROVIDERS` document.
#[must_use]
pub fn default_mistral_provider() -> Table {
    let mut table = Table::new();
    table.insert("name".to_owned(), Value::String("mistral".to_owned()));
    table.insert(
        "api_base".to_owned(),
        Value::String("https://api.mistral.ai/v1".to_owned()),
    );
    table.insert(
        "api_key_env_var".to_owned(),
        Value::String("MISTRAL_API_KEY".to_owned()),
    );
    table.insert(
        "browser_auth_base_url".to_owned(),
        Value::String(DEFAULT_BROWSER_AUTH_BASE_URL.to_owned()),
    );
    table.insert(
        "browser_auth_api_base_url".to_owned(),
        Value::String(DEFAULT_BROWSER_AUTH_API_BASE_URL.to_owned()),
    );
    table.insert("api_style".to_owned(), Value::String("openai".to_owned()));
    table.insert("backend".to_owned(), Value::String("mistral".to_owned()));
    table
}

/// What the flow starts from: the provider it authenticates, the help-link
/// base, and the configured theme. Reference `OnboardingContext`.
#[derive(Debug, Clone)]
pub struct OnboardingContext {
    pub provider: Table,
    pub vibe_base_url: String,
    pub theme: String,
}

impl OnboardingContext {
    /// Builds the context from the effective configuration document, falling
    /// back to the shipped defaults for anything missing or unreadable, as
    /// reference `OnboardingContext.load` falls back rather than failing.
    #[must_use]
    pub fn from_effective_config(config: Option<&JsonValue>) -> Self {
        let field = |name: &str| config.and_then(|config| config.get(name));
        let provider = resolve_onboarding_provider(
            field("active_model").and_then(JsonValue::as_str),
            field("models"),
            field("providers"),
        );
        Self {
            provider,
            vibe_base_url: field("vibe_base_url")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_VIBE_BASE_URL)
                .to_owned(),
            theme: field("theme")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_THEME)
                .to_owned(),
        }
    }

    #[must_use]
    pub fn supports_browser_sign_in(&self) -> bool {
        supports_browser_sign_in(&self.provider)
    }
}

/// Reference `_resolve_provider`: the active model's provider first, then the
/// default alias's, then any model's, then the only provider there is, and
/// the shipped Mistral entry when nothing resolves.
fn resolve_onboarding_provider(
    active_model: Option<&str>,
    models: Option<&JsonValue>,
    providers: Option<&JsonValue>,
) -> Table {
    let providers = json_entries(providers);
    let models = json_entries(models);
    let provider_named = |name: &str| {
        providers
            .iter()
            .find(|entry| entry.get("name").and_then(JsonValue::as_str) == Some(name))
    };
    let provider_of_model = |alias: &str| {
        models
            .iter()
            .filter(|model| model.get("alias").and_then(JsonValue::as_str) == Some(alias))
            .find_map(|model| provider_named(model.get("provider").and_then(JsonValue::as_str)?))
    };
    let resolved = active_model
        .and_then(provider_of_model)
        .or_else(|| provider_of_model(DEFAULT_ACTIVE_MODEL_ALIAS))
        .or_else(|| {
            models.iter().find_map(|model| {
                provider_named(model.get("provider").and_then(JsonValue::as_str)?)
            })
        })
        .or_else(|| (providers.len() == 1).then(|| &providers[0]));
    resolved
        .and_then(|entry| json_object_to_toml_table(entry))
        .unwrap_or_else(default_mistral_provider)
}

/// The object entries of a JSON array, dropping anything that is not one, as
/// the reference's payload validation drops entries that do not validate.
fn json_entries(value: Option<&JsonValue>) -> Vec<&JsonValue> {
    value
        .and_then(JsonValue::as_array)
        .map(|entries| entries.iter().filter(|entry| entry.is_object()).collect())
        .unwrap_or_default()
}

/// A JSON object as the TOML table the configuration writes deal in. `None`
/// when a value has no TOML counterpart, which no provider entry produces.
fn json_object_to_toml_table(value: &JsonValue) -> Option<Table> {
    match json_to_toml(value)? {
        Value::Table(table) => Some(table),
        _ => None,
    }
}

fn json_to_toml(value: &JsonValue) -> Option<Value> {
    Some(match value {
        JsonValue::Null => return None,
        JsonValue::Bool(value) => Value::Boolean(*value),
        JsonValue::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Value::Integer(integer)
            } else {
                Value::Float(value.as_f64()?)
            }
        }
        JsonValue::String(value) => Value::String(value.clone()),
        JsonValue::Array(values) => Value::Array(values.iter().filter_map(json_to_toml).collect()),
        JsonValue::Object(entries) => Value::Table(
            entries
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), json_to_toml(value)?)))
                .collect(),
        ),
    })
}
