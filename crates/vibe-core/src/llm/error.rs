//! What a failed model call is, before and after the loop classifies it.
//!
//! A backend fails with a [`BackendFailure`]: either a [`BackendError`], the
//! provider or the network refusing the call (reference `BackendError`,
//! `vibe/core/llm/exceptions.py`), or a local failure reading what came back.
//! The loop then decides what the turn fails with, a [`CallFailure`], which is
//! what `_chat_streaming` and `_complete` raise in `vibe/core/agent_loop/_loop.py`
//! and what `public_error` (`vibe/app_server/_utils.py`) turns into a code and
//! details.
//!
//! The predicates are the reference's, substring for substring, because a
//! client branches on the code they select. The prose is this port's own.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

use super::types::ToolChoice;

/// Where a credential was read from, which is what a user acts on when the
/// provider refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOrigin {
    pub source: KeySource,
    pub variable: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Environment,
    Keyring,
}

impl KeySource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Keyring => "keyring",
        }
    }
}

impl fmt::Display for KeyOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            KeySource::Environment => {
                write!(formatter, "the {} environment variable", self.variable)
            }
            KeySource::Keyring => write!(formatter, "the system keyring"),
        }
    }
}

/// What the call was, summarized for a failure report. Reference
/// `PayloadSummary`.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadSummary {
    pub model: String,
    pub message_count: usize,
    pub approx_chars: usize,
    pub temperature: f64,
    pub has_tools: bool,
    pub tool_choice: Option<ToolChoice>,
}

/// What produced a [`BackendError`], which decides whether it is retried and
/// how a retry is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendErrorSource {
    /// The provider answered with an error status.
    Status,
    /// The Mistral client refused the answer, whatever its status.
    Client,
    /// An OpenAI Responses stream reported a failure in an event.
    Stream,
    /// The request never got an answer; the value names the transport failure
    /// as the reference's HTTP client classes it (`ConnectError`, `ReadTimeout`,
    /// `RemoteProtocolError`, ...).
    Request(TransportKind),
}

/// A transport failure, named as `httpx` names its exception classes, which is
/// the detail a retry notice publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    ConnectError,
    ConnectTimeout,
    ReadTimeout,
    WriteTimeout,
    PoolTimeout,
    ReadError,
    WriteError,
    RemoteProtocolError,
    LocalProtocolError,
    UnsupportedProtocol,
    TooManyRedirects,
}

impl TransportKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ConnectError => "ConnectError",
            Self::ConnectTimeout => "ConnectTimeout",
            Self::ReadTimeout => "ReadTimeout",
            Self::WriteTimeout => "WriteTimeout",
            Self::PoolTimeout => "PoolTimeout",
            Self::ReadError => "ReadError",
            Self::WriteError => "WriteError",
            Self::RemoteProtocolError => "RemoteProtocolError",
            Self::LocalProtocolError => "LocalProtocolError",
            Self::UnsupportedProtocol => "UnsupportedProtocol",
            Self::TooManyRedirects => "TooManyRedirects",
        }
    }

    /// `httpx.TimeoutException`.
    #[must_use]
    pub const fn is_timeout(self) -> bool {
        matches!(
            self,
            Self::ConnectTimeout | Self::ReadTimeout | Self::WriteTimeout | Self::PoolTimeout
        )
    }

    /// `httpx.NetworkError`: the failures the Mistral client retries besides
    /// timeouts.
    #[must_use]
    pub const fn is_network(self) -> bool {
        matches!(
            self,
            Self::ConnectError | Self::ReadError | Self::WriteError
        )
    }

    /// The failures the generic backend retries: every timeout, a refused or
    /// broken connection, and a peer that closed without a whole answer.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        self.is_timeout()
            || matches!(
                self,
                Self::ConnectError | Self::ReadError | Self::WriteError | Self::RemoteProtocolError
            )
    }
}

/// A transport failure with the text the client library gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportFailure {
    pub kind: TransportKind,
    pub message: String,
}

impl fmt::Display for TransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            formatter.write_str(self.kind.name())
        } else {
            formatter.write_str(&self.message)
        }
    }
}

/// The provider or the network refused a call.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendError {
    pub provider: String,
    pub endpoint: String,
    pub status: Option<u16>,
    pub reason: Option<String>,
    /// Response headers, lowercased.
    pub headers: BTreeMap<String, String>,
    pub body_text: String,
    pub parsed_error: Option<String>,
    pub model: String,
    pub payload_summary: PayloadSummary,
    pub api_key_origin: Option<KeyOrigin>,
    pub source: BackendErrorSource,
}

const CONTEXT_TOO_LONG: &[&str] = &[
    "context too long",
    "maximum context length",
    "input too large",
    "couldn't fit with truncation",
    "prompt is too long",
    "model_context_exceeded",
    "prompt_too_long",
];

const RESPONSE_TOO_LONG: &[&str] = &["max_tokens_exceeded", "finish_reason=length"];

impl BackendError {
    fn body_mentions(&self, needles: &[&str]) -> bool {
        let body = self.body_text.to_lowercase();
        needles.iter().any(|needle| body.contains(needle))
    }

    #[must_use]
    pub fn is_context_too_long(&self) -> bool {
        matches!(self.status, Some(400 | 422)) && self.body_mentions(CONTEXT_TOO_LONG)
    }

    #[must_use]
    pub fn is_response_too_long(&self) -> bool {
        self.status == Some(422) && self.body_mentions(RESPONSE_TOO_LONG)
    }

    #[must_use]
    pub fn is_invalid_model(&self) -> bool {
        self.status == Some(400) && self.body_mentions(&["invalid_model"])
    }

    /// The request identifier the provider stamped on its answer.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.headers
            .get("x-request-id")
            .or_else(|| self.headers.get("request-id"))
            .map(String::as_str)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.status == Some(401) {
            formatter.write_str("The provider rejected the API key")?;
            if let Some(origin) = &self.api_key_origin {
                write!(formatter, " read from {origin}")?;
            }
            return formatter.write_str("; check the key, then retry.");
        }
        if self.status == Some(429) {
            return formatter
                .write_str("The provider is rate limiting requests; wait a little, then retry.");
        }
        if self.is_invalid_model() {
            write!(
                formatter,
                "{} does not serve the model `{}`. Pick another configured model with /model, \
                 or correct its name with /config.",
                self.provider, self.model
            )?;
            if let Some(message) = &self.parsed_error {
                write!(formatter, "\nThe provider said: {message}")?;
            }
            return Ok(());
        }
        let status = self.status.map_or_else(
            || "none".to_owned(),
            |status| {
                reqwest::StatusCode::from_u16(status)
                    .ok()
                    .and_then(|code| code.canonical_reason())
                    .map_or_else(|| status.to_string(), |reason| format!("{status} {reason}"))
            },
        );
        let summary = &self.payload_summary;
        write!(
            formatter,
            "The {} backend failed.\nStatus: {status}\nReason: {}\nRequest: {}\nEndpoint: {}\n\
             Model: {}\nProvider said: {}\nBody: {}\nRequest summary: {} messages, about {} \
             characters, temperature {}, {}",
            self.provider,
            self.reason.as_deref().unwrap_or("none"),
            self.request_id().unwrap_or("none"),
            self.endpoint,
            self.model,
            self.parsed_error.as_deref().unwrap_or("none"),
            excerpt(&self.body_text),
            summary.message_count,
            summary.approx_chars,
            summary.temperature,
            if summary.has_tools {
                "with tools"
            } else {
                "without tools"
            },
        )
    }
}

impl std::error::Error for BackendError {}

/// The first 400 characters of a body on one line.
fn excerpt(body: &str) -> String {
    let flattened = super::sse::python_strip(body).replace('\n', " ");
    let mut characters = flattened.chars();
    let head: String = characters.by_ref().take(400).collect();
    if characters.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

/// The message an error body carries, read the way the reference's
/// `ErrorResponse.primary_message` reads it: `error.message`, then
/// `error.type`, then `message`, then `detail`. A body of another shape, or
/// one whose fields have other types, carries none.
#[must_use]
pub fn provider_message(body: &str) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return None;
    };
    let error = match fields.get("error") {
        None | Some(Value::Null) => None,
        Some(Value::Object(error)) => Some(error),
        Some(_) => return None,
    };
    let text = |name: &str| match fields.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    };
    let (Ok(message), Ok(detail)) = (text("message"), text("detail")) else {
        return None;
    };
    // An `error` object whose `message` is text or absent reads as the
    // reference's `ErrorDetail`, which carries nothing else; only one whose
    // `message` is some other value stays a mapping and names its `type`.
    if let Some(error) = error {
        match error.get("message") {
            Some(Value::String(message)) => return Some(message.clone()),
            None | Some(Value::Null) => {}
            Some(_) => {
                if let Some(Value::String(kind)) = error.get("type") {
                    return Some(format!("error type {kind}"));
                }
            }
        }
    }
    message
        .filter(|message| !message.is_empty())
        .or_else(|| detail.filter(|detail| !detail.is_empty()))
}

/// A local failure reading an answer, named as the reference's exception
/// class is, since that class is what decides how the loop reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    /// A malformed event stream line, or answer pieces that do not merge
    /// (`ValueError`).
    Value,
    /// A success body that is not JSON (`JSONDecodeError`).
    Json,
    /// An answer whose shape the reader refuses (`ValidationError`).
    Validation,
    /// A field read off a value that is not an object (`AttributeError`).
    Attribute,
    /// A field that is missing where the reader indexes it (`KeyError`).
    Key,
    /// A value of the wrong type where the reader computes with it
    /// (`TypeError`).
    Type,
    /// A stream event that reports an error (`RuntimeError`).
    Runtime,
    /// A 422 the Mistral client read as a validation report
    /// (`HTTPValidationError`).
    MistralValidation,
    /// An answer the Mistral client could not read into its models
    /// (`ResponseValidationError`).
    MistralResponse,
    /// A configuration the backend cannot run with (`ValueError` at
    /// construction).
    Configuration,
    /// Credentials that could not be minted (`RefreshError`,
    /// `DefaultCredentialsError`).
    Credentials,
}

impl LocalKind {
    /// The reference exception class this failure corresponds to.
    #[must_use]
    pub const fn reference_class(self) -> &'static str {
        match self {
            Self::Value | Self::Configuration => "ValueError",
            Self::Json => "JSONDecodeError",
            Self::Validation => "ValidationError",
            Self::Attribute => "AttributeError",
            Self::Key => "KeyError",
            Self::Type => "TypeError",
            Self::Runtime => "RuntimeError",
            Self::MistralValidation => "HTTPValidationError",
            Self::MistralResponse => "ResponseValidationError",
            Self::Credentials => "DefaultCredentialsError",
        }
    }
}

/// A local failure with its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFailure {
    pub kind: LocalKind,
    pub message: String,
}

impl LocalFailure {
    #[must_use]
    pub fn new(kind: LocalKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for LocalFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// How a backend call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendFailure {
    Backend(Box<BackendError>),
    Local(LocalFailure),
    /// A Responses stream reported a failure; the backend turns it into a
    /// [`BackendError`] once it stops retrying.
    ResponsesStream(super::responses::StreamError),
}

impl BackendFailure {
    #[must_use]
    pub fn local(kind: LocalKind, message: impl Into<String>) -> Self {
        Self::Local(LocalFailure::new(kind, message))
    }

    #[must_use]
    pub fn backend(&self) -> Option<&BackendError> {
        match self {
            Self::Backend(error) => Some(error),
            Self::Local(_) | Self::ResponsesStream(_) => None,
        }
    }
}

impl fmt::Display for BackendFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => error.fmt(formatter),
            Self::Local(failure) => failure.fmt(formatter),
            Self::ResponsesStream(error) => write!(
                formatter,
                "The Responses stream reported {}: {}",
                error.error_type, error.message
            ),
        }
    }
}

impl std::error::Error for BackendFailure {}

impl From<BackendError> for BackendFailure {
    fn from(error: BackendError) -> Self {
        Self::Backend(Box::new(error))
    }
}

impl From<LocalFailure> for BackendFailure {
    fn from(failure: LocalFailure) -> Self {
        Self::Local(failure)
    }
}

/// The code a failed turn publishes. Reference `TurnErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    RateLimit,
    ContextTooLong,
    ResponseTooLong,
    Refusal,
    IncompleteStream,
    InvalidModel,
    InvalidApiKey,
    BackendError,
    InternalError,
}

impl FailureCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::ContextTooLong => "context_too_long",
            Self::ResponseTooLong => "response_too_long",
            Self::Refusal => "refusal",
            Self::IncompleteStream => "incomplete_stream",
            Self::InvalidModel => "invalid_model",
            Self::InvalidApiKey => "invalid_api_key",
            Self::BackendError => "backend_error",
            Self::InternalError => "internal_error",
        }
    }
}

/// What a model call fails the turn with, once the loop classified it.
#[derive(Debug, Clone, PartialEq)]
pub enum CallFailure {
    /// The provider answered 429 once the backend stopped retrying.
    RateLimit {
        provider: String,
        model: String,
        cause: Box<BackendError>,
    },
    ContextTooLong {
        provider: String,
        model: String,
        cause: Box<BackendError>,
    },
    ResponseTooLong {
        provider: String,
        model: String,
        cause: Box<BackendError>,
    },
    /// The model declined to answer.
    Refusal {
        provider: String,
        model: String,
        category: Option<String>,
        explanation: Option<String>,
    },
    /// A stream ended without saying why.
    IncompleteStream { provider: String, model: String },
    /// The provider does not serve the model; the backend error is raised as
    /// it is.
    InvalidModel(Box<BackendError>),
    /// Anything else, wrapped with the provider and model it came from.
    Wrapped {
        provider: String,
        model: String,
        cause: WrappedCause,
    },
}

/// What a wrapped failure wraps.
#[derive(Debug, Clone, PartialEq)]
pub enum WrappedCause {
    Backend(Box<BackendError>),
    Local(LocalFailure),
    /// An answer that reported no usage at all.
    MissingUsage {
        streaming: bool,
    },
}

impl CallFailure {
    /// Reference `public_error`'s code.
    #[must_use]
    pub fn code(&self) -> FailureCode {
        match self {
            Self::RateLimit { .. } => FailureCode::RateLimit,
            Self::ContextTooLong { .. } => FailureCode::ContextTooLong,
            Self::ResponseTooLong { .. } => FailureCode::ResponseTooLong,
            Self::Refusal { .. } => FailureCode::Refusal,
            Self::IncompleteStream { .. } => FailureCode::IncompleteStream,
            Self::InvalidModel(_) => FailureCode::InvalidModel,
            Self::Wrapped {
                cause: WrappedCause::Backend(error),
                ..
            } => {
                if error.is_invalid_model() {
                    FailureCode::InvalidModel
                } else if matches!(error.status, Some(401 | 403)) {
                    FailureCode::InvalidApiKey
                } else {
                    FailureCode::BackendError
                }
            }
            Self::Wrapped { .. } => FailureCode::InternalError,
        }
    }

    /// Reference `public_error`'s details: the provider, the model and a
    /// refusal's reasons, where the failure or its cause carries them.
    #[must_use]
    pub fn details(&self) -> Option<Map<String, Value>> {
        let mut details = Map::new();
        let mut put = |name: &str, value: Option<&String>| {
            if let Some(value) = value {
                details.insert(name.to_owned(), Value::String(value.clone()));
            }
        };
        match self {
            Self::RateLimit {
                provider, model, ..
            }
            | Self::ContextTooLong {
                provider, model, ..
            }
            | Self::ResponseTooLong {
                provider, model, ..
            }
            | Self::IncompleteStream { provider, model } => {
                put("provider", Some(provider));
                put("model", Some(model));
            }
            Self::Refusal {
                provider,
                model,
                category,
                explanation,
            } => {
                put("provider", Some(provider));
                put("model", Some(model));
                put("category", category.as_ref());
                put("explanation", explanation.as_ref());
            }
            Self::InvalidModel(error)
            | Self::Wrapped {
                cause: WrappedCause::Backend(error),
                ..
            } => {
                put("provider", Some(&error.provider));
                put("model", Some(&error.model));
            }
            Self::Wrapped { .. } => {}
        }
        (!details.is_empty()).then_some(details)
    }

    /// The backend error behind this failure, if one is.
    #[must_use]
    pub fn backend_error(&self) -> Option<&BackendError> {
        match self {
            Self::RateLimit { cause, .. }
            | Self::ContextTooLong { cause, .. }
            | Self::ResponseTooLong { cause, .. }
            | Self::InvalidModel(cause)
            | Self::Wrapped {
                cause: WrappedCause::Backend(cause),
                ..
            } => Some(cause),
            _ => None,
        }
    }
}

impl fmt::Display for CallFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimit { .. } => formatter.write_str(
                "The provider kept rate limiting the request. Wait a moment, then try again.",
            ),
            Self::ContextTooLong { .. } => formatter.write_str(
                "The conversation no longer fits the model's context window. Undo recent steps \
                 with /rewind, then summarize the conversation with /compact.",
            ),
            Self::ResponseTooLong { .. } => {
                formatter.write_str("The model's answer ran past its output token limit.")
            }
            Self::Refusal {
                category,
                explanation,
                ..
            } => {
                formatter.write_str("The model declined this request and stopped.")?;
                if let Some(category) = category {
                    write!(formatter, " Category: {category}.")?;
                }
                match explanation {
                    Some(explanation) => write!(formatter, " {explanation}"),
                    None => {
                        formatter.write_str(" Rephrase the request, or start a new conversation.")
                    }
                }
            }
            Self::IncompleteStream { provider, model } => write!(
                formatter,
                "The answer from {provider} ({model}) stopped before the model said it was done."
            ),
            Self::InvalidModel(error) => error.fmt(formatter),
            Self::Wrapped {
                provider,
                model,
                cause,
            } => {
                write!(formatter, "The call to {provider} for {model} failed: ")?;
                match cause {
                    WrappedCause::Backend(error) => error.fmt(formatter),
                    WrappedCause::Local(failure) => failure.fmt(formatter),
                    WrappedCause::MissingUsage { streaming } => {
                        formatter.write_str(if *streaming {
                            "the streamed answer reported no token usage"
                        } else {
                            "the answer reported no token usage"
                        })
                    }
                }
            }
        }
    }
}

impl std::error::Error for CallFailure {}
