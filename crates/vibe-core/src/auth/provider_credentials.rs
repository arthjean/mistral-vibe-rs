//! The configuration a sign-in leaves behind besides the key: the provider
//! entry, and the top-level console and chat bases when they moved.
//!
//! Reference `ProviderCredentialsPersistRequest`, `ProviderCredentialsPersistResult`
//! and `persist_provider_credentials` in `vibe/setup/auth/api_key_persistence.py`.
//! The three writes run one after the other against one configuration, and a
//! failed one never stops the next, so the result says field by field what
//! landed.

use toml::{Table, Value};

use crate::config::LayeredConfig;

/// The change-event reason the reference's sign-in writes carry.
const REASON: &str = "onboarding";

/// One batch of sign-in configuration writes. A base URL is `None` when it
/// did not move, which leaves it untouched rather than writing it back.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCredentialsRequest {
    pub provider: Table,
    pub console_base_url: Option<String>,
    pub vibe_base_url: Option<String>,
}

/// What each requested write did: `None` when it was not requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCredentialsResult {
    pub provider: bool,
    pub console_base_url: Option<bool>,
    pub vibe_base_url: Option<bool>,
}

impl ProviderCredentialsResult {
    /// The first field that failed, in write order. Reference `first_failure`.
    #[must_use]
    pub const fn first_failure(&self) -> Option<&'static str> {
        if !self.provider {
            return Some("provider");
        }
        if matches!(self.console_base_url, Some(false)) {
            return Some("console_base_url");
        }
        if matches!(self.vibe_base_url, Some(false)) {
            return Some("vibe_base_url");
        }
        None
    }
}

/// Writes the provider entry, then each base URL the request carries.
#[must_use]
pub fn persist_provider_credentials(
    config: &LayeredConfig,
    request: &ProviderCredentialsRequest,
) -> ProviderCredentialsResult {
    let provider = config.persist_provider(&request.provider, REASON).is_ok();
    let field = |key: &str, value: &Option<String>| {
        value.as_ref().map(|value| {
            config
                .persist_field(key, Value::String(value.clone()), REASON)
                .is_ok()
        })
    };
    ProviderCredentialsResult {
        provider,
        console_base_url: field("console_base_url", &request.console_base_url),
        vibe_base_url: field("vibe_base_url", &request.vibe_base_url),
    }
}
