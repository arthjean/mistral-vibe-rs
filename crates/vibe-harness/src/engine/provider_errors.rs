//! What a failed model call reports: the code, message and details the
//! reference's client failures carry.

use serde_json::{Map, Value, json};
use vibe_core::provider::ProviderError;

use super::TurnEnvironment;
use crate::state::PublicError;

/// Reference `execute_completion`'s failure events: what a failed model call
/// reports, which `harness_turn_error_code` later maps for clients.
pub(super) fn turn_error(error: &ProviderError, environment: &TurnEnvironment) -> PublicError {
    let cause = backend_cause(error);
    let local = local_cause(error);
    let status = status_of(error);
    let mut details = Map::new();
    details.insert(
        "provider".to_owned(),
        json!(cause.map_or_else(
            || {
                environment
                    .model
                    .as_ref()
                    .map_or("mistral", |model| model.provider.as_str())
                    .to_owned()
            },
            |cause| cause.provider.clone(),
        )),
    );
    details.insert(
        "model".to_owned(),
        json!(cause.map_or_else(
            || {
                environment
                    .model
                    .as_ref()
                    .map(|model| model.name.clone())
                    .unwrap_or_default()
            },
            |cause| cause.model.clone(),
        )),
    );
    details.insert("purpose".to_owned(), json!("agent"));
    details.insert(
        "exceptionType".to_owned(),
        json!(exception_type(cause, local)),
    );
    if let Some(status) = status {
        details.insert("httpStatus".to_owned(), json!(status));
    }
    if let Some(correlation) = cause.and_then(|cause| header(cause, "mistral-correlation-id")) {
        details.insert("correlationId".to_owned(), json!(correlation));
    }
    let rejection = match status {
        Some(401) => Some("http_unauthorized"),
        Some(403) => Some("http_forbidden"),
        _ => None,
    };
    if let Some(reason) = rejection {
        // Only a 401 says the credential itself was refused, so only then is
        // where it came from worth naming.
        let origin = cause
            .filter(|_| reason == "http_unauthorized")
            .and_then(|cause| cause.api_key_origin.as_ref());
        details.insert("reason".to_owned(), json!(reason));
        return PublicError {
            code: "model_unauthorized".to_owned(),
            message: invalid_api_key_message(origin),
            details: Some(Value::Object(details)),
        };
    }
    PublicError {
        code: "model_stream_failed".to_owned(),
        message: failure_message(error, cause),
        details: Some(Value::Object(details)),
    }
}

/// Reference `invalid_api_key_message`.
fn invalid_api_key_message(origin: Option<&vibe_core::llm::error::KeyOrigin>) -> String {
    use vibe_core::llm::error::KeySource;
    match origin {
        Some(origin) => {
            let source = match origin.source {
                KeySource::Environment => format!("env var {}", origin.variable),
                KeySource::Keyring => "the keyring".to_owned(),
            };
            format!("Invalid API key (from {source}). Please check your API key and try again.")
        }
        None => "Invalid API key. Please check your API key and try again.".to_owned(),
    }
}

/// The class name the reference's client raises the failure as.
fn exception_type(
    cause: Option<&vibe_core::llm::error::BackendError>,
    local: Option<&vibe_core::llm::error::LocalFailure>,
) -> String {
    use vibe_core::llm::error::BackendErrorSource;
    if let Some(cause) = cause {
        return match &cause.source {
            BackendErrorSource::Client if cause.status == Some(422) => {
                "HTTPValidationError".to_owned()
            }
            BackendErrorSource::Client => "SDKError".to_owned(),
            BackendErrorSource::Status => "HTTPStatusError".to_owned(),
            BackendErrorSource::Stream => "OpenAIResponsesStreamError".to_owned(),
            BackendErrorSource::Request(kind) => kind.name().to_owned(),
        };
    }
    local
        .map_or("Exception", |local| local.kind.reference_class())
        .to_owned()
}

/// Reference `_failure_message`: the client's own rendering, with the
/// provider's explanation appended when it does not already carry it.
fn failure_message(
    error: &ProviderError,
    cause: Option<&vibe_core::llm::error::BackendError>,
) -> String {
    use vibe_core::llm::error::BackendErrorSource;
    let Some(cause) = cause else {
        return local_cause(error).map_or_else(|| error.to_string(), |local| local.message.clone());
    };
    let rendered = match (&cause.source, cause.status) {
        (BackendErrorSource::Client, Some(status)) => sdk_error_message(cause, status),
        (BackendErrorSource::Status, Some(status)) => {
            let kind = match status {
                100..=199 => "Informational response",
                300..=399 => "Redirect response",
                400..=499 => "Client error",
                _ => "Server error",
            };
            format!(
                "{kind} '{status} {}' for url '{}'\nFor more information check: \
                 https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{status}",
                cause.reason.as_deref().unwrap_or_default(),
                cause.endpoint,
            )
        }
        _ => cause.reason.clone().unwrap_or_else(|| error.to_string()),
    };
    match provider_message(&cause.body_text) {
        Some(detail) if !rendered.contains(&detail) => format!("{rendered}: {detail}"),
        _ => rendered,
    }
}

/// How the Mistral client's `SDKError` renders a refused answer.
fn sdk_error_message(cause: &vibe_core::llm::error::BackendError, status: u16) -> String {
    const MAX_MESSAGE_LEN: usize = 10_000;
    let mut message = format!("API error occurred: Status {status}");
    let content_type = header(cause, "content-type").unwrap_or("\"\"");
    if content_type != "application/json" {
        if content_type.contains(' ') {
            message.push_str(&format!(" Content-Type \"{content_type}\""));
        } else {
            message.push_str(&format!(" Content-Type {content_type}"));
        }
    }
    let body = if cause.body_text.is_empty() {
        "\"\""
    } else {
        cause.body_text.as_str()
    };
    let length = body.chars().count();
    if length > MAX_MESSAGE_LEN {
        let truncated: String = body.chars().take(MAX_MESSAGE_LEN).collect();
        message.push_str(&format!(
            ". Body: {truncated}...and {} more chars",
            length - MAX_MESSAGE_LEN
        ));
    } else {
        message.push_str(&format!(". Body: {body}"));
    }
    message.trim().to_owned()
}

/// Reference `_provider_message`: the explanation a provider put in an error
/// body, the OpenAI-compatible `error.message` first.
fn provider_message(body: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(body).ok()?;
    let payload = payload.as_object()?;
    let error = payload.get("error");
    [
        error.and_then(|error| error.get("message")),
        error.filter(|error| error.is_string()),
        payload.get("message"),
        payload.get("detail"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::trim)
    .find(|candidate| !candidate.is_empty())
    .map(ToOwned::to_owned)
}

fn header<'a>(cause: &'a vibe_core::llm::error::BackendError, name: &str) -> Option<&'a str> {
    cause
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The backend failure a failed call carries, when it reached the provider.
fn backend_cause(error: &ProviderError) -> Option<&vibe_core::llm::error::BackendError> {
    use vibe_core::llm::error::{CallFailure, WrappedCause};
    let ProviderError::Call(call) = error else {
        return None;
    };
    match &call.failure {
        CallFailure::RateLimit { cause, .. }
        | CallFailure::ContextTooLong { cause, .. }
        | CallFailure::ResponseTooLong { cause, .. } => Some(cause),
        CallFailure::InvalidModel(cause) => Some(cause),
        CallFailure::Wrapped {
            cause: WrappedCause::Backend(cause),
            ..
        } => Some(cause),
        _ => None,
    }
}

fn local_cause(error: &ProviderError) -> Option<&vibe_core::llm::error::LocalFailure> {
    use vibe_core::llm::error::{CallFailure, WrappedCause};
    match error {
        ProviderError::Call(call) => match &call.failure {
            CallFailure::Wrapped {
                cause: WrappedCause::Local(local),
                ..
            } => Some(local),
            _ => None,
        },
        _ => None,
    }
}

/// The HTTP status a failed call carried, when it got that far.
fn status_of(error: &ProviderError) -> Option<u16> {
    match error {
        ProviderError::HttpStatus { status } => Some(*status),
        ProviderError::Call(call)
            if matches!(
                call.failure,
                vibe_core::llm::error::CallFailure::RateLimit { .. }
            ) =>
        {
            backend_cause(error)
                .and_then(|cause| cause.status)
                .or(Some(429))
        }
        _ => backend_cause(error).and_then(|cause| cause.status),
    }
}
