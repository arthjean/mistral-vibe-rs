//! What the onboarding flow starts from, and how it treats a typed console
//! domain.
//!
//! Reference `vibe/setup/onboarding/context.py`. The provider resolution and
//! the domain predicates live in [`vibe_core::auth::provider`] because the
//! ACP authentication surface consumes them too; this module keeps what only
//! the screens read: the help-link base, the configured theme, and the
//! validation classes the domain and API base inputs render.

use serde_json::Value as JsonValue;
use toml::Table;
pub use vibe_core::auth::{
    DEFAULT_CONSOLE_BASE_URL, DEFAULT_VIBE_BASE_URL, configured_custom_api_base,
    configured_custom_domain, default_mistral_provider, is_likely_mistral_private_cloud_domain,
    is_valid_custom_domain, resolve_api_key_provider, resolve_browser_auth_urls,
    supports_browser_sign_in,
};

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

/// The validation class pair the optional API base input carries. A blank
/// value carries none: the field is optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiBaseFeedback {
    /// Box `valid`, feedback `success`.
    Valid,
    /// Box `invalid`, feedback `error`.
    Invalid,
}

impl ApiBaseFeedback {
    #[must_use]
    pub const fn box_class(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
        }
    }

    #[must_use]
    pub const fn feedback_class(self) -> &'static str {
        match self {
            Self::Valid => "success",
            Self::Invalid => "error",
        }
    }
}

/// Reference `_is_valid_optional_custom_domain`: the API base may be left
/// blank, and anything else has to be a valid domain.
#[must_use]
pub fn is_valid_optional_custom_domain(value: &str) -> bool {
    let stripped = value.trim();
    stripped.is_empty() || is_valid_custom_domain(stripped)
}

/// Reference `_render_api_base_feedback`: a blank value shows nothing, an
/// invalid one the failure, anything else the plain valid class.
#[must_use]
pub fn api_base_feedback(value: &str) -> Option<ApiBaseFeedback> {
    if value.trim().is_empty() {
        return None;
    }
    Some(if is_valid_optional_custom_domain(value) {
        ApiBaseFeedback::Valid
    } else {
        ApiBaseFeedback::Invalid
    })
}

/// The one feedback line the custom-domain screen shares between its two
/// inputs, and which of them last spoke on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackLine {
    Domain(DomainFeedback),
    ApiBase(ApiBaseFeedback),
}

impl FeedbackLine {
    #[must_use]
    pub const fn feedback_class(self) -> &'static str {
        match self {
            Self::Domain(feedback) => feedback.feedback_class(),
            Self::ApiBase(feedback) => feedback.feedback_class(),
        }
    }
}

/// What the flow starts from: the provider it authenticates, the chat and
/// console bases, and the configured theme. Reference `OnboardingContext`.
#[derive(Debug, Clone)]
pub struct OnboardingContext {
    pub provider: Table,
    pub vibe_base_url: String,
    pub console_base_url: String,
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
            console_base_url: field("console_base_url")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_CONSOLE_BASE_URL)
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
