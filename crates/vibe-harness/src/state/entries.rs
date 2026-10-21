//! The history entries the projector writes, and the edits it makes to the
//! ones already there.

use serde_json::{Map, Value, json};

use super::{ReasoningPart, ToolAction, ToolResult};

/// A message entry, as [`message_entry`] builds it.
pub(super) struct MessageEntry<'a> {
    pub(super) session_id: &'a str,
    pub(super) turn_id: Option<&'a str>,
    pub(super) entry_id: &'a str,
    pub(super) role: &'a str,
    pub(super) content: &'a [Value],
    pub(super) observed_at: u64,
    pub(super) source: &'a str,
    pub(super) input_entry_id: Option<&'a str>,
    pub(super) client_message_id: Option<&'a str>,
    pub(super) user_display_content: Option<&'a Value>,
}

pub(super) fn message_entry(entry: &MessageEntry<'_>) -> Value {
    let mut out = json!({
        "type": "message",
        "id": entry.client_message_id.unwrap_or(entry.entry_id),
        "sessionId": entry.session_id,
        "turnId": entry.turn_id,
        "createdAt": entry.observed_at,
        "updatedAt": entry.observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": entry.input_entry_id,
        "role": entry.role,
        "content": entry.content,
        "source": entry.source,
    });
    if entry.role == "user"
        && let Some(display) = entry.user_display_content
    {
        out["userDisplayContent"] = display.clone();
    }
    if entry.role == "assistant" {
        out["outcome"] = json!({"type": "committed"});
    }
    out
}

/// A user message entry the host appends on its own.
#[must_use]
pub fn public_message_entry(
    session_id: &str,
    turn_id: Option<&str>,
    entry_id: &str,
    role: &str,
    content: &[Value],
    observed_at: u64,
    source: &str,
) -> Value {
    message_entry(&MessageEntry {
        session_id,
        turn_id,
        entry_id,
        role,
        content,
        observed_at,
        source,
        input_entry_id: None,
        client_message_id: None,
        user_display_content: None,
    })
}

pub(super) fn reasoning_entry(
    session_id: &str,
    turn_id: &str,
    entry_id: &str,
    part: &ReasoningPart,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Option<Value> {
    let mut text = part.text.concat();
    if text.is_empty() && part.summary.is_empty() && !part.redacted {
        return None;
    }
    if text.is_empty() && part.redacted {
        "[redacted]".clone_into(&mut text);
    }
    Some(json!({
        "type": "reasoning",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "outcome": {"type": "committed"},
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "text": text,
        "summary": part.summary,
    }))
}

pub(super) fn running_effect_entry(
    session_id: &str,
    action: &ToolAction,
    observed_at: u64,
    input_entry_id: Option<String>,
) -> Value {
    let (tool_name, detail) = effect_detail(action);
    json!({
        "type": "effect",
        "id": format!("effect-{}", action.action_id),
        "sessionId": session_id,
        "turnId": action.turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "in_progress",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "title": tool_name,
        "detail": detail,
        "state": {"status": "running", "outputText": ""},
    })
}

fn effect_detail(action: &ToolAction) -> (String, Value) {
    let name = action.tool_name.clone();
    if name == "subagent.spawn" {
        let argument = |key: &str| action.arguments.get(key).and_then(Value::as_str);
        let agent_name = argument("agentName").unwrap_or("subagent");
        let agent_type = argument("agentType").unwrap_or("generic");
        let message = argument("message").unwrap_or_default();
        return (
            name.clone(),
            json!({
                "kind": "subagent",
                "toolName": name,
                "input": {"task": message, "agent": agent_type},
                "childSessionId": null,
                "agentName": agent_name,
                "display": {
                    "summary": format!("Starting {agent_name}"),
                    "verb": "Starting",
                    "message": agent_name,
                    "settledVerb": "Started",
                    "settledMessage": format!("Started {agent_name}"),
                    "statusText": format!("Starting {agent_name}"),
                },
            }),
        );
    }
    (
        name.clone(),
        json!({
            "kind": "tool",
            "toolName": name,
            "input": action.arguments,
            "display": {
                "summary": name,
                "verb": "Running",
                "message": name,
                "settledVerb": "Ran",
                "settledMessage": name,
                "statusText": format!("Running {name}"),
            },
        }),
    )
}

pub(super) fn completed_effect_entry(
    session_id: &str,
    entries: &[Value],
    turn_id: &str,
    action_id: &str,
    result: &ToolResult,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Value {
    let entry_id = format!("effect-{action_id}");
    let existing = entries.iter().find(|entry| entry["id"] == json!(entry_id));
    let output_text = result
        .content()
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let serialized = result.to_json();
    let mut state = match result {
        ToolResult::Failure { code, message, .. } if code == "tool_skipped" => {
            let reason = if output_text.is_empty() {
                message.clone()
            } else {
                output_text.clone()
            };
            json!({
                "status": "skipped",
                "reason": reason,
                "display": {"success": false, "message": reason},
            })
        }
        ToolResult::Failure {
            code,
            message,
            details,
            ..
        } => json!({
            "status": "failed",
            "error": {"code": code, "message": message, "details": details},
            "output": serialized,
            "outputText": output_text,
            "display": {"success": false, "message": message},
        }),
        ToolResult::Success { meta, .. } => {
            let title = existing
                .and_then(|entry| entry["title"].as_str())
                .unwrap_or("tool");
            let message = existing
                .and_then(|entry| entry["detail"]["display"]["settledMessage"].as_str())
                .map_or_else(|| format!("{title} completed"), str::to_owned);
            let mut display = json!({"success": true, "message": message});
            if let Some(note) = meta
                .as_ref()
                .and_then(|meta| meta.get("approval_note"))
                .and_then(Value::as_str)
                .filter(|note| !note.is_empty())
            {
                display["approvalNote"] = json!(note);
            }
            json!({
                "status": "completed",
                "output": serialized,
                "outputText": output_text,
                "display": display,
            })
        }
    };
    if let Some(approval) = result
        .meta()
        .and_then(|meta| meta.get("approval"))
        .filter(|approval| {
            ["decision", "approvalType", "approvalSource"]
                .iter()
                .all(|key| approval[*key].is_string())
        })
    {
        for key in ["decision", "approvalType", "approvalSource"] {
            state[key] = approval[key].clone();
        }
    }
    if let Some(existing) = existing {
        let mut updated = existing.clone();
        updated["updatedAt"] = json!(observed_at);
        updated["generationStatus"] = json!("completed");
        updated["detail"] = completed_effect_detail(existing, result.meta());
        updated["state"] = state;
        return updated;
    }
    let message = state["display"]["message"]
        .as_str()
        .unwrap_or("tool completed")
        .to_owned();
    json!({
        "type": "effect",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "title": "tool",
        "detail": {
            "kind": "tool",
            "toolName": "tool",
            "input": null,
            "display": {"summary": "tool", "statusText": "Running tool", "settledMessage": message},
        },
        "state": state,
    })
}

fn completed_effect_detail(existing: &Value, meta: Option<&Map<String, Value>>) -> Value {
    let detail = existing.get("detail").cloned().unwrap_or_else(|| json!({}));
    if detail["kind"] != "subagent" {
        return detail;
    }
    let Some(child) = meta
        .and_then(|meta| meta.get("mistral.vibe.subagent"))
        .and_then(|annotation| annotation["childSessionId"].as_str())
        .filter(|child| !child.is_empty())
    else {
        return detail;
    };
    let mut completed = detail;
    completed["childSessionId"] = json!(child);
    completed
}

/// The entry of a `search_tool_functions` call, which the engine answers
/// itself: complete as soon as it is committed, with what it found.
pub(super) fn discovery_entry(
    session_id: &str,
    turn_id: &str,
    call_id: &str,
    mode: &str,
    output: Value,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Value {
    let message = "for relevant tools";
    json!({
        "type": "effect",
        "id": format!("effect-tool-discovery-{call_id}"),
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "title": "Searching for relevant tools",
        "detail": {
            "kind": "tool",
            "toolName": "search_tool_functions",
            "input": {"mode": mode},
            "display": {
                "summary": "Searching for relevant tools",
                "verb": "Searching",
                "message": "Searching for relevant tools",
                "settledVerb": "Searched",
                "settledMessage": message,
                "statusText": "Searching for relevant tools",
            },
        },
        "state": {
            "status": "completed",
            "output": output,
            "outputText": "",
            "display": {"success": true, "verb": "Searched", "message": message},
        },
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn terminal_compaction_entry(
    session_id: &str,
    entries: &[Value],
    compaction_id: &str,
    turn_id: Option<&str>,
    observed_at: u64,
    input_entry_id: Option<&str>,
    message: &str,
    details: Value,
) -> Value {
    let entry_id = format!("checkpoint-compaction-{compaction_id}");
    let existing = entries.iter().find(|entry| entry["id"] == json!(entry_id));
    let created_at = existing
        .and_then(|entry| entry["createdAt"].as_u64())
        .unwrap_or(observed_at);
    let input = existing
        .and_then(|entry| entry["inputEntryId"].as_str())
        .or(input_entry_id);
    json!({
        "type": "checkpoint",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": created_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": input,
        "kind": "compaction",
        "message": message,
        "details": details,
    })
}

/// Reference `_running_compaction_entry`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn running_compaction_entry(
    session_id: &str,
    entries: &[Value],
    compaction_id: &str,
    turn_id: Option<&str>,
    trigger: &str,
    attempt: u64,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Value {
    let entry_id = format!("checkpoint-compaction-{compaction_id}");
    let created_at = entries
        .iter()
        .find(|entry| entry["id"] == json!(entry_id))
        .and_then(|entry| entry["createdAt"].as_u64())
        .unwrap_or(observed_at);
    json!({
        "type": "checkpoint",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": created_at,
        "updatedAt": observed_at,
        "generationStatus": "in_progress",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "kind": "compaction",
        "message": "Compacting context",
        "details": {"trigger": trigger, "attempt": attempt},
    })
}

/// Reference `public_notice_entry`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn public_notice_entry(
    session_id: &str,
    entry_id: &str,
    kind: &str,
    scope: &str,
    observed_at: u64,
    fields: &[(&str, Option<&str>)],
    status: Option<&str>,
    content: Option<&str>,
) -> Value {
    let mut detail = json!({"kind": kind, "scope": scope});
    let mut hook_name = None;
    for (key, value) in fields {
        if let Some(value) = value {
            detail[*key] = json!(value);
            if *key == "hookName" {
                hook_name = Some(*value);
            }
        }
    }
    if let Some(status) = status {
        detail["status"] = json!(status);
    }
    if let Some(content) = content {
        detail["content"] = json!(content);
    }
    let level = match status {
        Some("warning") => "warning",
        Some("error") => "error",
        _ => "info",
    };
    let message = content.map_or_else(
        || match (kind, hook_name) {
            ("hook_run_started", _) => "Running hooks".to_owned(),
            ("hook_run_completed", _) => "Hooks completed".to_owned(),
            ("hook_started", Some(name)) => format!("Running hook {name}"),
            ("hook_started", None) => "Running hook".to_owned(),
            (_, Some(name)) => format!("Hook {name} completed"),
            (_, None) => "Hook completed".to_owned(),
        },
        str::to_owned,
    );
    json!({
        "type": "notice",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": null,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "level": level,
        "relatedEntryId": null,
        "inputEntryId": null,
        "message": message,
        "detail": detail,
    })
}

/// Whether an entry is streamed completion content awaiting its commit.
#[must_use]
pub fn is_provisional(entry: &Value) -> bool {
    entry["generationStatus"] == "in_progress"
        && (entry["type"] == "reasoning"
            || (entry["type"] == "message" && entry["role"] == "assistant"))
}

pub(super) fn belongs_to_action(entry: &Value, action_id: &str) -> bool {
    entry["id"].as_str().is_some_and(|id| {
        id == format!("assistant-{action_id}") || id.starts_with(&format!("reasoning-{action_id}-"))
    })
}

pub(super) fn settle_provisional(entry: &mut Value, observed_at: u64, cause: &str) {
    entry["updatedAt"] = json!(observed_at);
    entry["generationStatus"] = json!("completed");
    entry["outcome"] = json!({"type": "discarded", "cause": cause});
}

pub(super) fn is_open_effect(entry: &Value, turn_id: &str) -> bool {
    entry["type"] == "effect"
        && entry["turnId"] == json!(turn_id)
        && entry["generationStatus"] != "completed"
}

pub(super) fn settle_open_effects(
    entries: &mut [Value],
    turn_id: &str,
    observed_at: u64,
    cancelled: bool,
    reason: &str,
) {
    for entry in entries.iter_mut() {
        if is_open_effect(entry, turn_id) {
            let output_text = entry["state"]["outputText"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            entry["state"] = if cancelled {
                json!({
                    "status": "cancelled",
                    "reason": reason,
                    "outputText": output_text,
                    "display": {"success": false, "message": reason},
                })
            } else {
                json!({
                    "status": "failed",
                    "error": {"message": reason},
                    "outputText": output_text,
                    "display": {"success": false, "message": reason},
                })
            };
            entry["updatedAt"] = json!(observed_at);
            entry["generationStatus"] = json!("completed");
        } else if is_provisional(entry) && entry["turnId"] == json!(turn_id) {
            settle_provisional(
                entry,
                observed_at,
                if cancelled { "interrupt" } else { "failure" },
            );
        }
    }
}

pub(super) fn merge_provisional(
    entries: &mut [Value],
    entry_id: &str,
    observed_at: u64,
    extend: impl FnOnce(&mut Value),
) -> Option<()> {
    let entry = entries
        .iter_mut()
        .find(|entry| entry["id"] == json!(entry_id))?;
    if is_provisional(entry) {
        extend(entry);
        entry["updatedAt"] = json!(observed_at);
    }
    Some(())
}

pub(super) fn upsert(entries: &mut Vec<Value>, entry: Value) {
    match entries
        .iter_mut()
        .find(|existing| existing["id"] == entry["id"])
    {
        Some(existing) => *existing = entry,
        None => entries.push(entry),
    }
}

pub(super) fn count_checkpoints(entries: &[Value], kind: &str) -> usize {
    entries
        .iter()
        .filter(|entry| entry["type"] == "checkpoint" && entry["kind"] == json!(kind))
        .count()
}
