//! The request the Mistral client sends, as its request models serialize it.
//!
//! Nullable fields go out as `null`, an assistant turn always carries
//! `prefix` and `tool_calls`, reasoning travels as a `thinking` chunk only
//! when thinking is on, and the effort collapses to `none` or `high`. A
//! Unified Harness call goes through the same client from the runtime's own
//! adapter, which sends only the fields it sets.

use serde_json::{Map, Value, json};

use crate::llm::ModelRequest;
use crate::llm::adapter::{chat_image, image_parts, thinking_content};
use crate::llm::types::{ContentPart, Message, Role, Tool, ToolChoice};
use crate::provider::config::ThinkingLevel;

/// The reasoning effort the Mistral API takes for a thinking level.
const fn effort(thinking: ThinkingLevel) -> Option<&'static str> {
    match thinking {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("none"),
        ThinkingLevel::Medium | ThinkingLevel::High | ThinkingLevel::Max => Some("high"),
    }
}

/// The request body as the client's request model serializes it.
pub(super) fn body(request: &ModelRequest<'_>, streaming: bool) -> Value {
    if request.unified {
        return unified_body(request, streaming);
    }
    let include_reasoning = request.model.thinking != ThinkingLevel::Off;
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.model.name));
    body.insert("temperature".to_owned(), json!(request.temperature));
    body.insert("max_tokens".to_owned(), json!(request.max_tokens));
    body.insert("stream".to_owned(), Value::Bool(streaming));
    body.insert(
        "messages".to_owned(),
        Value::Array(
            request
                .messages
                .iter()
                .map(|message| mistral_message(message, include_reasoning))
                .collect(),
        ),
    );
    body.insert(
        "tools".to_owned(),
        request
            .tools
            .filter(|tools| !tools.is_empty())
            .map_or(Value::Null, |tools| {
                Value::Array(tools.iter().map(Tool::chat_declaration).collect())
            }),
    );
    if let Some(choice) = request.tool_choice {
        body.insert(
            "tool_choice".to_owned(),
            match choice {
                ToolChoice::Tool(tool) => {
                    json!({"type": "function", "function": {"name": tool.name}})
                }
                keyword => json!(keyword.keyword()),
            },
        );
    }
    body.insert(
        "metadata".to_owned(),
        request
            .metadata
            .map_or(Value::Null, |metadata| Value::Object(metadata.clone())),
    );
    body.insert(
        "reasoning_effort".to_owned(),
        json!(effort(request.model.thinking)),
    );
    Value::Object(body)
}

/// The body the Unified Harness runtime's adapter hands the same client
/// (`vibe/adapters/mistral.py` under `harness/runtimes/python/python/mistralai_vibe_local_harness/`):
/// only the fields it sets reach the wire, `parallel_tool_calls` rides with
/// the tools, and the metadata goes out only when there is some.
fn unified_body(request: &ModelRequest<'_>, streaming: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.model.name));
    body.insert(
        "messages".to_owned(),
        Value::Array(request.messages.iter().map(unified_message).collect()),
    );
    body.insert("temperature".to_owned(), json!(request.temperature));
    if let Some(effort) = effort(request.model.thinking) {
        body.insert("reasoning_effort".to_owned(), json!(effort));
    }
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    if let Some(metadata) = request.metadata.filter(|metadata| !metadata.is_empty()) {
        body.insert("metadata".to_owned(), Value::Object(metadata.clone()));
    }
    if let Some(tools) = request.tools.filter(|tools| !tools.is_empty()) {
        body.insert(
            "tools".to_owned(),
            Value::Array(tools.iter().map(Tool::chat_declaration).collect()),
        );
        body.insert("parallel_tool_calls".to_owned(), Value::Bool(true));
    }
    body.insert("stream".to_owned(), Value::Bool(streaming));
    Value::Object(body)
}

/// One message as the unified adapter writes it and the client's models
/// then serialize it: an assistant turn gains `prefix`, a tool call its
/// `index`.
fn unified_message(message: &Message) -> Value {
    let content = |message: &Message| -> Value {
        if !message.parts.is_empty() {
            return Value::Array(
                message
                    .parts
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text(text) => json!({"type": "text", "text": text}),
                        ContentPart::Image(picture) => chat_image(picture),
                    })
                    .collect(),
            );
        }
        if message.images.is_empty() {
            return json!(message.text());
        }
        Value::Array(image_parts(message, "text", chat_image))
    };
    match message.role {
        Role::System => json!({"role": "system", "content": content(message)}),
        Role::User => json!({"role": "user", "content": content(message)}),
        Role::Assistant => {
            let reasoning = message
                .reasoning_content
                .as_deref()
                .filter(|reasoning| !reasoning.is_empty());
            let mut payload = Map::new();
            payload.insert("role".to_owned(), json!("assistant"));
            let body = match reasoning {
                Some(reasoning) => thinking_content(reasoning, message.text()),
                None if message.text().is_empty() => Value::Null,
                None => json!(message.text()),
            };
            payload.insert("content".to_owned(), body);
            let calls: Vec<Value> = message
                .tool_calls
                .iter()
                .flatten()
                .map(|call| {
                    json!({
                        "id": call.id.as_deref().unwrap_or_default(),
                        "type": "function",
                        "function": {
                            "name": call.name.as_deref().unwrap_or_default(),
                            "arguments": call.arguments.as_deref().unwrap_or_default(),
                        },
                        "index": 0,
                    })
                })
                .collect();
            if !calls.is_empty() {
                payload.insert("tool_calls".to_owned(), Value::Array(calls));
            }
            payload.insert("prefix".to_owned(), Value::Bool(false));
            Value::Object(payload)
        }
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id,
            "name": message.name,
            "content": message.content,
        }),
    }
}

fn mistral_message(message: &Message, include_reasoning: bool) -> Value {
    match message.role {
        Role::System => json!({"role": "system", "content": message.text()}),
        Role::User if !message.images.is_empty() => {
            json!({"role": "user", "content": image_parts(message, "text", chat_image)})
        }
        Role::User => json!({"role": "user", "content": message.content}),
        Role::Assistant => {
            let reasoning = message
                .reasoning_content
                .as_deref()
                .filter(|reasoning| include_reasoning && !reasoning.is_empty());
            let content = match reasoning {
                Some(reasoning) => thinking_content(reasoning, message.text()),
                None => json!(message.text()),
            };
            let calls: Vec<Value> = message
                .tool_calls
                .iter()
                .flatten()
                .map(|call| {
                    let mut converted = Map::new();
                    converted.insert(
                        "function".to_owned(),
                        json!({
                            "name": call.name.as_deref().unwrap_or_default(),
                            "arguments": call.arguments.as_deref().unwrap_or_default(),
                        }),
                    );
                    if let Some(id) = &call.id {
                        converted.insert("id".to_owned(), json!(id));
                    }
                    converted.insert("type".to_owned(), json!("function"));
                    if let Some(index) = call.index {
                        converted.insert("index".to_owned(), json!(index));
                    }
                    Value::Object(converted)
                })
                .collect();
            json!({
                "role": "assistant",
                "content": content,
                "tool_calls": calls,
                "prefix": false,
            })
        }
        Role::Tool => json!({
            "role": "tool",
            "content": message.content,
            "tool_call_id": message.tool_call_id,
            "name": message.name,
        }),
    }
}
