//! The line a message is written to `messages.jsonl` as.
//!
//! Reference `SessionLogger` writes `LLMMessage.model_dump(exclude_none=True,
//! mode="json")` (`vibe/core/session/session_logger.py`), so a field the
//! message does not carry is left out, and the keys follow the order
//! `LLMMessage` declares them in (`vibe/core/types.py`). Writing that record
//! here is what lets this port and the reference read each other's sessions.
//!
//! Reading accepts the record this port wrote before it adopted the
//! reference's: tool answers keyed `call_id` with an `is_error` flag, flat tool
//! calls and `reasoning` for the reasoning text.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::events::{
    ManualShellRecord, ModelMessage, ModelToolCall, PersistedToolResult, PublicContentBlock,
};

/// Reference `TOOL_ERROR_TAG`, which marks the answer of a call that failed.
const TOOL_ERROR_OPEN: &str = "<tool_error>";
const TOOL_ERROR_CLOSE: &str = "</tool_error>";

#[derive(Serialize)]
struct FunctionRecord<'a> {
    name: &'a str,
    arguments: &'a str,
}

/// Reference `ToolCall`, declared `id`, `index`, `function`, `type`,
/// `presentation`.
#[derive(Serialize)]
struct ToolCallRecord<'a> {
    id: &'a str,
    index: usize,
    function: FunctionRecord<'a>,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation: Option<&'a Value>,
}

/// Reference `LLMMessage`, in its declaration order. `None` is a field the
/// message does not carry; `model_dump(exclude_none=True)` leaves it out.
#[derive(Serialize, Default)]
struct MessageRecord<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<&'a Value>>,
    injected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_payloads: Option<&'a [Map<String, Value>]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_message_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallRecord<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_result: Option<&'a PersistedToolResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_display_content: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_shell: Option<&'a ManualShellRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_boundary: Option<&'static str>,
}

fn record(message: &ModelMessage) -> MessageRecord<'_> {
    match message {
        ModelMessage::System { content } => MessageRecord {
            role: "system",
            content: Some(content),
            ..MessageRecord::default()
        },
        ModelMessage::User {
            content,
            injected,
            message_id,
            attachments,
            user_display_content,
            input_text,
            manual_shell,
            compaction_boundary,
        } => {
            let images = attachments
                .iter()
                .filter_map(|block| match block {
                    PublicContentBlock::Image { attachment } => Some(attachment),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let resources = attachments
                .iter()
                .filter_map(|block| match block {
                    // `exclude_none` reaches into the resource too.
                    PublicContentBlock::Resource { resource } => Some(without_nulls(resource)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            MessageRecord {
                role: "user",
                content: Some(content),
                images: (!images.is_empty()).then_some(images),
                injected: *injected,
                message_id: message_id.as_deref(),
                user_display_content: user_display_content.as_ref(),
                input_text: input_text.as_deref(),
                resources: (!resources.is_empty()).then_some(resources),
                manual_shell: manual_shell.as_deref(),
                context_boundary: compaction_boundary.then_some("compaction"),
                ..MessageRecord::default()
            }
        }
        ModelMessage::Assistant {
            message_id,
            reasoning_message_id,
            content,
            reasoning,
            reasoning_payloads,
            tool_calls,
            keeps_empty_content,
        } => MessageRecord {
            role: "assistant",
            content: (!content.is_empty() || *keeps_empty_content).then_some(content.as_str()),
            reasoning_content: reasoning.as_deref(),
            reasoning_payloads: (!reasoning_payloads.is_empty())
                .then_some(reasoning_payloads.as_slice()),
            reasoning_message_id: reasoning_message_id.as_deref(),
            tool_calls: (!tool_calls.is_empty()).then(|| {
                tool_calls
                    .iter()
                    .enumerate()
                    .map(|(index, call)| ToolCallRecord {
                        id: &call.id,
                        index,
                        function: FunctionRecord {
                            name: &call.name,
                            arguments: &call.arguments,
                        },
                        kind: "function",
                        presentation: call.presentation.as_ref(),
                    })
                    .collect()
            }),
            message_id: message_id.as_deref(),
            ..MessageRecord::default()
        },
        ModelMessage::Tool {
            call_id,
            content,
            name,
            result,
            ..
        } => MessageRecord {
            role: "tool",
            content: Some(content),
            name: Some(name),
            tool_call_id: Some(call_id),
            tool_result: result.as_deref(),
            ..MessageRecord::default()
        },
    }
}

/// `value` without the object members that are `null`, at every depth.
fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        other => other.clone(),
    }
}

/// The record `message` is written as, as a JSON value.
pub(super) fn to_value(message: &ModelMessage) -> Result<Value, serde_json::Error> {
    serde_json::to_value(record(message))
}

/// The line `message` is written as, keys in the reference's order.
pub(super) fn to_line(message: &ModelMessage) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&record(message))
}

/// Reference `meta.json` `system_prompt`: the system message dumped whole,
/// every field it does not carry written as its default.
pub(super) fn system_prompt(message: &ModelMessage) -> Value {
    serde_json::json!({
        "role": "system",
        "content": message.content(),
        "images": null,
        "injected": false,
        "reasoning_content": null,
        "reasoning_payloads": null,
        "reasoning_message_id": null,
        "tool_calls": null,
        "name": null,
        "tool_call_id": null,
        "tool_result": null,
        "message_id": null,
        "user_display_content": null,
        "input_text": null,
        "resources": null,
        "manual_shell": null,
        "context_boundary": null,
    })
}

/// Reference `_content_before`: a list of parts reads as its texts joined by
/// newlines, and `None` as empty.
fn content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("text") {
                Some(Value::String(text)) => text.clone(),
                _ => part.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn tool_call(value: &Value) -> Option<ModelToolCall> {
    let object = value.as_object()?;
    let function = object.get("function").and_then(Value::as_object);
    // This port's earlier record kept the name and arguments flat.
    let field = |key: &str| {
        function
            .and_then(|function| function.get(key))
            .or_else(|| object.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Some(ModelToolCall {
        id: string(object, "id").unwrap_or_default(),
        name: field("name").unwrap_or_default(),
        arguments: field("arguments").unwrap_or_default(),
        presentation: object
            .get("presentation")
            .filter(|value| !value.is_null())
            .cloned(),
    })
}

/// Whether a tool answer is a failure, read as reference `TaggedText` reads
/// it: the whole answer wrapped in the tool error tag.
fn is_tool_error(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.starts_with(TOOL_ERROR_OPEN) && trimmed.ends_with(TOOL_ERROR_CLOSE)
}

/// Reads one line of `messages.jsonl`, in the reference's record or this
/// port's earlier one.
pub(super) fn from_line(line: &str) -> Result<ModelMessage, String> {
    let value: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "a message record is a JSON object".to_owned())?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    let message = match role {
        "system" => ModelMessage::System {
            content: content(object.get("content")),
        },
        "user" => {
            let mut attachments = Vec::new();
            for image in object
                .get("images")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                attachments.push(PublicContentBlock::Image {
                    attachment: image.clone(),
                });
            }
            for resource in object
                .get("resources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                attachments.push(PublicContentBlock::Resource {
                    resource: crate::events::canonical_user_resource(resource),
                });
            }
            // This port's earlier record kept both kinds as content blocks.
            if let Some(blocks) = object.get("attachments") {
                attachments.extend(
                    serde_json::from_value::<Vec<PublicContentBlock>>(blocks.clone())
                        .map_err(|error| error.to_string())?,
                );
            }
            ModelMessage::User {
                content: content(object.get("content")),
                injected: object
                    .get("injected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                message_id: string(object, "message_id"),
                attachments,
                user_display_content: object
                    .get("user_display_content")
                    .filter(|value| !value.is_null())
                    .cloned(),
                input_text: string(object, "input_text"),
                manual_shell: object
                    .get("manual_shell")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()
                    .map_err(|error| error.to_string())?
                    .map(Box::new),
                compaction_boundary: object.get("context_boundary").and_then(Value::as_str)
                    == Some("compaction"),
            }
        }
        "tool" => {
            let text = content(object.get("content"));
            ModelMessage::Tool {
                call_id: string(object, "tool_call_id")
                    .or_else(|| string(object, "call_id"))
                    .unwrap_or_default(),
                is_error: object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| is_tool_error(&text)),
                content: text,
                name: string(object, "name").unwrap_or_default(),
                result: object
                    .get("tool_result")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()
                    .map_err(|error| error.to_string())?
                    .map(Box::new),
            }
        }
        _ => {
            let text = object.get("content");
            ModelMessage::Assistant {
                message_id: string(object, "message_id"),
                reasoning_message_id: string(object, "reasoning_message_id"),
                keeps_empty_content: matches!(text, Some(Value::String(text)) if text.is_empty()),
                content: content(text),
                reasoning: string(object, "reasoning_content")
                    .or_else(|| string(object, "reasoning")),
                reasoning_payloads: object
                    .get("reasoning_payloads")
                    .and_then(Value::as_array)
                    .map(|payloads| {
                        payloads
                            .iter()
                            .filter_map(|payload| payload.as_object().cloned())
                            .collect()
                    })
                    .unwrap_or_default(),
                tool_calls: object
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|calls| calls.iter().filter_map(tool_call).collect())
                    .unwrap_or_default(),
            }
        }
    };
    Ok(message)
}
