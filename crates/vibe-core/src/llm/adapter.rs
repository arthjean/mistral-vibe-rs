//! What every API dialect shares: the request it prepares, the reader that
//! turns response events into chunks, and the lenient readers for the values a
//! provider sends back.
//!
//! Reference `APIAdapter` (`vibe/core/llm/backend/base.py`) and its five
//! implementations, picked by a provider's `api_style` in `generic.py`.

use serde_json::{Map, Value, json};

use super::error::{BackendFailure, LocalFailure, LocalKind};
use super::types::{Chunk, Message, Role, Tool, ToolCall, ToolChoice, Usage};
use crate::provider::config::ProviderConfig;

/// Everything a dialect needs to write one request.
#[derive(Debug, Clone, Copy)]
pub struct RequestParts<'a> {
    pub model_name: &'a str,
    pub messages: &'a [Message],
    pub temperature: f64,
    pub tools: Option<&'a [Tool]>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<&'a ToolChoice>,
    pub streaming: bool,
    pub provider: &'a ProviderConfig,
    pub api_key: Option<&'a str>,
    pub thinking: &'a str,
}

impl RequestParts<'_> {
    /// The tools, when there is at least one.
    #[must_use]
    pub fn declared_tools(&self) -> Option<&[Tool]> {
        self.tools.filter(|tools| !tools.is_empty())
    }
}

/// A request ready to send: the path under the base, the headers the dialect
/// sets, the JSON body, and the base itself when the dialect decides it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRequest {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub base_url: Option<String>,
}

/// The API styles the generic backend speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    OpenAi,
    Reasoning,
    Anthropic,
    Responses,
    VertexAnthropic,
}

impl Dialect {
    /// The dialect a provider's `api_style` names.
    #[must_use]
    pub fn for_style(style: &str) -> Option<Self> {
        match style {
            "openai" => Some(Self::OpenAi),
            "reasoning" => Some(Self::Reasoning),
            "anthropic" => Some(Self::Anthropic),
            "openai-responses" => Some(Self::Responses),
            "vertex-anthropic" => Some(Self::VertexAnthropic),
            _ => None,
        }
    }

    /// A fresh reader for one response: several dialects keep state while a
    /// stream is read, so a reader never outlives the response it reads.
    #[must_use]
    pub fn reader(self) -> Reader {
        match self {
            Self::OpenAi => Reader::OpenAi,
            Self::Reasoning => Reader::Reasoning,
            Self::Anthropic | Self::VertexAnthropic => {
                Reader::Anthropic(super::anthropic::AnthropicReader::default())
            }
            Self::Responses => Reader::Responses(super::responses::ResponsesReader::default()),
        }
    }

    /// Writes the request for `parts`.
    ///
    /// # Errors
    ///
    /// A request the dialect cannot write, such as a Vertex provider without
    /// its project or region.
    pub fn prepare(
        self,
        parts: &RequestParts<'_>,
        vertex_token: Option<&str>,
    ) -> Result<PreparedRequest, LocalFailure> {
        match self {
            Self::OpenAi => Ok(super::chat::prepare_openai(parts)),
            Self::Reasoning => Ok(super::chat::prepare_reasoning(parts)),
            Self::Anthropic => Ok(super::anthropic::prepare(parts)),
            Self::VertexAnthropic => super::anthropic::prepare_vertex(parts, vertex_token),
            Self::Responses => Ok(super::responses::prepare(parts)),
        }
    }
}

/// Reads the events or the body of one response.
#[derive(Debug)]
pub enum Reader {
    OpenAi,
    Reasoning,
    Anthropic(super::anthropic::AnthropicReader),
    Responses(super::responses::ResponsesReader),
}

impl Reader {
    /// The chunk one event or one whole body carries.
    ///
    /// # Errors
    ///
    /// An event the dialect refuses, or one that reports a failure.
    pub fn parse(
        &mut self,
        data: &Value,
        provider: &ProviderConfig,
    ) -> Result<Chunk, BackendFailure> {
        let object = data.as_object().ok_or_else(not_an_object)?;
        match self {
            Self::OpenAi => super::chat::parse_openai(object, provider).map_err(Into::into),
            Self::Reasoning => super::chat::parse_reasoning(object).map_err(Into::into),
            Self::Anthropic(reader) => reader.parse(object).map_err(Into::into),
            Self::Responses(reader) => reader.parse(object),
        }
    }

    /// The chunks one streamed event releases. Only the Responses dialect
    /// holds chunks back, until the stream shows output.
    ///
    /// # Errors
    ///
    /// As [`Reader::parse`].
    pub fn stream_event(
        &mut self,
        data: &Value,
        provider: &ProviderConfig,
    ) -> Result<Vec<Chunk>, BackendFailure> {
        if let Self::Responses(reader) = self {
            let object = data.as_object().ok_or_else(not_an_object)?;
            return reader.stream_event(object);
        }
        self.parse(data, provider).map(|chunk| vec![chunk])
    }

    /// What the reader still holds when the stream ends.
    pub fn finish(&mut self) -> Vec<Chunk> {
        match self {
            Self::Responses(reader) => reader.finish(),
            _ => Vec::new(),
        }
    }
}

fn not_an_object() -> BackendFailure {
    BackendFailure::local(
        LocalKind::Attribute,
        "a response event is not a JSON object",
    )
}

/// `{"Content-Type": "application/json", "Authorization": "Bearer ..."}`.
#[must_use]
pub fn bearer_headers(api_key: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        headers.push(("Authorization".to_owned(), format!("Bearer {key}")));
    }
    headers
}

/// The chat-completions payload both chat dialects share. Reference
/// `build_chat_payload`: the temperature is always sent, the effort only when
/// thinking is on, and the tools, the choice and the budget only when set.
#[must_use]
pub fn chat_payload(parts: &RequestParts<'_>, messages: Vec<Value>) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("model".to_owned(), json!(parts.model_name));
    payload.insert("messages".to_owned(), Value::Array(messages));
    payload.insert("temperature".to_owned(), json!(parts.temperature));
    if parts.thinking != "off" {
        payload.insert("reasoning_effort".to_owned(), json!(parts.thinking));
    }
    if let Some(tools) = parts.declared_tools() {
        payload.insert(
            "tools".to_owned(),
            Value::Array(tools.iter().map(Tool::chat_declaration).collect()),
        );
    }
    if let Some(choice) = parts.tool_choice {
        payload.insert(
            "tool_choice".to_owned(),
            match choice {
                ToolChoice::Tool(tool) => tool.chat_declaration(),
                keyword => json!(keyword.keyword()),
            },
        );
    }
    if let Some(max_tokens) = parts.max_tokens {
        payload.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    payload
}

/// Adds the streaming switch and its options to a chat payload.
pub fn finalize_chat(payload: &mut Map<String, Value>, streaming: bool, options: Value) {
    if streaming {
        payload.insert("stream".to_owned(), Value::Bool(true));
        payload.insert("stream_options".to_owned(), options);
    }
}

/// Python truthiness of a JSON value.
#[must_use]
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// `str(value)` of a value `json.loads` produced.
#[must_use]
pub fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(text) => text.clone(),
        other => super::python_json::Ordered::from(other).dumps(false),
    }
}

/// A token count as a lenient integer field reads it: an integer, a float
/// without a fraction, or a string of digits. Anything else is refused.
///
/// # Errors
///
/// A value no integer field would accept.
pub fn lax_count(value: &Value) -> Result<u64, LocalFailure> {
    let refused = || LocalFailure::new(LocalKind::Validation, "a token count is not an integer");
    match value {
        Value::Number(number) => {
            if let Some(count) = number.as_u64() {
                Ok(count)
            } else if number.as_i64().is_some() {
                Ok(0)
            } else {
                let float = number.as_f64().ok_or_else(refused)?;
                if float.fract() == 0.0 && float.is_finite() {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    Ok(float.max(0.0) as u64)
                } else {
                    Err(refused())
                }
            }
        }
        Value::String(text) => text.trim().parse::<u64>().map_err(|_| refused()),
        _ => Err(refused()),
    }
}

/// A count read with `.get(name, 0)`: absent reads as zero.
///
/// # Errors
///
/// As [`lax_count`].
pub fn count_field(object: &Map<String, Value>, name: &str) -> Result<u64, LocalFailure> {
    object.get(name).map_or(Ok(0), lax_count)
}

/// `data.get(name) or {}` over a value that must then be an object.
///
/// # Errors
///
/// A truthy value that is not an object.
pub fn object_or_empty<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a Map<String, Value>>, LocalFailure> {
    match object.get(name) {
        Some(value) if truthy(value) => value.as_object().map(Some).ok_or_else(|| {
            LocalFailure::new(LocalKind::Attribute, format!("`{name}` is not an object"))
        }),
        _ => Ok(None),
    }
}

/// Chat-completions usage: `prompt_tokens`, `completion_tokens`, and the
/// cached part under `prompt_tokens_details`.
///
/// # Errors
///
/// Counts that are not integers.
pub fn chat_usage(data: &Map<String, Value>) -> Result<Usage, LocalFailure> {
    let Some(usage) = object_or_empty(data, "usage")? else {
        return Ok(Usage::default());
    };
    let cached = match object_or_empty(usage, "prompt_tokens_details")? {
        Some(details) => count_field(details, "cached_tokens")?,
        None => 0,
    };
    Ok(Usage {
        prompt_tokens: count_field(usage, "prompt_tokens")?,
        completion_tokens: count_field(usage, "completion_tokens")?,
        cached_tokens: cached,
    })
}

/// The first choice's finish reason, as a string.
///
/// # Errors
///
/// A first choice that is not an object.
pub fn chat_finish_reason(data: &Map<String, Value>) -> Result<Option<String>, LocalFailure> {
    let choices = match data.get("choices") {
        Some(Value::Array(choices)) if !choices.is_empty() => choices,
        _ => return Ok(None),
    };
    let first = choices[0]
        .as_object()
        .ok_or_else(|| LocalFailure::new(LocalKind::Attribute, "a choice is not a JSON object"))?;
    Ok(match first.get("finish_reason") {
        None | Some(Value::Null) => None,
        Some(reason) => Some(python_str(reason)),
    })
}

/// A content value as the message model coerces it: a string as it is, a
/// list of parts as their texts joined by newlines.
#[must_use]
pub fn coerce_content(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .map(|part| match part.get("text") {
                    Some(Value::String(text)) if part.is_object() => text.clone(),
                    _ => python_str(part),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        other => Some(python_str(other)),
    }
}

/// An optional string field of the message model.
///
/// # Errors
///
/// A value of another type.
pub fn optional_string(
    object: &Map<String, Value>,
    name: &str,
) -> Result<Option<String>, LocalFailure> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(LocalFailure::new(
            LocalKind::Validation,
            format!("`{name}` is not a string"),
        )),
    }
}

/// An optional integer field of the message model.
///
/// # Errors
///
/// A value no integer field accepts.
pub fn optional_index(value: Option<&Value>) -> Result<Option<u64>, LocalFailure> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(_)) => Err(LocalFailure::new(
            LocalKind::Validation,
            "a tool call index is not an integer",
        )),
        Some(value) => lax_count(value).map(Some),
    }
}

/// A message as the message model validates a provider's dictionary. Content
/// that is absent reads as empty, a role that is absent as `assistant`.
///
/// # Errors
///
/// A field of the wrong type.
pub fn validate_message(object: &Map<String, Value>) -> Result<Message, LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::Validation, what.to_owned());
    let role = match object.get("role") {
        None => Role::Assistant,
        Some(Value::String(role)) => Role::parse(role).ok_or_else(|| refused("unknown role"))?,
        Some(_) => return Err(refused("the role is not a string")),
    };
    let content = match object.get("content") {
        None => Some(String::new()),
        Some(value) => coerce_content(value),
    };
    let reasoning_content = object.get("reasoning_content").and_then(coerce_content);
    let tool_calls = match object.get("tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Array(calls)) => Some(
            calls
                .iter()
                .map(validate_tool_call)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(refused("`tool_calls` is not a list")),
    };
    let reasoning_payloads = match object.get("reasoning_payloads") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| {
                    item.as_object()
                        .cloned()
                        .ok_or_else(|| refused("a reasoning payload is not an object"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(refused("`reasoning_payloads` is not a list")),
    };
    Ok(Message {
        role,
        content,
        images: Vec::new(),
        injected: false,
        reasoning_content,
        reasoning_payloads,
        tool_calls,
        name: optional_string(object, "name")?,
        tool_call_id: optional_string(object, "tool_call_id")?,
    })
}

fn validate_tool_call(value: &Value) -> Result<ToolCall, LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::Validation, what.to_owned());
    let call = value
        .as_object()
        .ok_or_else(|| refused("a tool call is not an object"))?;
    if let Some(kind) = call.get("type")
        && kind != "function"
    {
        return Err(refused("a tool call is not a function call"));
    }
    let (name, arguments) = match call.get("function") {
        None => (None, None),
        Some(Value::Object(function)) => (
            optional_string(function, "name")?,
            optional_string(function, "arguments")?,
        ),
        Some(_) => return Err(refused("a tool call's function is not an object")),
    };
    Ok(ToolCall {
        id: optional_string(call, "id")?,
        index: optional_index(call.get("index"))?,
        name,
        arguments,
    })
}

/// What the loop keeps of a message a backend produced: the role, the text,
/// the reasoning and its payloads, and the tool calls when there are any.
/// Reference `APIToolFormatHandler.process_api_response_message`.
#[must_use]
pub fn processed(message: Message) -> Message {
    Message {
        role: message.role,
        content: message.content,
        images: Vec::new(),
        injected: false,
        reasoning_content: message.reasoning_content,
        reasoning_payloads: message.reasoning_payloads,
        tool_calls: message.tool_calls.filter(|calls| !calls.is_empty()),
        name: None,
        tool_call_id: None,
    }
}
