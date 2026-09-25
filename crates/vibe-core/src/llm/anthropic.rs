//! The Anthropic Messages dialect, and its Vertex AI variant.
//!
//! Reference `AnthropicAdapter` and `AnthropicMapper`
//! (`vibe/core/llm/backend/anthropic.py`), and `VertexAnthropicAdapter`
//! (`vibe/core/llm/backend/vertex.py`). The system prompt travels as a cached
//! block, the last user block is marked cacheable, tool results join the user
//! turn before them, and an assistant's native reasoning blocks are replayed
//! first. Thinking is adaptive: on when the model asks for it, and forced to
//! `medium` when the history already carries reasoning, since the API refuses
//! a thinking block in a conversation that has thinking off.
//!
//! A streamed reasoning block arrives across several events; it is rebuilt
//! here and released as a payload when its block closes.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::adapter::{PreparedRequest, RequestParts, count_field};
use super::error::{LocalFailure, LocalKind};
use super::python_json::Ordered;
use super::types::{Chunk, Message, Role, StopInfo, ToolCall, ToolChoice, Usage};

const ENDPOINT: &str = "/v1/messages";
const API_VERSION: &str = "2023-06-01";
const BETA_FEATURES: &str = "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14,prompt-caching-2024-07-31,context-1m-2025-08-07";
const ADAPTIVE_MAX_TOKENS: u64 = 32_768;
const DEFAULT_MAX_TOKENS: u64 = 8_192;
const VERTEX_VERSION: &str = "vertex-2023-10-16";

fn is_reasoning_block(block: &Map<String, Value>) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    )
}

/// A tool call identifier with every character outside `[A-Za-z0-9_-]`
/// replaced, which the Messages API requires.
#[must_use]
pub fn sanitize_tool_id(id: Option<&str>) -> String {
    id.unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The system prompt and the converted conversation.
fn convert_messages(messages: &[Message]) -> (Option<String>, Vec<Map<String, Value>>) {
    let mut system = None;
    let mut converted: Vec<Map<String, Value>> = Vec::new();
    for message in messages {
        match message.role {
            Role::System => system = Some(message.text().to_owned()),
            Role::User => {
                let mut content = Vec::new();
                if !message.text().is_empty() {
                    content.push(json!({"type": "text", "text": message.text()}));
                }
                content.extend(message.images.iter().map(|picture| {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": picture.mime_type,
                            "data": picture.data,
                        },
                    })
                }));
                let mut user = Map::new();
                user.insert("role".to_owned(), json!("user"));
                user.insert(
                    "content".to_owned(),
                    if content.is_empty() {
                        json!("")
                    } else {
                        Value::Array(content)
                    },
                );
                converted.push(user);
            }
            Role::Assistant => converted.push(convert_assistant(message)),
            Role::Tool => append_tool_result(&mut converted, message),
        }
    }
    (system, converted)
}

fn convert_assistant(message: &Message) -> Map<String, Value> {
    let mut content: Vec<Value> = message
        .reasoning_payloads
        .iter()
        .flatten()
        .filter(|block| is_reasoning_block(block))
        .map(|block| Value::Object(block.clone()))
        .collect();
    if !message.text().is_empty() {
        content.push(json!({"type": "text", "text": message.text()}));
    }
    for call in message.tool_calls.iter().flatten() {
        let input = serde_json::from_str::<Value>(
            call.arguments
                .as_deref()
                .filter(|arguments| !arguments.is_empty())
                .unwrap_or("{}"),
        )
        .unwrap_or_else(|_| json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": sanitize_tool_id(call.id.as_deref()),
            "name": call.name,
            "input": input,
        }));
    }
    let mut assistant = Map::new();
    assistant.insert("role".to_owned(), json!("assistant"));
    assistant.insert(
        "content".to_owned(),
        if content.is_empty() {
            json!("")
        } else {
            Value::Array(content)
        },
    );
    assistant
}

fn append_tool_result(converted: &mut Vec<Map<String, Value>>, message: &Message) {
    let result = json!({
        "type": "tool_result",
        "tool_use_id": sanitize_tool_id(message.tool_call_id.as_deref()),
        "content": message.text(),
    });
    if let Some(last) = converted
        .last_mut()
        .filter(|last| last.get("role").and_then(Value::as_str) == Some("user"))
    {
        match last.get_mut("content") {
            Some(Value::Array(blocks)) => blocks.push(result),
            Some(Value::String(text)) => {
                let text = std::mem::take(text);
                last.insert(
                    "content".to_owned(),
                    json!([{"type": "text", "text": text}, result]),
                );
            }
            _ => {}
        }
        return;
    }
    let mut user = Map::new();
    user.insert("role".to_owned(), json!("user"));
    user.insert("content".to_owned(), json!([result]));
    converted.push(user);
}

fn tool_declarations(parts: &RequestParts<'_>) -> Option<Value> {
    parts.declared_tools().map(|tools| {
        Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect(),
        )
    })
}

fn tool_choice(choice: Option<&ToolChoice>) -> Option<Value> {
    match choice? {
        ToolChoice::None => Some(json!({"type": "none"})),
        ToolChoice::Auto => Some(json!({"type": "auto"})),
        ToolChoice::Any | ToolChoice::Required => Some(json!({"type": "any"})),
        ToolChoice::Tool(tool) => Some(json!({"type": "tool", "name": tool.name})),
    }
}

fn history_has_reasoning(messages: &[Map<String, Value>]) -> bool {
    messages.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .filter_map(Value::as_object)
                        .any(is_reasoning_block)
                })
    })
}

fn apply_thinking(
    payload: &mut Map<String, Value>,
    messages: &[Map<String, Value>],
    max_tokens: Option<u64>,
    thinking: &str,
) {
    if thinking == "off" && !history_has_reasoning(messages) {
        payload.insert(
            "max_tokens".to_owned(),
            json!(max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );
        return;
    }
    let effort = if thinking == "off" {
        "medium"
    } else {
        thinking
    };
    payload.insert(
        "thinking".to_owned(),
        json!({"type": "adaptive", "display": "summarized"}),
    );
    payload.insert("output_config".to_owned(), json!({"effort": effort}));
    payload.insert(
        "max_tokens".to_owned(),
        json!(max_tokens.unwrap_or(ADAPTIVE_MAX_TOKENS)),
    );
}

/// Marks the last block of a closing user turn cacheable.
fn cache_last_user_block(messages: &mut [Map<String, Value>]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return;
    }
    let Some(Value::Array(blocks)) = last.get_mut("content") else {
        return;
    };
    if let Some(Value::Object(block)) = blocks.last_mut()
        && matches!(
            block.get("type").and_then(Value::as_str),
            Some("text" | "image" | "tool_result")
        )
    {
        block.insert("cache_control".to_owned(), json!({"type": "ephemeral"}));
    }
}

/// The payload both variants share, `model` aside.
fn payload(parts: &RequestParts<'_>, leading: Map<String, Value>) -> Map<String, Value> {
    let (system, mut messages) = convert_messages(parts.messages);
    let mut payload = leading;
    apply_thinking(&mut payload, &messages, parts.max_tokens, parts.thinking);
    if let Some(system) = system.filter(|system| !system.is_empty()) {
        payload.insert(
            "system".to_owned(),
            json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]),
        );
    }
    if let Some(tools) = tool_declarations(parts) {
        payload.insert("tools".to_owned(), tools);
    }
    if let Some(choice) = tool_choice(parts.tool_choice) {
        payload.insert("tool_choice".to_owned(), choice);
    }
    if parts.streaming {
        payload.insert("stream".to_owned(), Value::Bool(true));
    }
    cache_last_user_block(&mut messages);
    payload.insert(
        "messages".to_owned(),
        Value::Array(messages.into_iter().map(Value::Object).collect()),
    );
    payload
}

#[must_use]
pub fn prepare(parts: &RequestParts<'_>) -> PreparedRequest {
    let mut leading = Map::new();
    leading.insert("model".to_owned(), json!(parts.model_name));
    let body = payload(parts, leading);
    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("anthropic-version".to_owned(), API_VERSION.to_owned()),
        ("anthropic-beta".to_owned(), BETA_FEATURES.to_owned()),
    ];
    if let Some(key) = parts.api_key.filter(|key| !key.is_empty()) {
        headers.push(("x-api-key".to_owned(), key.to_owned()));
    }
    PreparedRequest {
        path: ENDPOINT.to_owned(),
        headers,
        body: Value::Object(body),
        base_url: None,
    }
}

/// # Errors
///
/// A provider without its project or region, and no access token.
pub fn prepare_vertex(
    parts: &RequestParts<'_>,
    token: Option<&str>,
) -> Result<PreparedRequest, LocalFailure> {
    let project = parts.provider.project_id.as_str();
    let region = parts.provider.region.as_str();
    if project.is_empty() {
        return Err(LocalFailure::new(
            LocalKind::Configuration,
            "a Vertex AI provider needs a project_id",
        ));
    }
    if region.is_empty() {
        return Err(LocalFailure::new(
            LocalKind::Configuration,
            "a Vertex AI provider needs a region",
        ));
    }
    let token = token.ok_or_else(|| {
        LocalFailure::new(
            LocalKind::Credentials,
            "no Vertex AI access token is available",
        )
    })?;
    let mut leading = Map::new();
    leading.insert("anthropic_version".to_owned(), json!(VERTEX_VERSION));
    let body = payload(parts, leading);
    Ok(PreparedRequest {
        path: super::vertex::endpoint(region, project, parts.model_name, parts.streaming),
        headers: vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Authorization".to_owned(), format!("Bearer {token}")),
            ("anthropic-beta".to_owned(), String::new()),
        ],
        body: Value::Object(body),
        base_url: Some(super::vertex::base_url(region)),
    })
}

/// A stop reason with the provider's details, as `_parse_stop_info` reads it.
fn stop_info(
    reason: Option<&Value>,
    details: Option<&Value>,
) -> Result<Option<StopInfo>, LocalFailure> {
    let reason = reason.filter(|reason| !reason.is_null());
    let details = details.and_then(Value::as_object);
    if reason.is_none() && details.is_none() {
        return Ok(None);
    }
    let text = |value: Option<&Value>| match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(LocalFailure::new(
            LocalKind::Validation,
            "a stop detail is not a string",
        )),
    };
    let reason = details.and_then(|details| details.get("reason")).or(reason);
    Ok(Some(StopInfo {
        reason: text(reason)?,
        category: text(details.and_then(|details| details.get("category")))?,
        explanation: text(details.and_then(|details| details.get("explanation")))?,
    }))
}

/// Input tokens with the cached ones folded back in, which is what the
/// OpenTelemetry convention calls the prompt.
fn prompt_usage(usage: &Map<String, Value>) -> Result<Usage, LocalFailure> {
    let cache_read = count_field(usage, "cache_read_input_tokens")?;
    Ok(Usage {
        prompt_tokens: count_field(usage, "input_tokens")?
            + count_field(usage, "cache_creation_input_tokens")?
            + cache_read,
        completion_tokens: 0,
        cached_tokens: cache_read,
    })
}

fn empty() -> Chunk {
    Chunk::of(Message::assistant())
}

/// Reads one Messages response, whole or streamed.
#[derive(Debug, Default)]
pub struct AnthropicReader {
    open_reasoning: BTreeMap<i64, Map<String, Value>>,
}

const STREAM_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
    "error",
];

fn index_of(data: &Map<String, Value>) -> i64 {
    data.get("index").and_then(Value::as_i64).unwrap_or(0)
}

fn object_field<'a>(
    data: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a Map<String, Value>>, LocalFailure> {
    match data.get(name) {
        None => Ok(None),
        Some(Value::Object(object)) => Ok(Some(object)),
        Some(_) => Err(LocalFailure::new(
            LocalKind::Attribute,
            format!("`{name}` is not an object"),
        )),
    }
}

impl AnthropicReader {
    /// # Errors
    ///
    /// An `error` event, and fields of the wrong type.
    pub fn parse(&mut self, data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
        match data.get("type").and_then(Value::as_str) {
            Some(kind) if STREAM_EVENTS.contains(&kind) => self.event(kind, data),
            _ => whole_response(data),
        }
    }

    fn event(&mut self, kind: &str, data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
        match kind {
            "message_start" => {
                self.open_reasoning.clear();
                let usage = object_field(data, "message")?
                    .map(|message| object_field(message, "usage"))
                    .transpose()?
                    .flatten()
                    .filter(|usage| !usage.is_empty());
                match usage {
                    None => Ok(empty()),
                    Some(usage) => Ok(Chunk {
                        usage: Some(prompt_usage(usage)?),
                        ..empty()
                    }),
                }
            }
            "content_block_start" => {
                let block = object_field(data, "content_block")?
                    .cloned()
                    .unwrap_or_default();
                let index = index_of(data);
                match block.get("type").and_then(Value::as_str) {
                    Some("thinking") => {
                        let thinking = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        self.open_reasoning.insert(index, block);
                        Ok(Chunk::of(Message {
                            reasoning_content: Some(thinking),
                            ..Message::assistant()
                        }))
                    }
                    Some("redacted_thinking") => {
                        self.open_reasoning.insert(index, block);
                        Ok(empty())
                    }
                    Some("tool_use") => Ok(Chunk::of(Message {
                        tool_calls: Some(vec![ToolCall {
                            id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                            index: u64::try_from(index).ok(),
                            name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                            arguments: Some(String::new()),
                        }]),
                        ..Message::assistant()
                    })),
                    _ => Ok(empty()),
                }
            }
            "content_block_delta" => {
                let delta = object_field(data, "delta")?.cloned().unwrap_or_default();
                let index = index_of(data);
                let text = |name: &str| {
                    delta
                        .get(name)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => Ok(Chunk::of(Message::assistant().with_content(text("text")))),
                    "thinking_delta" => {
                        let thinking = text("thinking");
                        if let Some(block) = self.open_reasoning.get_mut(&index)
                            && !block.is_empty()
                        {
                            append_text(block, "thinking", &thinking);
                        }
                        Ok(Chunk::of(Message {
                            reasoning_content: Some(thinking),
                            ..Message::assistant()
                        }))
                    }
                    "signature_delta" => {
                        if let Some(block) = self.open_reasoning.get_mut(&index)
                            && !block.is_empty()
                        {
                            append_text(block, "signature", &text("signature"));
                        }
                        Ok(empty())
                    }
                    "input_json_delta" => Ok(Chunk::of(Message {
                        tool_calls: Some(vec![ToolCall {
                            id: None,
                            index: u64::try_from(index).ok(),
                            name: None,
                            arguments: Some(text("partial_json")),
                        }]),
                        ..Message::assistant()
                    })),
                    _ => Ok(empty()),
                }
            }
            "content_block_stop" => {
                let block = self
                    .open_reasoning
                    .remove(&index_of(data))
                    .filter(|block| !block.is_empty());
                Ok(Chunk::of(Message {
                    reasoning_payloads: block.map(|block| vec![block]),
                    ..Message::assistant()
                }))
            }
            "message_delta" => {
                let delta = object_field(data, "delta")?.cloned().unwrap_or_default();
                let usage = match object_field(data, "usage")? {
                    Some(usage) if !usage.is_empty() => Some(Usage {
                        prompt_tokens: 0,
                        completion_tokens: count_field(usage, "output_tokens")?,
                        cached_tokens: 0,
                    }),
                    _ => None,
                };
                Ok(Chunk {
                    usage,
                    stop: stop_info(delta.get("stop_reason"), delta.get("stop_details"))?,
                    ..empty()
                })
            }
            "error" => {
                let error = object_field(data, "error")?.cloned().unwrap_or_default();
                let kind = error
                    .get("type")
                    .map_or_else(|| "unknown_error".to_owned(), super::adapter::python_str);
                let message = error.get("message").map_or_else(
                    || "unknown streaming error".to_owned(),
                    super::adapter::python_str,
                );
                Err(LocalFailure::new(
                    LocalKind::Runtime,
                    format!("The Anthropic stream reported {kind}: {message}"),
                ))
            }
            _ => Ok(empty()),
        }
    }
}

fn append_text(block: &mut Map<String, Value>, field: &str, piece: &str) {
    let mut text = block
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    text.push_str(piece);
    block.insert(field.to_owned(), Value::String(text));
}

/// A whole Messages response.
fn whole_response(data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut payloads = Vec::new();
    let mut calls = Vec::new();
    let blocks = match data.get("content") {
        None => Vec::new(),
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(_) => {
            return Err(LocalFailure::new(
                LocalKind::Type,
                "`content` is not a list",
            ));
        }
    };
    for (index, block) in blocks.iter().enumerate() {
        let block = block.as_object().ok_or_else(|| {
            LocalFailure::new(LocalKind::Attribute, "a content block is not an object")
        })?;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            Some("thinking" | "redacted_thinking") => {
                payloads.push(block.clone());
                thinking.push_str(
                    block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            Some("tool_use") => {
                let input = block
                    .get("input")
                    .map_or_else(|| "{}".to_owned(), |input| Ordered::from(input).dumps(true));
                calls.push(ToolCall {
                    id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                    index: u64::try_from(index).ok(),
                    name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                    arguments: Some(input),
                });
            }
            _ => {}
        }
    }
    let usage = match data.get("usage") {
        None => Usage::default(),
        Some(Value::Object(usage)) => Usage {
            completion_tokens: count_field(usage, "output_tokens")?,
            ..prompt_usage(usage)?
        },
        Some(_) => {
            return Err(LocalFailure::new(
                LocalKind::Attribute,
                "`usage` is not an object",
            ));
        }
    };
    Ok(Chunk {
        message: Message {
            content: (!text.is_empty()).then_some(text),
            reasoning_content: (!thinking.is_empty()).then_some(thinking),
            reasoning_payloads: (!payloads.is_empty()).then_some(payloads),
            tool_calls: (!calls.is_empty()).then_some(calls),
            ..Message::assistant()
        },
        usage: Some(usage),
        correlation_id: None,
        stop: stop_info(data.get("stop_reason"), data.get("stop_details"))?,
    })
}

/// Rewrites the tool inputs of a whole response from its raw text, keeping
/// each input's key order the way the provider wrote it.
pub fn reorder_tool_inputs(chunk: &mut Chunk, raw: &str) {
    let Some(tree) = Ordered::parse(raw) else {
        return;
    };
    let Some(calls) = chunk.message.tool_calls.as_mut() else {
        return;
    };
    for call in calls {
        let Some(index) = call.index.and_then(|index| usize::try_from(index).ok()) else {
            continue;
        };
        if let Some(input) = tree
            .get("content")
            .and_then(|content| content.at(index))
            .and_then(|block| block.get("input"))
        {
            call.arguments = Some(input.dumps(true));
        }
    }
}
