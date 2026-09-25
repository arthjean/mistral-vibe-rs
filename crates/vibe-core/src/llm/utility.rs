//! The model a background nicety runs on: a session title, a worktree name.
//!
//! Reference `vibe/core/llm/utility_completion.py`. The small fast Mistral
//! model is preferred whenever a Mistral provider is usable (the allowlist
//! admits the model and its key resolves, or it needs none), even when the
//! session runs on another provider; otherwise the call runs on the session's
//! own model.

use std::time::Duration;

use serde_json::{Map, Value};

use super::error::BackendFailure;
use super::retry::NoRetryObserver;
use super::types::Message;
use super::{Backend, BackendContext, Credentials, ModelRequest};
use crate::provider::config::{
    ApiSettings, ModelConfig, ModelRouting, ProviderConfig, RoutingError,
};

/// The fast model's API name.
pub const FAST_MODEL_NAME: &str = "mistral-vibe-cli-fast";
/// The alias the allowlist is matched against.
pub const FAST_MODEL_ALIAS: &str = "mistral-small";

/// The fast model as the reference declares it.
#[must_use]
pub fn fast_model() -> ModelConfig {
    let mut model = ModelConfig::new(FAST_MODEL_NAME, "mistral");
    model.alias = FAST_MODEL_ALIAS.to_owned();
    model
}

/// What a utility completion runs on.
#[derive(Debug, Clone, PartialEq)]
pub struct UtilitySelection {
    pub model: ModelConfig,
    pub provider: ProviderConfig,
}

impl UtilitySelection {
    /// Reference `is_fast_utility_model`: false means the call fell back to
    /// the session's own model, which may be large and expensive.
    #[must_use]
    pub fn is_fast(&self) -> bool {
        self.model.name == FAST_MODEL_NAME
    }
}

/// Reference `select_utility_model`. The session's model is resolved first,
/// so a configuration without one fails even when the fast model is usable.
///
/// # Errors
///
/// An active model or its provider missing from the configuration.
pub fn select(
    routing: &ModelRouting,
    credentials: &dyn Credentials,
) -> Result<UtilitySelection, RoutingError> {
    let active = routing.model(None)?;
    let active_provider = routing.provider_for(&active)?;
    Ok(match fast_provider(routing, credentials) {
        Some(provider) => UtilitySelection {
            model: fast_model(),
            provider,
        },
        None => UtilitySelection {
            model: active,
            provider: active_provider,
        },
    })
}

fn fast_provider(routing: &ModelRouting, credentials: &dyn Credentials) -> Option<ProviderConfig> {
    if !routing.allows(FAST_MODEL_ALIAS) {
        return None;
    }
    let provider = routing.mistral_provider()?;
    if !provider.api_key_env_var.is_empty()
        && credentials.resolve(&provider.api_key_env_var).is_none()
    {
        return None;
    }
    Some(provider)
}

/// One utility completion's shape.
#[derive(Debug, Clone)]
pub struct UtilityRequest<'a> {
    pub system_prompt: &'a str,
    pub user_content: &'a str,
    pub max_tokens: u64,
    pub request_timeout: Duration,
    pub retry_budget: Duration,
    /// Skip the call, rather than fail it, when the selected provider's key
    /// does not resolve.
    pub skip_if_no_key: bool,
    /// The `secondary_call` request metadata.
    pub metadata: &'a Map<String, Value>,
}

/// Reference `run_utility_completion`: one non-streaming call at temperature
/// zero, answering with the raw message content.
///
/// # Errors
///
/// The backend failing the call.
pub async fn complete(
    selection: &UtilitySelection,
    context: &BackendContext,
    request: &UtilityRequest<'_>,
) -> Result<Option<String>, BackendFailure> {
    let provider = &selection.provider;
    if request.skip_if_no_key
        && !provider.api_key_env_var.is_empty()
        && context
            .credentials
            .resolve(&provider.api_key_env_var)
            .is_none()
    {
        return Ok(None);
    }
    let context = BackendContext {
        api: ApiSettings {
            timeout: request.request_timeout,
            retry_max_elapsed_time: request.retry_budget,
            ..ApiSettings::default()
        },
        ..context.clone()
    };
    let backend = Backend::new(provider.clone(), context)?;
    let messages = [
        Message::system(request.system_prompt),
        Message::user(request.user_content),
    ];
    let headers = [(
        "user-agent".to_owned(),
        super::call::user_agent(provider.backend),
    )];
    let chunk = backend
        .complete(
            &ModelRequest {
                model: &selection.model,
                messages: &messages,
                temperature: 0.0,
                tools: None,
                max_tokens: Some(request.max_tokens),
                tool_choice: None,
                extra_headers: &headers,
                metadata: Some(request.metadata),
            },
            &NoRetryObserver,
        )
        .await?;
    Ok(chunk.message.content)
}
