//! Projecting a saved transcript as ACP session updates.
//!
//! Persisted entries predate the structured public shape, so both encodings
//! are accepted: a stored entry may be a typed public entry or the older
//! role-and-content record.

use serde_json::{Value, json};
use vibe_app_server::client::PublicHistoryEntry;

use crate::protocol::AcpError;
use crate::updates::stream::{
    normalize_image_content, normalize_resource_content, public_entry_updates,
};
use crate::updates::{content_chunk, text_chunk};

/// Projects one persisted history entry for replay. Persisted entries predate
/// the structured public shape, so both encodings are accepted.
pub(crate) fn history_entry_updates(entry: &Value, index: usize) -> Result<Vec<Value>, AcpError> {
    if entry.get("type").and_then(Value::as_str).is_some() {
        let public = serde_json::from_value::<PublicHistoryEntry>(entry.clone())?;
        let message_id = public.metadata().id.clone();
        return public_entry_updates(None, &public, &message_id);
    }
    let role = entry
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidResponse("history entry omitted role".to_owned()))?;
    let base_id = format!("history:{index}:{role}");
    match role {
        "system" | "private" => Ok(Vec::new()),
        "user" => Ok(history_message_chunks(
            "user_message_chunk",
            &base_id,
            entry.get("content"),
        )),
        "assistant" => {
            let mut updates = Vec::new();
            if let Some(reasoning) = entry
                .get("reasoning")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                updates.push(text_chunk(
                    "agent_thought_chunk",
                    &format!("history:{index}:reasoning"),
                    reasoning,
                ));
            }
            updates.extend(history_message_chunks(
                "agent_message_chunk",
                &base_id,
                entry.get("content"),
            ));
            updates.extend(history_tool_calls(entry, &base_id));
            Ok(updates)
        }
        "tool" => {
            let call_id = entry
                .get("call_id")
                .or_else(|| entry.get("callId"))
                .and_then(Value::as_str)
                .unwrap_or(&base_id);
            let content = entry
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let is_error = entry
                .get("is_error")
                .or_else(|| entry.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "status": if is_error {"failed"} else {"completed"},
                "content": (!content.is_empty()).then(|| vec![json!({
                    "type": "content",
                    "content": {"type": "text", "text": content},
                })]),
                "rawOutput": content,
            })])
        }
        unsupported => Err(AcpError::InvalidResponse(format!(
            "unsupported history role `{unsupported}`"
        ))),
    }
}

fn history_tool_calls(entry: &Value, base_id: &str) -> Vec<Value> {
    entry
        .get("tool_calls")
        .or_else(|| entry.get("toolCalls"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tool_call| {
            let arguments = tool_call.get("arguments").cloned().unwrap_or(Value::Null);
            let raw_input = arguments.as_str().map_or(arguments.clone(), |arguments| {
                serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments))
            });
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call.get("id").and_then(Value::as_str).unwrap_or(base_id),
                "title": tool_call.get("name").and_then(Value::as_str).unwrap_or("Tool"),
                "kind": "other",
                "status": "pending",
                "rawInput": raw_input,
            })
        })
        .collect()
}

fn history_message_chunks(kind: &str, message_id: &str, content: Option<&Value>) -> Vec<Value> {
    normalized_history_content(content)
        .into_iter()
        .map(|content| content_chunk(kind, message_id, content))
        .collect()
}

fn normalized_history_content(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type": "text", "text": text})]
        }
        Some(Value::Array(blocks)) => blocks.iter().filter_map(normalize_history_block).collect(),
        Some(block @ Value::Object(_)) => normalize_history_block(block).into_iter().collect(),
        Some(Value::String(_) | Value::Null) | None => Vec::new(),
        Some(other) => vec![json!({"type": "text", "text": other.to_string()})],
    }
}

fn normalize_history_block(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| json!({"type": "text", "text": text})),
        Some("image") => block
            .get("attachment")
            .map_or_else(|| normalize_image_content(block), normalize_image_content),
        Some("resource") => block.get("resource").map_or_else(
            || normalize_resource_content(block),
            |resource| {
                if resource.get("uri").is_some() {
                    Some(json!({"type": "resource", "resource": resource}))
                } else {
                    normalize_resource_content(resource)
                }
            },
        ),
        Some("resource_link") => Some(block.clone()),
        _ => None,
    }
}
