//! The two chat-completions dialects.
//!
//! `openai` (reference `OpenAIAdapter`, `vibe/core/llm/backend/generic.py`)
//! sends each message as the message model dumps it, with the reasoning under
//! the provider's `reasoning_field_name`, and reads a delta back through the
//! same model. `reasoning` (reference `ReasoningAdapter`,
//! `vibe/core/llm/backend/reasoning_adapter.py`) sends an assistant's
//! reasoning as a typed `thinking` block and reads such blocks back.

use serde_json::{Map, Value, json};

use super::adapter::{
    PreparedRequest, RequestParts, bearer_headers, chat_finish_reason, chat_payload, chat_usage,
    finalize_chat, optional_index, optional_string, python_str, truthy, validate_message,
};
use super::error::{LocalFailure, LocalKind};
use super::types::{Chunk, Message, Role, StopInfo, ToolCall};
use crate::provider::config::ProviderConfig;

const ENDPOINT: &str = "/chat/completions";

/// The parts a user message with images is sent as.
fn image_parts(message: &Message, text_kind: &str, image: impl Fn(String) -> Value) -> Vec<Value> {
    let mut parts = Vec::new();
    if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
        parts.push(json!({"type": text_kind, "text": text}));
    }
    parts.extend(
        message
            .images
            .iter()
            .map(|picture| image(picture.data_uri())),
    );
    parts
}

fn chat_image(url: String) -> Value {
    json!({"type": "image_url", "image_url": {"url": url}})
}

/// A message as the message model dumps it, without the fields that never
/// reach a provider.
fn openai_message(message: &Message, reasoning_field: &str) -> Value {
    let mut dumped = Map::new();
    dumped.insert("role".to_owned(), json!(message.role.as_str()));
    if let Some(content) = &message.content {
        dumped.insert("content".to_owned(), json!(content));
    }
    if let Some(reasoning) = &message.reasoning_content {
        dumped.insert(reasoning_field.to_owned(), json!(reasoning));
    }
    if let Some(calls) = &message.tool_calls {
        let calls = calls
            .iter()
            .map(|call| {
                let mut dumped = Map::new();
                if let Some(id) = &call.id {
                    dumped.insert("id".to_owned(), json!(id));
                }
                if let Some(index) = call.index {
                    dumped.insert("index".to_owned(), json!(index));
                }
                let mut function = Map::new();
                if let Some(name) = &call.name {
                    function.insert("name".to_owned(), json!(name));
                }
                if let Some(arguments) = &call.arguments {
                    function.insert("arguments".to_owned(), json!(arguments));
                }
                dumped.insert("function".to_owned(), Value::Object(function));
                dumped.insert("type".to_owned(), json!("function"));
                Value::Object(dumped)
            })
            .collect();
        dumped.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    if let Some(name) = &message.name {
        dumped.insert("name".to_owned(), json!(name));
    }
    if let Some(id) = &message.tool_call_id {
        dumped.insert("tool_call_id".to_owned(), json!(id));
    }
    if message.role == Role::User && !message.images.is_empty() {
        let text = message.content.as_deref().unwrap_or_default();
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(json!({"type": "text", "text": text}));
        }
        parts.extend(
            message
                .images
                .iter()
                .map(|picture| chat_image(picture.data_uri())),
        );
        dumped.insert("content".to_owned(), Value::Array(parts));
    }
    Value::Object(dumped)
}

pub fn prepare_openai(parts: &RequestParts<'_>) -> PreparedRequest {
    let field = parts.provider.reasoning_field_name.as_str();
    let messages = parts
        .messages
        .iter()
        .map(|message| openai_message(message, field))
        .collect();
    let mut payload = chat_payload(parts, messages);
    let mut options = Map::new();
    options.insert("include_usage".to_owned(), Value::Bool(true));
    if parts.provider.name == "mistral" {
        options.insert("stream_tool_calls".to_owned(), Value::Bool(true));
    }
    finalize_chat(&mut payload, parts.streaming, Value::Object(options));
    PreparedRequest {
        path: ENDPOINT.to_owned(),
        headers: bearer_headers(parts.api_key),
        body: Value::Object(payload),
        base_url: None,
    }
}

fn reasoning_message(message: &Message) -> Value {
    match message.role {
        Role::System => json!({"role": "system", "content": message.text()}),
        Role::User if !message.images.is_empty() => json!({
            "role": "user",
            "content": image_parts(message, "text", chat_image),
        }),
        Role::User => json!({"role": "user", "content": message.text()}),
        Role::Assistant => {
            let mut converted = Map::new();
            converted.insert("role".to_owned(), json!("assistant"));
            let content = match message
                .reasoning_content
                .as_deref()
                .filter(|reasoning| !reasoning.is_empty())
            {
                Some(reasoning) => {
                    let mut blocks = vec![json!({
                        "type": "thinking",
                        "thinking": [{"type": "text", "text": reasoning}],
                    })];
                    if !message.text().is_empty() {
                        blocks.push(json!({"type": "text", "text": message.text()}));
                    }
                    Value::Array(blocks)
                }
                None => json!(message.text()),
            };
            converted.insert("content".to_owned(), content);
            if let Some(calls) = message
                .tool_calls
                .as_ref()
                .filter(|calls| !calls.is_empty())
            {
                let calls = calls
                    .iter()
                    .map(|call| {
                        let mut converted = Map::new();
                        converted.insert("id".to_owned(), json!(call.id));
                        converted.insert("type".to_owned(), json!("function"));
                        converted.insert(
                            "function".to_owned(),
                            json!({
                                "name": call.name.as_deref().unwrap_or_default(),
                                "arguments": call.arguments.as_deref().unwrap_or_default(),
                            }),
                        );
                        if let Some(index) = call.index {
                            converted.insert("index".to_owned(), json!(index));
                        }
                        Value::Object(converted)
                    })
                    .collect();
                converted.insert("tool_calls".to_owned(), Value::Array(calls));
            }
            Value::Object(converted)
        }
        Role::Tool => {
            let mut converted = Map::new();
            converted.insert("role".to_owned(), json!("tool"));
            converted.insert("content".to_owned(), json!(message.text()));
            converted.insert("tool_call_id".to_owned(), json!(message.tool_call_id));
            if let Some(name) = message.name.as_deref().filter(|name| !name.is_empty()) {
                converted.insert("name".to_owned(), json!(name));
            }
            Value::Object(converted)
        }
    }
}

pub fn prepare_reasoning(parts: &RequestParts<'_>) -> PreparedRequest {
    let messages = parts.messages.iter().map(reasoning_message).collect();
    let mut payload = chat_payload(parts, messages);
    finalize_chat(
        &mut payload,
        parts.streaming,
        json!({"include_usage": true, "stream_tool_calls": true}),
    );
    PreparedRequest {
        path: ENDPOINT.to_owned(),
        headers: bearer_headers(parts.api_key),
        body: Value::Object(payload),
        base_url: None,
    }
}

/// Moves a custom reasoning field back under `reasoning_content`.
fn reasoning_from_api(object: &Map<String, Value>, field: &str) -> Map<String, Value> {
    let mut renamed = object.clone();
    if field != "reasoning_content"
        && let Some(value) = renamed.remove(field)
    {
        renamed.insert("reasoning_content".to_owned(), value);
    }
    renamed
}

fn object_of(value: &Value, what: &str) -> Result<Map<String, Value>, LocalFailure> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| LocalFailure::new(LocalKind::Validation, format!("{what} is not an object")))
}

fn openai_message_of(
    data: &Map<String, Value>,
    field: &str,
) -> Result<Option<Message>, LocalFailure> {
    let as_delta = |value: &Value| -> Result<Message, LocalFailure> {
        let mut delta = reasoning_from_api(&object_of(value, "a delta")?, field);
        if delta.get("role").is_none_or(Value::is_null) {
            delta.insert("role".to_owned(), json!("assistant"));
        }
        validate_message(&delta)
    };
    let as_message = |value: &Value| -> Result<Message, LocalFailure> {
        validate_message(&reasoning_from_api(&object_of(value, "a message")?, field))
    };
    if let Some(Value::Array(choices)) = data.get("choices")
        && let Some(first) = choices.first()
    {
        let choice = first.as_object().ok_or_else(|| {
            LocalFailure::new(LocalKind::Attribute, "a choice is not a JSON object")
        })?;
        if let Some(message) = choice.get("message") {
            return as_message(message).map(Some);
        }
        if let Some(delta) = choice.get("delta") {
            return as_delta(delta).map(Some);
        }
        return Err(LocalFailure::new(
            LocalKind::Value,
            "a choice carries neither a message nor a delta",
        ));
    }
    if let Some(choices) = data.get("choices")
        && truthy(choices)
    {
        return Err(LocalFailure::new(LocalKind::Key, "`choices` is not a list"));
    }
    if let Some(message) = data.get("message") {
        return as_message(message).map(Some);
    }
    if let Some(delta) = data.get("delta") {
        return as_delta(delta).map(Some);
    }
    Ok(None)
}

/// One event or body of the `openai` dialect.
///
/// # Errors
///
/// A choice with neither message nor delta, and fields of the wrong type.
pub fn parse_openai(
    data: &Map<String, Value>,
    provider: &ProviderConfig,
) -> Result<Chunk, LocalFailure> {
    let message = openai_message_of(data, &provider.reasoning_field_name)?
        .unwrap_or_else(|| Message::assistant().with_content(""));
    Ok(Chunk {
        message,
        usage: Some(chat_usage(data)?),
        correlation_id: None,
        stop: chat_finish_reason(data)?.map(StopInfo::reason),
    })
}

fn get_or_attribute<'a>(
    value: &'a Value,
    what: &str,
) -> Result<&'a Map<String, Value>, LocalFailure> {
    value
        .as_object()
        .ok_or_else(|| LocalFailure::new(LocalKind::Attribute, format!("{what} is not an object")))
}

/// Reads the text and thinking blocks of a content value.
fn content_blocks(content: &Value) -> Result<(Option<String>, Option<String>), LocalFailure> {
    let blocks = match content {
        Value::String(text) => return Ok(((!text.is_empty()).then(|| text.clone()), None)),
        Value::Array(blocks) => blocks,
        _ => {
            return Err(LocalFailure::new(
                LocalKind::Type,
                "message content is neither text nor a list of blocks",
            ));
        }
    };
    let mut text = String::new();
    let mut thinking = String::new();
    for block in blocks {
        let block = get_or_attribute(block, "a content block")?;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(&text_of(block.get("text"))?),
            Some("thinking") => {
                let inner = match block.get("thinking") {
                    None => continue,
                    Some(Value::Array(inner)) => inner,
                    Some(Value::String(inner)) => {
                        // Iterating a string walks its characters, each one
                        // a string of its own.
                        thinking.push_str(inner);
                        continue;
                    }
                    Some(_) => {
                        return Err(LocalFailure::new(
                            LocalKind::Type,
                            "a thinking block is not a list",
                        ));
                    }
                };
                for part in inner {
                    match part {
                        Value::Object(part)
                            if part.get("type").and_then(Value::as_str) == Some("text") =>
                        {
                            thinking.push_str(&text_of(part.get("text"))?);
                        }
                        Value::String(part) => thinking.push_str(part),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok((
        (!text.is_empty()).then_some(text),
        (!thinking.is_empty()).then_some(thinking),
    ))
}

fn text_of(value: Option<&Value>) -> Result<String, LocalFailure> {
    match value {
        None => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(LocalFailure::new(
            LocalKind::Type,
            "a block's text is not a string",
        )),
    }
}

fn reasoning_tool_calls(value: Option<&Value>) -> Result<Option<Vec<ToolCall>>, LocalFailure> {
    let calls = match value {
        Some(value) if truthy(value) => value,
        _ => return Ok(None),
    };
    let Value::Array(calls) = calls else {
        return Err(LocalFailure::new(
            LocalKind::Attribute,
            "`tool_calls` is not a list",
        ));
    };
    calls
        .iter()
        .map(|call| {
            let call = get_or_attribute(call, "a tool call")?;
            let function = match call.get("function") {
                None => None,
                Some(function) => Some(get_or_attribute(function, "a tool call's function")?),
            };
            let name = function
                .and_then(|function| function.get("name"))
                .map(|name| match name {
                    Value::Null => Ok(None),
                    Value::String(name) => Ok(Some(name.clone())),
                    _ => Err(LocalFailure::new(
                        LocalKind::Validation,
                        "a tool name is not a string",
                    )),
                })
                .transpose()?
                .flatten();
            let arguments = match function.and_then(|function| function.get("arguments")) {
                None => Some(String::new()),
                Some(Value::Null) => None,
                Some(Value::String(arguments)) => Some(arguments.clone()),
                Some(_) => {
                    return Err(LocalFailure::new(
                        LocalKind::Validation,
                        "tool arguments are not a string",
                    ));
                }
            };
            Ok(ToolCall {
                id: optional_string(call, "id")?,
                index: optional_index(call.get("index"))?,
                name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn reasoning_message_of(object: &Map<String, Value>) -> Result<Message, LocalFailure> {
    let (content, reasoning) = match object.get("content") {
        None | Some(Value::Null) => (None, None),
        Some(content) => content_blocks(content)?,
    };
    Ok(Message {
        content,
        reasoning_content: reasoning,
        tool_calls: reasoning_tool_calls(object.get("tool_calls"))?,
        ..Message::assistant()
    })
}

/// One event or body of the `reasoning` dialect.
///
/// # Errors
///
/// Blocks and tool calls of the wrong shape.
pub fn parse_reasoning(data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
    let mut message = None;
    if let Some(Value::Array(choices)) = data.get("choices")
        && let Some(first) = choices.first()
    {
        let choice = get_or_attribute(first, "a choice")?;
        if let Some(inner) = choice.get("message") {
            message = Some(reasoning_message_of(get_or_attribute(inner, "a message")?)?);
        } else if let Some(inner) = choice.get("delta") {
            message = Some(reasoning_message_of(get_or_attribute(inner, "a delta")?)?);
        }
    }
    Ok(Chunk {
        message: message.unwrap_or_else(|| Message::assistant().with_content("")),
        usage: Some(chat_usage(data)?),
        correlation_id: None,
        stop: chat_finish_reason(data)?.map(StopInfo::reason),
    })
}

/// `str()` of a finish reason, which may be any JSON value.
#[must_use]
pub fn finish_reason_text(value: &Value) -> String {
    python_str(value)
}
