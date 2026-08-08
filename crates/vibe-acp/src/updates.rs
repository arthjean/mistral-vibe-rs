//! Translation between canonical app-server entries and ACP session updates.

use std::collections::BTreeMap;

use base64::Engine;
use serde_json::{Value, json};
use vibe_app_server::client::{
    ProgrammaticTurn, ProgrammaticUpdate, PublicCallbackState, PublicContentBlock,
    PublicEffectState, PublicHistoryEntry, PublicMessageRole, PublicTurnStopReason, TurnRequest,
};

use crate::protocol::{AcpError, AcpSessionUpdate};

pub const MAX_ACP_UPDATE_QUEUE: usize = 1_024;
const MAX_EMBEDDED_CONTENT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PROMPT_BLOCKS: usize = 256;

pub(crate) type UpdateSender = tokio::sync::mpsc::Sender<AcpSessionUpdate>;

pub(crate) fn queue_update(
    sender: &UpdateSender,
    session_id: &str,
    update: Value,
) -> Result<(), AcpError> {
    sender
        .try_send(AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update,
        })
        .map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => AcpError::Backpressure,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => AcpError::Disconnected,
        })
}

pub(crate) fn text_chunk(kind: &str, message_id: &str, text: &str) -> Value {
    content_chunk(kind, message_id, json!({"type": "text", "text": text}))
}

pub(crate) fn content_chunk(kind: &str, message_id: &str, content: Value) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": content,
        "messageId": message_id,
    })
}

/// A canonical entry is announced once and revised afterward.
const fn tool_call_kind(revision: bool) -> &'static str {
    if revision {
        "tool_call_update"
    } else {
        "tool_call"
    }
}

/// Tracks the entries already streamed in a turn so each canonical revision is
/// projected as a delta, and rewrites the first user entry to the ACP-visible
/// message ID that `session/fork` can anchor on.
#[derive(Default)]
pub(crate) struct AcpUpdateProjection {
    entries: BTreeMap<String, PublicHistoryEntry>,
    message_ids: BTreeMap<String, String>,
    next_user_message_id: Option<String>,
}

impl AcpUpdateProjection {
    pub(crate) fn with_user_message_id(message_id: String) -> Self {
        Self {
            next_user_message_id: Some(message_id),
            ..Self::default()
        }
    }

    fn message_id(&mut self, entry: &PublicHistoryEntry) -> String {
        let entry_id = &entry.metadata().id;
        if let Some(message_id) = self.message_ids.get(entry_id) {
            return message_id.clone();
        }
        let message_id = if matches!(
            entry,
            PublicHistoryEntry::Message {
                role: PublicMessageRole::User,
                ..
            }
        ) {
            self.next_user_message_id
                .take()
                .unwrap_or_else(|| entry_id.clone())
        } else {
            entry_id.clone()
        };
        self.message_ids
            .insert(entry_id.clone(), message_id.clone());
        message_id
    }
}

pub(crate) fn send_acp_updates(
    session_id: &str,
    update: ProgrammaticUpdate,
    projection: &mut AcpUpdateProjection,
    sender: &UpdateSender,
) -> Result<(), AcpError> {
    let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
        return Ok(());
    };
    let entry_id = entry.metadata().id.clone();
    let message_id = projection.message_id(entry.as_ref());
    let previous = projection.entries.insert(entry_id, (*entry).clone());
    for mut update in public_entry_updates(previous.as_ref(), entry.as_ref())? {
        if message_id != entry.metadata().id
            && update.get("messageId").and_then(Value::as_str) == Some(entry.metadata().id.as_str())
        {
            update["messageId"] = json!(message_id);
        }
        queue_update(sender, session_id, update)?;
    }
    Ok(())
}

pub(crate) fn public_entry_updates(
    previous: Option<&PublicHistoryEntry>,
    entry: &PublicHistoryEntry,
) -> Result<Vec<Value>, AcpError> {
    let entry_id = &entry.metadata().id;
    match entry {
        PublicHistoryEntry::Message { role, content, .. } => {
            let kind = match role {
                PublicMessageRole::User => "user_message_chunk",
                PublicMessageRole::Assistant => "agent_message_chunk",
                PublicMessageRole::System => return Ok(Vec::new()),
            };
            let Some(previous) = previous else {
                return Ok(content
                    .iter()
                    .filter_map(acp_public_content)
                    .map(|content| content_chunk(kind, entry_id, content))
                    .collect());
            };
            let PublicHistoryEntry::Message {
                role: previous_role,
                content: previous_content,
                ..
            } = previous
            else {
                return Err(changed_type(entry_id));
            };
            if previous_role != role {
                return Err(AcpError::InvalidResponse(format!(
                    "public message `{entry_id}` changed role"
                )));
            }
            let delta = cumulative_text_delta(
                &public_text(previous_content),
                &public_text(content),
                entry_id,
            )?;
            Ok(delta
                .filter(|text| !text.is_empty())
                .map(|text| vec![text_chunk(kind, entry_id, &text)])
                .unwrap_or_default())
        }
        PublicHistoryEntry::Reasoning { text, .. } => {
            let delta = match previous {
                None => Some(text.clone()),
                Some(PublicHistoryEntry::Reasoning {
                    text: previous_text,
                    ..
                }) => cumulative_text_delta(previous_text, text, entry_id)?,
                Some(_) => return Err(changed_type(entry_id)),
            };
            Ok(delta
                .filter(|text| !text.is_empty())
                .map(|text| vec![text_chunk("agent_thought_chunk", entry_id, &text)])
                .unwrap_or_default())
        }
        PublicHistoryEntry::Effect {
            title,
            state,
            detail,
            ..
        } => {
            let previous_output = match previous {
                None => "",
                Some(PublicHistoryEntry::Effect { state, .. }) => effect_output_text(state),
                Some(_) => return Err(changed_type(entry_id)),
            };
            let output_delta =
                cumulative_text_delta(previous_output, effect_output_text(state), entry_id)?;
            let mut update = json!({
                "sessionUpdate": tool_call_kind(previous.is_some()),
                "toolCallId": entry_id,
                "title": title,
                "kind": "other",
                "status": acp_effect_status(state),
                "rawInput": detail,
                "rawOutput": effect_raw_output(state),
            });
            if let Some(delta) = output_delta.filter(|delta| !delta.is_empty()) {
                update["content"] = json!([{
                    "type": "content",
                    "content": {"type": "text", "text": delta},
                }]);
            }
            Ok(vec![update])
        }
        PublicHistoryEntry::Callback {
            callback_id,
            title,
            state,
            detail,
            ..
        } => Ok(vec![json!({
            "sessionUpdate": tool_call_kind(previous.is_some()),
            "toolCallId": callback_id,
            "title": title,
            "kind": "other",
            "status": acp_callback_status(state),
            "rawInput": detail,
        })]),
        PublicHistoryEntry::Notice {
            message, detail, ..
        } => Ok(previous
            .is_none_or(|previous| previous != entry)
            .then(|| {
                json!({
                    "sessionUpdate": "session_info_update",
                    "title": message,
                    "_meta": {"detail": detail, "entryId": entry_id},
                })
            })
            .into_iter()
            .collect()),
        PublicHistoryEntry::Checkpoint {
            kind,
            message,
            details,
            ..
        } => {
            let encoded = serde_json::to_value(entry)?;
            let completed =
                encoded.get("generationStatus").and_then(Value::as_str) == Some("completed");
            Ok(vec![json!({
                "sessionUpdate": tool_call_kind(previous.is_some()),
                "toolCallId": entry_id,
                "title": message.as_deref().unwrap_or("Checkpoint"),
                "kind": "think",
                "status": if completed {"completed"} else {"in_progress"},
                "rawInput": details,
                "_meta": {"checkpointKind": kind},
            })])
        }
    }
}

fn changed_type(entry_id: &str) -> AcpError {
    AcpError::InvalidResponse(format!("public entry `{entry_id}` changed type"))
}

fn public_text(content: &[PublicContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            PublicContentBlock::Text { text } => Some(text.as_str()),
            PublicContentBlock::Image { .. } | PublicContentBlock::Resource { .. } => None,
        })
        .collect()
}

fn cumulative_text_delta(
    previous: &str,
    current: &str,
    entry_id: &str,
) -> Result<Option<String>, AcpError> {
    if previous == current {
        return Ok(None);
    }
    current
        .strip_prefix(previous)
        .map(|delta| Some(delta.to_owned()))
        .ok_or_else(|| {
            AcpError::InvalidResponse(format!(
                "public entry `{entry_id}` replaced previously streamed text"
            ))
        })
}

fn acp_public_content(block: &PublicContentBlock) -> Option<Value> {
    match block {
        PublicContentBlock::Text { text } => {
            (!text.is_empty()).then(|| json!({"type": "text", "text": text}))
        }
        PublicContentBlock::Image { attachment } => normalize_image_content(attachment),
        PublicContentBlock::Resource { resource } => normalize_resource_content(resource),
    }
}

fn normalize_image_content(attachment: &Value) -> Option<Value> {
    if attachment.get("type").and_then(Value::as_str) == Some("image")
        && attachment.get("data").is_some_and(Value::is_string)
    {
        return Some(attachment.clone());
    }
    let data = attachment
        .get("data")
        .or_else(|| attachment.pointer("/source/data"))
        .and_then(Value::as_str)?;
    let mime_type = attachment
        .get("mimeType")
        .or_else(|| attachment.get("mediaType"))
        .and_then(Value::as_str)?;
    Some(json!({"type": "image", "data": data, "mimeType": mime_type}))
}

fn normalize_resource_content(resource: &Value) -> Option<Value> {
    match resource.get("type").and_then(Value::as_str) {
        Some("resource" | "resource_link") => Some(resource.clone()),
        _ if resource.get("uri").is_some_and(Value::is_string) => {
            Some(json!({"type": "resource", "resource": resource}))
        }
        _ => None,
    }
}

fn effect_output_text(state: &PublicEffectState) -> &str {
    match state {
        PublicEffectState::Pending | PublicEffectState::Skipped { .. } => "",
        PublicEffectState::Running { output_text }
        | PublicEffectState::Blocked { output_text, .. }
        | PublicEffectState::Completed { output_text, .. }
        | PublicEffectState::Failed { output_text, .. }
        | PublicEffectState::Cancelled { output_text, .. } => output_text,
    }
}

fn effect_raw_output(state: &PublicEffectState) -> Value {
    match state {
        PublicEffectState::Completed { output, .. } => output.clone(),
        PublicEffectState::Failed { error, .. } => {
            serde_json::to_value(error).unwrap_or(Value::Null)
        }
        PublicEffectState::Cancelled { reason, .. } | PublicEffectState::Skipped { reason, .. } => {
            json!(reason)
        }
        PublicEffectState::Pending
        | PublicEffectState::Running { .. }
        | PublicEffectState::Blocked { .. } => Value::Null,
    }
}

fn acp_effect_status(state: &PublicEffectState) -> &'static str {
    match state {
        PublicEffectState::Pending => "pending",
        PublicEffectState::Running { .. } | PublicEffectState::Blocked { .. } => "in_progress",
        PublicEffectState::Completed { .. } | PublicEffectState::Skipped { .. } => "completed",
        PublicEffectState::Failed { .. } | PublicEffectState::Cancelled { .. } => "failed",
    }
}

fn acp_callback_status(state: &PublicCallbackState) -> &'static str {
    match state {
        PublicCallbackState::Open => "pending",
        PublicCallbackState::Answered { .. } => "completed",
        PublicCallbackState::Cancelled { .. } | PublicCallbackState::Expired { .. } => "failed",
    }
}

/// Projects one persisted history entry for replay. Persisted entries predate
/// the structured public shape, so both encodings are accepted.
pub(crate) fn history_entry_updates(entry: &Value, index: usize) -> Result<Vec<Value>, AcpError> {
    if entry.get("type").and_then(Value::as_str).is_some() {
        let public = serde_json::from_value::<PublicHistoryEntry>(entry.clone())?;
        return public_entry_updates(None, &public);
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
        Some(Value::Object(_)) => content
            .and_then(normalize_history_block)
            .into_iter()
            .collect(),
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

pub(crate) fn send_usage(
    session_id: &str,
    turn: &ProgrammaticTurn,
    sender: &UpdateSender,
) -> Result<(), AcpError> {
    let mut update = json!({
        "sessionUpdate": "usage_update",
        "used": turn.context_tokens,
        "size": 200_000,
    });
    if turn.usage.price_micros > 0 {
        update["cost"] = json!({
            "amount": turn.usage.price_micros as f64 / 1_000_000.0,
            "currency": "USD",
        });
    }
    queue_update(sender, session_id, update)
}

pub(crate) fn acp_stop_reason(turn: &ProgrammaticTurn) -> &'static str {
    match turn.stop_reason {
        PublicTurnStopReason::Complete => "end_turn",
        PublicTurnStopReason::MaxSteps => "max_turn_requests",
        PublicTurnStopReason::TokenLimit
        | PublicTurnStopReason::PriceLimit
        | PublicTurnStopReason::ResponseLength => "max_tokens",
        PublicTurnStopReason::Refusal => "refusal",
        PublicTurnStopReason::Cancelled | PublicTurnStopReason::Failed => "cancelled",
    }
}

/// Validates untrusted ACP prompt blocks and projects them onto the canonical
/// turn request.
pub(crate) fn turn_request(content: Vec<Value>) -> Result<TurnRequest, AcpError> {
    if content.is_empty() {
        return Err(AcpError::InvalidParams(
            "prompt must contain at least one content block".to_owned(),
        ));
    }
    if content.len() > MAX_PROMPT_BLOCKS {
        return Err(AcpError::InvalidParams(format!(
            "prompt cannot contain more than {MAX_PROMPT_BLOCKS} content blocks"
        )));
    }
    let mut public = Vec::with_capacity(content.len());
    let mut text = Vec::new();
    let mut content_bytes = 0_usize;
    for block in &content {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::InvalidParams("content block type is required".to_owned()))?;
        content_bytes = content_bytes.saturating_add(serde_json::to_vec(block)?.len());
        if content_bytes > MAX_EMBEDDED_CONTENT_BYTES {
            return Err(AcpError::InvalidParams(format!(
                "prompt content exceeds {MAX_EMBEDDED_CONTENT_BYTES} bytes"
            )));
        }
        public.push(prompt_block(block, kind, &mut text)?);
    }
    if text.is_empty() {
        let fallback = "[Attached content]".to_owned();
        public.insert(
            0,
            PublicContentBlock::Text {
                text: fallback.clone(),
            },
        );
        text.push(fallback);
    }
    Ok(TurnRequest {
        prompt: text.join("\n\n"),
        input: public,
        injected: false,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: Some(Value::Array(content)),
        mention_stats: None,
    })
}

fn prompt_block(
    block: &Value,
    kind: &str,
    text: &mut Vec<String>,
) -> Result<PublicContentBlock, AcpError> {
    match kind {
        "text" => {
            let value = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| AcpError::InvalidParams("text block requires text".to_owned()))?
                .to_owned();
            text.push(value.clone());
            Ok(PublicContentBlock::Text { text: value })
        }
        "image" => {
            let mime = block
                .get("mimeType")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AcpError::InvalidParams("image block requires mimeType".to_owned())
                })?;
            if !matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) {
                return Err(AcpError::InvalidParams(format!(
                    "unsupported image MIME type `{mime}`"
                )));
            }
            let data = block.get("data").and_then(Value::as_str).ok_or_else(|| {
                AcpError::InvalidParams("image block requires base64 data".to_owned())
            })?;
            if data.is_empty()
                || base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .is_err()
            {
                return Err(AcpError::InvalidParams(
                    "image data is not canonical base64".to_owned(),
                ));
            }
            Ok(PublicContentBlock::Image {
                attachment: block.clone(),
            })
        }
        "resource" | "resource_link" => {
            let has_uri = block
                .get("uri")
                .or_else(|| block.pointer("/resource/uri"))
                .and_then(Value::as_str)
                .is_some_and(|uri| !uri.trim().is_empty());
            if !has_uri {
                return Err(AcpError::InvalidParams(
                    "resource block requires a URI".to_owned(),
                ));
            }
            Ok(PublicContentBlock::Resource {
                resource: block.clone(),
            })
        }
        unsupported => Err(AcpError::InvalidParams(format!(
            "unsupported content block `{unsupported}`"
        ))),
    }
}
