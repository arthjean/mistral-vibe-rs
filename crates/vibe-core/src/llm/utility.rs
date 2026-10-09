//! The model a background nicety runs on: a session title, a worktree name.
//!
//! Reference `vibe/core/llm/utility_completion.py`. A small fast Mistral
//! model, asked for under two names, is preferred whenever a Mistral provider
//! is usable (its key resolves, or it needs none) and the deployment is known
//! to serve it: the public Mistral API is presumed to serve the first name
//! until a probe says otherwise, any other deployment has to be probed first
//! ([`super::availability`]). Otherwise the call runs on the session's own
//! model, even when the session runs on another provider.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Map, Value};

use super::availability::{ModelAvailability, PROBE_TIMEOUT};
use super::error::BackendFailure;
use super::retry::NoRetryObserver;
use super::types::Message;
use super::{Backend, BackendContext, Credentials, ModelRequest};
use crate::matching::NameFilter;
use crate::observability::{self, LogLevel};
pub use crate::provider::config::UtilityFeature;
use crate::provider::config::{
    ApiSettings, ModelConfig, ModelRouting, ProviderConfig, RoutingError,
};
use crate::pyurl::PyUrl;
use crate::telemetry::records::RequestSent;
use crate::telemetry::{
    ClientTelemetry, LaunchContext, TelemetryCallType, TelemetryContext, TelemetryRecord,
};

/// The name this client is billed under on the Mistral API.
pub const FAST_MODEL_NAME: &str = "mistral-vibe-cli-fast";
/// Its alias, which request telemetry reports.
pub const FAST_MODEL_ALIAS: &str = "mistral-small";
/// The public name self-hosted deployments serve the same model under, which
/// is also its alias.
pub const PUBLIC_FAST_MODEL_NAME: &str = "mistral-small-latest";
/// Turns probing off, for harnesses that run a real client against a
/// scripted model. Reference `_DISABLE_MODEL_PROBE_ENV_VAR`.
pub const DISABLE_PROBE_ENVIRONMENT: &str = "VIBE_TEST_DISABLE_MODEL_PROBE";

/// The scheme, host and port of the public Mistral API.
const PUBLIC_MISTRAL_API_ORIGIN: (&str, &str, u16) = ("https", "api.mistral.ai", 443);

/// The fast model under its billed name.
#[must_use]
pub fn fast_model() -> ModelConfig {
    let mut model = ModelConfig::new(FAST_MODEL_NAME, "mistral");
    model.alias = FAST_MODEL_ALIAS.to_owned();
    model
}

/// The fast model's two names, the metered one first. Reference
/// `FAST_MODEL_CANDIDATES`.
#[must_use]
pub fn fast_model_candidates() -> [ModelConfig; 2] {
    [
        fast_model(),
        ModelConfig::new(PUBLIC_FAST_MODEL_NAME, "mistral"),
    ]
}

/// What a utility completion runs on.
#[derive(Debug, Clone, PartialEq)]
pub struct UtilitySelection {
    pub model: ModelConfig,
    pub provider: ProviderConfig,
    /// Whether `[utility_models]` named the model for the feature.
    pub overridden: bool,
}

impl UtilitySelection {
    /// Reference `is_fast_utility_model`: false means the call fell back to
    /// the session's own model, which may be large and expensive, or runs on
    /// a model the configuration chose, which is never presumed cheap.
    #[must_use]
    pub fn is_fast(&self) -> bool {
        !self.overridden
            && [FAST_MODEL_NAME, PUBLIC_FAST_MODEL_NAME].contains(&self.model.name.as_str())
    }
}

/// Reference `select_utility_model`: an override from `[utility_models]`
/// first, then a fast model found served, then the model the session runs on.
///
/// # Errors
///
/// An override or the active model whose provider, or which itself, is
/// missing from the configuration.
pub fn select(
    routing: &ModelRouting,
    credentials: &dyn Credentials,
    availability: &ModelAvailability,
    feature: Option<UtilityFeature>,
) -> Result<UtilitySelection, RoutingError> {
    if let Some(feature) = feature
        && let Some(model) = routing.utility_model(feature)?
    {
        let provider = routing.provider_for(&model)?;
        return Ok(UtilitySelection {
            model,
            provider,
            overridden: true,
        });
    }
    if let Some(discovered) = discovered_fast_model(routing, credentials, availability) {
        return Ok(discovered);
    }
    let model = routing.model(None)?;
    let provider = routing.provider_for(&model)?;
    Ok(UtilitySelection {
        model,
        provider,
        overridden: false,
    })
}

/// The earliest candidate known to be served by a Mistral provider whose key
/// resolves or which needs none. Reference `_discovered_fast_model`.
fn discovered_fast_model(
    routing: &ModelRouting,
    credentials: &dyn Credentials,
    availability: &ModelAvailability,
) -> Option<UtilitySelection> {
    let provider = routing.mistral_provider()?;
    if !provider.api_key_env_var.is_empty()
        && credentials.resolve(&provider.api_key_env_var).is_none()
    {
        return None;
    }
    let assume_first = is_public_mistral_api(&provider.api_base);
    for (index, candidate) in fast_model_candidates().into_iter().enumerate() {
        if !fast_model_allowed(&routing.allowed_models, &candidate) {
            continue;
        }
        let known = availability.peek(&provider, &candidate, credentials);
        // An unprobed public API keeps serving the first name until a probe
        // answers.
        if known == Some(true) || (known.is_none() && index == 0 && assume_first) {
            return Some(UtilitySelection {
                model: candidate,
                provider,
                overridden: false,
            });
        }
    }
    None
}

/// Whether the allowlist admits a candidate, matched on its API name as
/// configured models are, since a user can give any model any alias.
/// Reference `_fast_model_allowed`.
fn fast_model_allowed(allowed: &[String], model: &ModelConfig) -> bool {
    allowed.is_empty() || NameFilter::new(allowed).matches(&model.name)
}

/// Reference `_is_public_mistral_api`: an omitted port counts as 443 only
/// under `https`, and a port that does not parse matches nothing.
fn is_public_mistral_api(api_base: &str) -> bool {
    let parsed = PyUrl::split(api_base);
    let Ok(port) = parsed.port() else {
        return false;
    };
    let port = match port {
        None if parsed.scheme == "https" => Some(443),
        port => port,
    };
    let (scheme, host, public_port) = PUBLIC_MISTRAL_API_ORIGIN;
    parsed.scheme == scheme
        && parsed.hostname().as_deref() == Some(host)
        && port == Some(public_port)
}

/// Settles, before the first selection, whether a fast model is served.
/// Reference `ensure_utility_models_probed`: it runs while a session opens, never fails, and never takes more than twice `budget`
/// (two seconds when `None`). Nothing is asked when every feature, or none
/// is given, names its own model in `[utility_models]`.
pub async fn ensure_probed(
    routing: &ModelRouting,
    features: &[UtilityFeature],
    budget: Option<Duration>,
    availability: &ModelAvailability,
    context: &BackendContext,
) {
    let disabled = std::env::var(DISABLE_PROBE_ENVIRONMENT).is_ok_and(|value| value == "1");
    probe_unless_disabled(disabled, routing, features, budget, availability, context).await;
}

/// [`ensure_probed`] with the harness switch already read.
pub(crate) async fn probe_unless_disabled(
    disabled: bool,
    routing: &ModelRouting,
    features: &[UtilityFeature],
    budget: Option<Duration>,
    availability: &ModelAvailability,
    context: &BackendContext,
) {
    if disabled {
        return;
    }
    let budget = budget.unwrap_or(PROBE_TIMEOUT);
    // Answers whether the check ran to its end, as opposed to failing.
    let probed = async {
        // Nothing to learn when every feature names its own model; an
        // override that cannot resolve fails the check, as it raises upstream.
        let mut wanted = false;
        for feature in features {
            match routing.utility_model(*feature) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    wanted = true;
                    break;
                }
                Err(_) => return false,
            }
        }
        if !wanted {
            return true;
        }
        let Some(provider) = routing.mistral_provider() else {
            return true;
        };
        let candidates: Vec<ModelConfig> = fast_model_candidates()
            .into_iter()
            .filter(|candidate| fast_model_allowed(&routing.allowed_models, candidate))
            .collect();
        if candidates.is_empty() {
            return true;
        }
        availability
            .ensure_first_available(&provider, &candidates, budget, context)
            .await;
        true
    };
    // A check that fails, or overruns twice its budget, is logged and dropped.
    if !matches!(tokio::time::timeout(budget * 2, probed).await, Ok(true)) {
        observability::log(
            LogLevel::Warning,
            "Could not learn whether a fast model is served; background features run on the \
             session model for now.",
        );
    }
}

/// The telemetry label a feature's completion carries, a function of the
/// feature alone. Reference `_call_type`.
#[must_use]
pub const fn feature_call_type(feature: UtilityFeature) -> TelemetryCallType {
    match feature {
        UtilityFeature::Title => TelemetryCallType::TitleGeneration,
        UtilityFeature::SmartApprove => TelemetryCallType::SmartApprove,
    }
}

/// Who a utility completion is made for, stamped as a session's requests
/// are, and the client that hears its `vibe.request_sent`.
#[derive(Clone, Copy, Default)]
pub struct Attribution<'a> {
    pub launch: Option<&'a LaunchContext>,
    pub session_id: Option<&'a str>,
    /// Reports one `vibe.request_sent` for the call when given.
    pub telemetry: Option<&'a dyn ClientTelemetry>,
}

/// Reference `build_request_metadata(launch_context, session_id, call_type)`
/// dumped without its absent fields: no parent, no plan, no experiments.
#[must_use]
pub fn request_metadata(
    launch: Option<&LaunchContext>,
    session_id: Option<&str>,
    call_type: TelemetryCallType,
) -> Map<String, Value> {
    TelemetryContext {
        launch: launch.cloned(),
        ..TelemetryContext::default()
    }
    .request_metadata(session_id, call_type, None)
    .properties()
}

/// One utility completion's shape.
#[derive(Clone)]
pub struct UtilityRequest<'a> {
    pub system_prompt: &'a str,
    pub user_content: &'a str,
    pub max_tokens: u64,
    pub request_timeout: Duration,
    pub retry_budget: Duration,
    /// Skip the call, rather than fail it, when the selected provider's key
    /// does not resolve.
    pub skip_if_no_key: bool,
    pub call_type: TelemetryCallType,
    pub attribution: Attribution<'a>,
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
    let attribution = request.attribution;
    if let Some(telemetry) = attribution.telemetry {
        let prompt_chars = request.user_content.chars().count() as u64;
        telemetry.record(
            &TelemetryRecord::RequestSent(RequestSent {
                model: selection.model.alias.clone(),
                nb_context_chars: request.system_prompt.chars().count() as u64 + prompt_chars,
                nb_context_messages: 2,
                nb_prompt_chars: prompt_chars,
                call_type: request.call_type,
                message_id: None,
                attachment_counts: BTreeMap::new(),
            }),
            attribution.session_id,
        );
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
    let metadata = request_metadata(
        attribution.launch,
        attribution.session_id,
        request.call_type,
    );
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
                metadata: Some(&metadata),
            },
            &NoRetryObserver,
        )
        .await?;
    Ok(chunk.message.content)
}
