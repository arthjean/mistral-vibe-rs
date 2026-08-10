//! What the onboarding flow starts from, and how it treats a typed console
//! domain.
//!
//! Reference `vibe/setup/onboarding/context.py`. The provider resolution and
//! the domain predicates live in [`vibe_core::auth::provider`] because the
//! ACP authentication surface consumes them too; this module keeps what only
//! the screens read: the help-link base, the configured theme, and the
//! validation classes the domain input renders.

use serde_json::Value as JsonValue;
use toml::Table;
pub use vibe_core::auth::{
    configured_custom_domain, default_mistral_provider, is_likely_mistral_private_cloud_domain,
    is_valid_custom_domain, resolve_api_key_provider, resolve_browser_auth_urls,
    supports_browser_sign_in,
};

/// Reference `DEFAULT_VIBE_BASE_URL`: the base the API key screen derives its
/// console link from when the configuration carries no other.
pub const DEFAULT_VIBE_BASE_URL: &str = "https://chat.mistral.ai";

/// Reference `DEFAULT_THEME`, the catalog entry the picker starts on when the
/// configuration names no theme the catalog knows.
pub const DEFAULT_THEME: &str = "auto";

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
        let provider = vibe_core::auth::resolve_active_provider(
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
