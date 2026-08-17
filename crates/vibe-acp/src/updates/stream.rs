//! Projecting the canonical entries of a running turn as ACP session updates.
//!
//! Every entry the app server publishes is cumulative: the same entry is
//! revised as it grows. This module turns that into the deltas ACP expects,
//! and remembers what it already streamed so a revision is never re-sent.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use vibe_app_server::client::{
    ProgrammaticUpdate, PublicCallbackState, PublicContentBlock, PublicEffectState,
    PublicHistoryEntry, PublicMessageRole,
};

use crate::protocol::AcpError;
use crate::updates::{UpdateSender, content_chunk, queue_update, text_chunk};

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
    for update in public_entry_updates(previous.as_ref(), entry.as_ref(), &message_id)? {
        queue_update(sender, session_id, update)?;
    }
    Ok(())
}

/// Projects one canonical entry revision as ACP updates.
///
/// `message_id` is the ACP-visible identity of this entry, which is the entry
/// ID everywhere except the first user message of a turn: `session/fork`
/// anchors on that one, so the turn gives it an ID a later fork can resolve.
pub(crate) fn public_entry_updates(
    previous: Option<&PublicHistoryEntry>,
    entry: &PublicHistoryEntry,
    message_id: &str,
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
                    .map(|content| content_chunk(kind, message_id, content))
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
                .map(|text| vec![text_chunk(kind, message_id, &text)])
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
                .map(|text| vec![text_chunk("agent_thought_chunk", message_id, &text)])
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

pub(super) fn normalize_image_content(attachment: &Value) -> Option<Value> {
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

pub(super) fn normalize_resource_content(resource: &Value) -> Option<Value> {
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
