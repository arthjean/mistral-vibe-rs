//! What the conversation engine asks of a model and gets back.
//!
//! The engine speaks to a model through [`crate::engine::CompletionProvider`]
//! with a [`ProviderInput`] and reads back [`ProviderChunk`]s, which fold into
//! an [`AssistantMessage`]. The backends that reach a real provider live in
//! [`crate::llm`]; [`crate::llm::completion`] adapts them to this contract.

use std::collections::BTreeMap;
use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::events::{ModelMessage, ModelToolCall};
use crate::llm::error::CallFailure;

pub mod config;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    #[serde(skip)]
    pub turn_id: Option<String>,
    /// The session the request belongs to, which the provider pins its
    /// affinity to. Reference `x-affinity`.
    #[serde(skip)]
    pub session_id: Option<String>,
    #[serde(skip)]
    pub model_override: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default = "default_streaming")]
    pub stream: bool,
    #[serde(default)]
    pub images: Vec<ImageInput>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: RequestLimits,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInput {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLimits {
    /// The completion budget of one request; unset leaves it to the provider,
    /// as the reference's `max_tokens=None` does.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// The temperature in thousandths, when the caller sets one rather than
    /// taking the model's.
    #[serde(default)]
    pub temperature_millis: Option<u16>,
}

const fn default_streaming() -> bool {
    true
}

pub type ProviderChunkStream<'a> =
    Pin<Box<dyn Stream<Item = Result<ProviderChunk, ProviderError>> + Send + 'a>>;

pub struct ProviderStream<'a> {
    pub correlation_id: Option<String>,
    pub chunks: ProviderChunkStream<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderChunk {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    /// A provider-native reasoning item the next request replays verbatim.
    ReasoningPayload {
        payload: Map<String, Value>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Refusal {
        message: String,
    },
    Stop {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// What a backend reports about the request it makes, which is what a model
/// call span carries beyond what the turn itself knows. Reference
/// `GenericBackend._model_call_span`, whose provider name, API style and URL
/// come from the provider entry rather than from the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCallDescriptor {
    pub provider_name: String,
    pub api_style: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub text: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_payloads: Vec<Map<String, Value>>,
    #[serde(default)]
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: Usage,
    #[serde(default)]
    pub refusal: Option<String>,
    pub stop_reason: String,
    #[serde(default)]
    pub correlation_id: Option<String>,
}

/// A model call the loop classified, with what it leaves in the history.
/// Reference `_chat_streaming` and `_complete`, which record an interrupted
/// answer's text or a refused answer before they raise.
#[derive(Debug, Clone, PartialEq)]
pub struct CallError {
    pub failure: CallFailure,
    /// The message the history still receives.
    pub appended: Option<AssistantMessage>,
}

#[derive(Debug, Error, PartialEq)]
pub enum ProviderError {
    /// A failed model call, classified the way the reference loop raises it.
    #[error("{}", .0.failure)]
    Call(Box<CallError>),
    #[error("invalid provider request: {0}")]
    InvalidRequest(String),
    #[error("provider context window is full")]
    ContextOverflow,
    #[error("provider returned HTTP {status}")]
    HttpStatus { status: u16 },
    #[error("malformed provider stream: {0}")]
    MalformedStream(String),
    #[error("provider response omitted final usage")]
    MissingUsage,
    #[error("provider response was refused: {0}")]
    Refusal(String),
}

impl ProviderError {
    /// Whether the conversation outgrew the model's window, which a turn
    /// answers by compacting once.
    #[must_use]
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::ContextOverflow => true,
            Self::Call(call) => matches!(call.failure, CallFailure::ContextTooLong { .. }),
            _ => false,
        }
    }

    /// Whether the model declined to answer.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        match self {
            Self::Refusal(_) => true,
            Self::Call(call) => matches!(call.failure, CallFailure::Refusal { .. }),
            _ => false,
        }
    }

    /// Whether the answer ran past its output budget.
    #[must_use]
    pub fn is_response_too_long(&self) -> bool {
        matches!(self, Self::Call(call) if matches!(call.failure, CallFailure::ResponseTooLong { .. }))
    }

    /// The classified failure, when a backend produced one.
    #[must_use]
    pub fn call_failure(&self) -> Option<&CallFailure> {
        match self {
            Self::Call(call) => Some(&call.failure),
            _ => None,
        }
    }
}

impl crate::tracing::TracedError for ProviderError {
    fn error_type(&self) -> &'static str {
        "ProviderError"
    }

    /// Reference `_backend_error_from`: the backend error a failure carries,
    /// through whatever wraps it. The provider is left for the span to fill in
    /// when the failure names none.
    fn backend_failure(&self) -> Option<crate::tracing::BackendFailure> {
        match self {
            Self::Call(call) => {
                call.failure
                    .backend_error()
                    .map(|error| crate::tracing::BackendFailure {
                        provider: Some(error.provider.clone()),
                        status: error.status.map(i64::from),
                    })
            }
            Self::HttpStatus { status } => Some(crate::tracing::BackendFailure {
                provider: None,
                status: Some(i64::from(*status)),
            }),
            Self::ContextOverflow => Some(crate::tracing::BackendFailure {
                provider: None,
                status: Some(413),
            }),
            _ => None,
        }
    }
}

/// Folds a stream's chunks into the answer: text and reasoning concatenate,
/// tool call pieces merge by identifier, the last usage and stop win.
pub(crate) fn aggregate_provider_chunks(
    chunks: Vec<ProviderChunk>,
    correlation_id: Option<String>,
) -> Result<AssistantMessage, ProviderError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_payloads = Vec::new();
    let mut tool_calls = Vec::<ModelToolCall>::new();
    let mut usage: Option<Usage> = None;
    let mut refusal = None;
    let mut stop_reason = None;
    for chunk in chunks {
        match chunk {
            ProviderChunk::Text { text: delta } => text.push_str(&delta),
            ProviderChunk::Reasoning { text: delta } => reasoning.push_str(&delta),
            ProviderChunk::ReasoningPayload { payload } => reasoning_payloads.push(payload),
            ProviderChunk::ToolCall {
                id,
                name,
                arguments,
            } => {
                if let Some(existing) = tool_calls.iter_mut().find(|call| call.id == id) {
                    if !name.is_empty() {
                        existing.name = name;
                    }
                    existing.arguments.push_str(&arguments);
                } else {
                    tool_calls.push(ModelToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            ProviderChunk::Usage {
                input_tokens,
                output_tokens,
            } => match &mut usage {
                Some(usage) => {
                    if input_tokens > 0 {
                        usage.input_tokens = input_tokens;
                    }
                    if output_tokens > 0 {
                        usage.output_tokens = output_tokens;
                    }
                }
                None => {
                    usage = Some(Usage {
                        input_tokens,
                        output_tokens,
                    });
                }
            },
            ProviderChunk::Refusal { message } => refusal = Some(message),
            ProviderChunk::Stop { reason } => stop_reason = Some(reason),
        }
    }
    let usage = usage.ok_or(ProviderError::MissingUsage)?;
    if let Some(message) = &refusal {
        return Err(ProviderError::Refusal(message.clone()));
    }
    let stop_reason = stop_reason.ok_or_else(|| {
        ProviderError::MalformedStream("provider response omitted stop state".to_owned())
    })?;
    Ok(AssistantMessage {
        text,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        reasoning_payloads,
        tool_calls,
        usage,
        refusal,
        stop_reason,
        correlation_id,
    })
}
