//! The context's edges: the corpus replay covers the full domain-validation
//! matrix, so these tests hold the behaviors that name a rule rather than a
//! scenario, chiefly the provider resolution order and the key-provider
//! fallback.

use serde_json::json;
use toml::Value;

use super::context::{
    DomainFeedback, OnboardingContext, configured_custom_domain, default_mistral_provider,
    domain_feedback, is_valid_custom_domain, resolve_api_key_provider, resolve_browser_auth_urls,
};

#[test]
fn a_scheme_less_domain_upgrades_to_https_and_an_explicit_scheme_is_respected() {
    assert_eq!(
        resolve_browser_auth_urls("console.internal.example"),
        (
            "https://console.internal.example".to_owned(),
            "https://console.internal.example/api".to_owned()
        )
    );
    assert_eq!(
        resolve_browser_auth_urls("http://localhost:8080"),
        (
            "http://localhost:8080".to_owned(),
            "http://localhost:8080/api".to_owned()
        )
    );
    assert!(is_valid_custom_domain("http://localhost:8080"));
    assert!(!is_valid_custom_domain("ftp://example.com"));
    assert!(!is_valid_custom_domain("http:/broken"));
    assert!(!is_valid_custom_domain("   "));
}

#[test]
fn the_private_cloud_heuristic_warns_without_blocking() {
    assert_eq!(
        domain_feedback("console.oracle.mistral.ai"),
        DomainFeedback::Warning
    );
    assert_eq!(domain_feedback("console.mistral.ai"), DomainFeedback::Valid);
    assert_eq!(domain_feedback("foo.mistral.ai"), DomainFeedback::Valid);
    assert_eq!(domain_feedback("not a domain"), DomainFeedback::Invalid);
}

#[test]
fn the_provider_resolution_follows_the_active_model_then_the_default_alias() {
    let config = json!({
        "active_model": "custom-alias",
        "models": [
            {"alias": "custom-alias", "provider": "internal"},
            {"alias": "mistral-medium-3.5", "provider": "mistral"},
        ],
        "providers": [
            {"name": "mistral", "backend": "mistral", "api_key_env_var": "MISTRAL_API_KEY"},
            {"name": "internal", "backend": "generic", "api_key_env_var": "INTERNAL_KEY"},
        ],
    });
    let context = OnboardingContext::from_effective_config(Some(&config));
    assert_eq!(
        context.provider.get("name").and_then(Value::as_str),
        Some("internal")
    );

    let config = json!({
        "active_model": "unknown-alias",
        "models": [{"alias": "mistral-medium-3.5", "provider": "mistral"}],
        "providers": [
            {"name": "mistral", "backend": "mistral", "api_key_env_var": "MISTRAL_API_KEY"},
            {"name": "other", "backend": "generic"},
        ],
    });
    let context = OnboardingContext::from_effective_config(Some(&config));
    assert_eq!(
        context.provider.get("name").and_then(Value::as_str),
        Some("mistral")
    );
}

#[test]
fn an_unreadable_configuration_falls_back_to_the_shipped_mistral_entry() {
    let context = OnboardingContext::from_effective_config(None);
    assert_eq!(
        context.provider.get("name").and_then(Value::as_str),
        Some("mistral")
    );
    assert!(context.supports_browser_sign_in());
    assert_eq!(context.vibe_base_url, "https://chat.mistral.ai");
    assert_eq!(context.theme, "auto");
}

#[test]
fn a_provider_with_an_empty_key_variable_resolves_to_the_shipped_default() {
    let mut provider = default_mistral_provider();
    provider.insert("api_key_env_var".to_owned(), Value::String(String::new()));
    provider.insert("name".to_owned(), Value::String("keyless".to_owned()));
    let resolved = resolve_api_key_provider(&provider);
    assert_eq!(
        resolved.get("name").and_then(Value::as_str),
        Some("mistral")
    );
    assert_eq!(
        resolved.get("api_key_env_var").and_then(Value::as_str),
        Some("MISTRAL_API_KEY")
    );
}

#[test]
fn the_configured_custom_domain_ignores_the_shipped_default() {
    let provider = default_mistral_provider();
    assert_eq!(configured_custom_domain(&provider), None);
    let mut custom = provider;
    custom.insert(
        "browser_auth_base_url".to_owned(),
        Value::String("https://console.internal.example".to_owned()),
    );
    assert_eq!(
        configured_custom_domain(&custom),
        Some("https://console.internal.example")
    );
}
