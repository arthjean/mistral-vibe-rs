//! The provider entry the sign-in flows start from, and how a typed console
//! domain is treated.
//!
//! Reference `vibe/setup/onboarding/context.py`: the flows resolve the
//! provider they authenticate against from the effective configuration,
//! validate a typed domain by normalizing it into an HTTP origin, derive the
//! browser and API base URLs from that origin, and warn on a host shaped like
//! a Mistral private-cloud console without blocking it. Both the onboarding
//! screens in `vibe-cli` and the ACP authentication surface in `vibe-acp`
//! consume these answers, which is what places them in this layer.

use serde_json::Value as JsonValue;
use toml::{Table, Value};

use super::sign_in_http::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, browser_sign_in_bases,
    effective_browser_auth_url,
};
use crate::pyurl::{OriginKey, PyUrl, default_port, normalize_url_origin};

/// Reference `DEFAULT_CONSOLE_BASE_URL`: the public console account calls go
/// to, and the one console tenant discovery never asks.
pub const DEFAULT_CONSOLE_BASE_URL: &str = "https://console.mistral.ai";

/// Reference `DEFAULT_VIBE_BASE_URL`: the public chat base.
pub const DEFAULT_VIBE_BASE_URL: &str = "https://chat.mistral.ai";

/// The shipped Mistral provider's `api_base`, reference
/// `f"{DEFAULT_MISTRAL_SERVER_URL}/v1"`.
pub const DEFAULT_MISTRAL_API_BASE: &str = "https://api.mistral.ai/v1";

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
/// origin as typed, and the API base is the separately supplied one when a
/// split-horizon deployment names it, normalized the same way, or that origin
/// with `/api` appended otherwise. An empty API base reads as absent.
#[must_use]
pub fn resolve_browser_auth_urls(domain: &str, api_base_url: Option<&str>) -> (String, String) {
    let base = normalize_origin(domain);
    match api_base_url.filter(|api| !api.is_empty()) {
        Some(api) => (base, normalize_origin(api)),
        None => {
            let api = format!("{base}/api");
            (base, api)
        }
    }
}

/// Reference `_origin_key`: the origin a configured URL names once
/// normalized, a malformed port reading as the scheme's default rather than
/// failing, since the gateway rejects that URL later anyway. `None` for a
/// value whose bracketed host does not split, where the reference raises.
fn origin_key(value: &str) -> Option<OriginKey> {
    let parsed = PyUrl::try_split(&normalize_origin(value)).ok()?;
    Some(normalize_url_origin(&parsed).unwrap_or_else(|_| {
        let scheme = parsed.scheme.to_ascii_lowercase();
        let port = default_port(&scheme);
        (scheme, parsed.hostname(), port)
    }))
}

/// Reference `browser_auth_requires_origin_rewrite`: whether the browser and
/// API bases sit on different origins, which is what a split-horizon
/// deployment looks like and what turns `browser_auth_allow_origin_rewrite` on.
#[must_use]
pub fn browser_auth_requires_origin_rewrite(browser_base_url: &str, api_base_url: &str) -> bool {
    origin_key(browser_base_url) != origin_key(api_base_url)
}

/// Reference `browser_auth_account_base`: the base account calls (`/whoami`,
/// plan lookups) go to. On a split-horizon deployment the browser console is
/// not reachable from here but the API base is, so its origin answers; on a
/// single host the normalized browser base answers, path prefix included.
#[must_use]
pub fn browser_auth_account_base(browser_base_url: &str, api_base_url: Option<&str>) -> String {
    let browser_base = normalize_origin(browser_base_url);
    let Some(api_base_url) = api_base_url.filter(|api| !api.is_empty()) else {
        return browser_base;
    };
    if !browser_auth_requires_origin_rewrite(browser_base_url, api_base_url) {
        return browser_base;
    }
    let parsed = PyUrl::split(&normalize_origin(api_base_url));
    format!("{}://{}", parsed.scheme, parsed.netloc)
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

/// The split-horizon API base the configuration already carries: the
/// provider's browser-auth API base when both browser-auth URLs are set and
/// sit on different origins. A same-origin API base is the derived default,
/// so it answers `None`. Reference `OnboardingApp.configured_custom_api_base`.
#[must_use]
pub fn configured_custom_api_base(provider: &Table) -> Option<String> {
    let browser = effective_browser_auth_url(provider, "browser_auth_base_url")
        .filter(|value| !value.is_empty())?;
    let api = effective_browser_auth_url(provider, "browser_auth_api_base_url")
        .filter(|value| !value.is_empty())?;
    browser_auth_requires_origin_rewrite(&browser, &api).then_some(api)
}

/// The provider's `browser_auth_allow_origin_rewrite`, `false` when absent.
#[must_use]
pub fn allows_origin_rewrite(provider: &Table) -> bool {
    provider
        .get("browser_auth_allow_origin_rewrite")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Points `provider` at a browser console: both browser-auth URLs, and the
/// origin-rewrite flag a split between them requires. Reference
/// `OnboardingApp.apply_custom_domain` and the custom target of
/// `AcpAuthController._resolve_sign_in_provider`.
pub fn apply_browser_auth_urls(provider: &mut Table, base_url: &str, api_base_url: &str) {
    provider.insert(
        "browser_auth_base_url".to_owned(),
        Value::String(base_url.to_owned()),
    );
    provider.insert(
        "browser_auth_api_base_url".to_owned(),
        Value::String(api_base_url.to_owned()),
    );
    provider.insert(
        "browser_auth_allow_origin_rewrite".to_owned(),
        Value::Boolean(browser_auth_requires_origin_rewrite(base_url, api_base_url)),
    );
}

/// Whether two provider entries are the same provider model: equal field by
/// field once every omitted field reads as its default, which is how the
/// reference's validated models compare. An entry that spells a default out
/// and one that leaves it implicit are the same provider.
#[must_use]
pub fn same_provider(left: &Table, right: &Table) -> bool {
    provider_values(left) == provider_values(right)
}

fn provider_values(provider: &Table) -> Table {
    let mut values = provider.clone();
    let defaults = [
        ("api_key_env_var", Value::String(String::new())),
        ("browser_auth_allow_origin_rewrite", Value::Boolean(false)),
        ("api_style", Value::String("openai".to_owned())),
        ("backend", Value::String("generic".to_owned())),
        (
            "reasoning_field_name",
            Value::String("reasoning_content".to_owned()),
        ),
        ("emits_finish_reason", Value::Boolean(true)),
        ("project_id", Value::String(String::new())),
        ("region", Value::String(String::new())),
        ("extra_headers", Value::Table(Table::new())),
    ];
    for (key, default) in defaults {
        values.entry(key).or_insert(default);
    }
    for key in ["browser_auth_base_url", "browser_auth_api_base_url"] {
        if let Some(url) = effective_browser_auth_url(provider, key) {
            values.insert(key.to_owned(), Value::String(url));
        }
    }
    values
}

/// Whether the provider entry can browser sign-in at all, which is the
/// reference predicate that gates the onboarding screens and the ACP
/// authentication methods alike.
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
        Value::String(DEFAULT_MISTRAL_API_BASE.to_owned()),
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

/// Reference `_resolve_provider`: the active model's provider first, then the
/// default alias's, then any model's, then the only provider there is, and
/// the shipped Mistral entry when nothing resolves.
#[must_use]
pub fn resolve_active_provider(
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

/// The object entries of a collection, dropping anything that is not one, as
/// the reference's payload validation drops entries that do not validate.
/// Providers travel as an array while the effective document keys models by
/// alias, so both container shapes read as their entries.
fn json_entries(value: Option<&JsonValue>) -> Vec<&JsonValue> {
    match value {
        Some(JsonValue::Array(entries)) => {
            entries.iter().filter(|entry| entry.is_object()).collect()
        }
        Some(JsonValue::Object(entries)) => {
            entries.values().filter(|entry| entry.is_object()).collect()
        }
        _ => Vec::new(),
    }
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
