//! The harness snapshot as a client reads it.
//!
//! Reference `_read_response`, `_public_session`, `_public_turn` and
//! `_project_history_entries` in
//! `vibe/app_server/_unified_harness_backend_adapter.py`: the harness's
//! entries take the wire's typed shapes, its session gains what the host
//! keeps beside it (the marks, the pins, the working directory), and its
//! latest turn becomes the state's only turn.

use serde_json::{Value, json};

use crate::state::{HarnessSession, HarnessState, HarnessTurn, TokenUsage, TurnStatus};
use crate::tool_projection::{project_callback_effect, project_effect, result_display};

/// The wire format tag of a session state.
pub const WIRE_STATE_FORMAT: &str = "vibe.public-session-state/v1";

/// The marks the host keeps for a session, each in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionMarks {
    pub bumped_at: Option<u64>,
    pub pinned_at: Option<u64>,
    pub archived_at: Option<u64>,
    pub is_unseen: bool,
}

/// What the session runs, as the host pinned it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionPins {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub agent: Option<Value>,
}

/// Everything besides the snapshot a wire state is composed from.
#[derive(Debug, Clone, Default)]
pub struct WireContext {
    pub cwd: Option<String>,
    pub event_id: u64,
    /// How many entries the state publishes, the latest ones; `None` for all.
    pub history_limit: Option<usize>,
    pub marks: SessionMarks,
    pub pins: SessionPins,
    pub worktree: Option<Value>,
    pub child_sessions: Vec<Value>,
}

/// The wire token usage, which drops the cached count.
fn wire_usage(usage: Option<TokenUsage>) -> Value {
    usage.map_or(Value::Null, |usage| {
        json!({
            "inputTokens": usage.input_tokens,
            "outputTokens": usage.output_tokens,
            "totalTokens": usage.total_tokens,
        })
    })
}

/// Reference `_public_session`.
#[must_use]
pub fn wire_session(session: &HarnessSession, context: &WireContext) -> Value {
    let status = match &session.status {
        crate::state::SessionStatus::Idle => json!({"type": "idle"}),
        crate::state::SessionStatus::Running { active_turn_id } => {
            json!({"type": "running", "activeTurnId": active_turn_id})
        }
        crate::state::SessionStatus::Blocked {
            active_turn_id,
            callback_id,
            callback_kind,
        } => json!({
            "type": "blocked",
            "activeTurnId": active_turn_id,
            "callbackId": callback_id,
            "reason": callback_kind,
        }),
        crate::state::SessionStatus::Failed { message } => {
            json!({"type": "failed", "message": message})
        }
    };
    json!({
        "id": session.id,
        "rootSessionId": session.root_session_id,
        "parentSessionId": session.parent_session_id,
        "title": session.title,
        "preview": crate::loops::project_loop_preview(&crate::invocation::public_preview(&session.preview)),
        "status": status,
        "createdAt": session.created_at,
        "updatedAt": session.updated_at,
        "bumpedAt": context.marks.bumped_at,
        "pinnedAt": context.marks.pinned_at,
        "archivedAt": context.marks.archived_at,
        "isUnseen": context.marks.is_unseen,
        "cwd": context.cwd,
        "workspaceRoots": [],
        "worktree": context.worktree,
        "model": context.pins.model,
        "reasoningEffort": context.pins.reasoning_effort,
        "agent": context.pins.agent,
        "tokenUsage": wire_usage(session.token_usage),
        "contextUsage": wire_usage(session.context_usage),
        "harness": "unified",
    })
}

/// Reference `_public_turn`.
#[must_use]
pub fn wire_turn(turn: &HarnessTurn) -> Value {
    let error = turn.error.as_ref().map_or(Value::Null, |error| {
        json!({
            "message": error.message,
            "code": harness_turn_error_code(&error.code, error.details.as_ref()),
            "details": error.details,
        })
    });
    json!({
        "id": turn.id,
        "sessionId": turn.session_id,
        "status": turn.status.as_str(),
        "startedAt": turn.started_at,
        "completedAt": if turn.status == TurnStatus::InProgress { Value::Null } else { json!(turn.completed_at) },
        "error": error,
        "stopReason": null,
        "queueItemId": turn.queue_item_id,
        "inputEntryId": turn.input_entry_id,
    })
}

/// Reference `_harness_turn_error_code`: the harness's own error vocabulary
/// mapped onto the codes a client knows, any other code kept.
fn harness_turn_error_code<'a>(code: &'a str, details: Option<&Value>) -> &'a str {
    if code == "model_stream_failed" {
        match details.and_then(|details| details["httpStatus"].as_u64()) {
            Some(429) => return "rate_limit",
            Some(401 | 403) => return "invalid_api_key",
            _ => {}
        }
    }
    match code {
        "images_not_supported" => "images_not_supported",
        "model_stream_failed" => "backend_error",
        "model_stream_incomplete" => "incomplete_stream",
        "model_unauthorized" => "invalid_api_key",
        other => other,
    }
}

/// Reference `_read_response`.
#[must_use]
pub fn wire_state(state: &HarnessState, context: &WireContext) -> Value {
    let entries = &state.entries;
    let window_start = context
        .history_limit
        .map_or(0, |limit| entries.len().saturating_sub(limit));
    let has_older = window_start > 0;
    let mut history: Vec<Value> = entries[window_start..]
        .iter()
        .flat_map(wire_entries)
        .collect();
    for callback in &state.active_callbacks {
        let projected = wire_entry(callback);
        match history
            .iter_mut()
            .find(|entry| entry["id"] == projected["id"])
        {
            Some(existing) => *existing = projected,
            None => history.push(projected),
        }
    }
    let history_before_cursor = if has_older {
        history
            .first()
            .map_or(Value::Null, |entry| entry["id"].clone())
    } else {
        Value::Null
    };
    json!({
        "format": WIRE_STATE_FORMAT,
        "eventId": context.event_id,
        "session": wire_session(&state.session, context),
        "history": history,
        "historyBeforeCursor": history_before_cursor,
        "turns": state.latest_turn.as_ref().map(wire_turn).into_iter().collect::<Vec<_>>(),
        "turnQueue": state.turn_queue_json(),
        "activeCallbacks": [],
        "childSessions": context.child_sessions,
        "backgroundProcesses": state.background_processes,
        "retrying": state.retrying,
        "isQuiescent": null,
    })
}

/// A user-input request with every default its wire model fills.
fn wire_question_request(request: &Value) -> Value {
    let questions: Vec<Value> = request["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|question| {
            let options: Vec<Value> = question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|option| {
                    json!({
                        "label": option["label"],
                        "description": option.get("description").cloned().unwrap_or_else(|| json!("")),
                    })
                })
                .collect();
            json!({
                "question": question["question"],
                "header": question.get("header").cloned().unwrap_or_else(|| json!("")),
                "options": options,
                "multiSelect": question.get("multiSelect").cloned().unwrap_or(json!(false)),
                "hideOther": question.get("hideOther").cloned().unwrap_or(json!(false)),
            })
        })
        .collect();
    json!({
        "questions": questions,
        "footerNote": request.get("footerNote").cloned().unwrap_or(Value::Null),
    })
}

/// Reference `_project_history_entries`: one harness entry as the wire
/// entries it publishes.
#[must_use]
pub fn wire_entries(entry: &Value) -> Vec<Value> {
    let mut normalized = entry.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("outcome");
        if object.get("type") == Some(&json!("notice"))
            && !object.contains_key("detail")
            && object
                .get("details")
                .is_some_and(|details| details["kind"] == "scheduled_loop_fired")
            && let Some(details) = object.remove("details")
        {
            object.insert("detail".to_owned(), details);
        }
    }
    let projected = wire_entry(&normalized);
    if projected["type"] == "message" {
        return crate::invocation::expand_message(&projected);
    }
    vec![projected]
}

/// Reference `_public_content_block`: an image the harness holds reads as
/// an inline attachment, whatever it was read from.
fn public_content(content: &Value) -> Value {
    let Some(blocks) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        blocks
            .iter()
            .map(|block| {
                if block["type"] == "image" && block.get("attachment").is_none() {
                    json!({
                        "type": "image",
                        "attachment": {
                            "source": {"kind": "inline", "data": block["data"]},
                            "alias": "image",
                            "mimeType": block["mimeType"],
                        },
                    })
                } else {
                    block.clone()
                }
            })
            .collect(),
    )
}

/// One entry in its wire shape, with every default the wire model fills.
#[must_use]
pub fn wire_entry(entry: &Value) -> Value {
    let mut out = json!({
        "id": entry["id"],
        "sessionId": entry["sessionId"],
        "turnId": entry.get("turnId").cloned().unwrap_or(Value::Null),
        "inputEntryId": entry.get("inputEntryId").cloned().unwrap_or(Value::Null),
        "createdAt": entry["createdAt"],
        "updatedAt": entry["updatedAt"],
        "generationStatus": entry["generationStatus"],
        "relatedEntryId": entry.get("relatedEntryId").cloned().unwrap_or(Value::Null),
        "type": entry["type"],
    });
    match entry["type"].as_str() {
        Some("message") => {
            out["role"] = entry["role"].clone();
            out["content"] = if entry["role"] == "user" {
                public_content(&crate::loops::without_instructions(&entry["content"]))
            } else {
                entry["content"].clone()
            };
            out["source"] = entry.get("source").cloned().unwrap_or(Value::Null);
            out["userDisplayContent"] = entry
                .get("userDisplayContent")
                .cloned()
                .unwrap_or(Value::Null);
        }
        Some("reasoning") => {
            out["text"] = entry["text"].clone();
            out["summary"] = entry.get("summary").cloned().unwrap_or_else(|| json!([]));
        }
        Some("effect") => {
            out["title"] = entry["title"].clone();
            out["detail"] = wire_detail(&entry["detail"]);
            out["state"] = wire_effect_state(&entry["state"]);
            project_effect(&mut out);
            normalize_effect_output(&mut out);
        }
        Some("callback") => {
            out["callbackId"] = entry["callbackId"].clone();
            out["title"] = entry["title"].clone();
            let mut detail = entry["detail"].clone();
            if detail["kind"] == "approval" {
                if detail.get("effect").is_some() {
                    let mut effect = wire_detail(&detail["effect"]);
                    project_callback_effect(&mut effect);
                    detail["effect"] = effect;
                }
                if detail.get("pathScopeChoices").is_none() {
                    detail["pathScopeChoices"] = json!([]);
                }
            }
            if detail["kind"] == "user_input" {
                detail["request"] = wire_question_request(&detail["request"]);
            }
            out["detail"] = detail;
            out["state"] = entry["state"].clone();
        }
        Some("checkpoint") => {
            out["kind"] = entry["kind"].clone();
            out["message"] = entry.get("message").cloned().unwrap_or(Value::Null);
            out["details"] = entry.get("details").cloned().unwrap_or(Value::Null);
        }
        Some("notice") => {
            out["level"] = entry["level"].clone();
            out["message"] = entry["message"].clone();
            out["detail"] = wire_notice_detail(&entry["detail"]);
        }
        _ => {}
    }
    out
}

fn wire_notice_detail(detail: &Value) -> Value {
    if detail["kind"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("hook_"))
    {
        let field = |key: &str| detail.get(key).cloned().unwrap_or(Value::Null);
        return json!({
            "kind": detail["kind"],
            "scope": detail.get("scope").cloned().unwrap_or_else(|| json!("post_agent")),
            "toolName": field("toolName"),
            "toolCallId": field("toolCallId"),
            "hookName": field("hookName"),
            "status": field("status"),
            "content": field("content"),
        });
    }
    detail.clone()
}

/// An effect detail with the call display's defaults filled.
fn wire_detail(detail: &Value) -> Value {
    let mut out = detail.clone();
    if !out.is_object() {
        return out;
    }
    let display = &detail["display"];
    let field = |key: &str, default: Value| display.get(key).cloned().unwrap_or(default);
    out["display"] = json!({
        "summary": field("summary", json!("")),
        "content": field("content", Value::Null),
        "suffix": field("suffix", json!("")),
        "verb": field("verb", json!("")),
        "message": field("message", Value::Null),
        "settledVerb": field("settledVerb", json!("")),
        "settledMessage": field("settledMessage", Value::Null),
        "statusText": field("statusText", json!("")),
    });
    if out.get("input").is_none() {
        out["input"] = Value::Null;
    }
    out
}

fn wire_result_display(display: &Value) -> Value {
    if !display.is_object() {
        return Value::Null;
    }
    let mut out = result_display(
        display["success"].as_bool().unwrap_or(false),
        display
            .get("verb")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        display
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    for key in ["warnings", "approvalNote", "suffix"] {
        if let Some(value) = display.get(key) {
            out[key] = value.clone();
        }
    }
    out
}

/// A settled state with the wire model's defaults, durations as floats.
fn wire_effect_state(state: &Value) -> Value {
    let status = state["status"].as_str().unwrap_or("running");
    let text = || {
        state
            .get("outputText")
            .cloned()
            .unwrap_or_else(|| json!(""))
    };
    let duration = || {
        state
            .get("durationMs")
            .and_then(Value::as_f64)
            .map_or_else(|| json!(0.0), |ms| json!(ms))
    };
    let approval = |mut out: Value| {
        for key in ["decision", "approvalType", "approvalSource"] {
            out[key] = state.get(key).cloned().unwrap_or(Value::Null);
        }
        out
    };
    match status {
        "pending" => json!({"status": "pending"}),
        "blocked" => json!({
            "status": "blocked",
            "callbackId": state["callbackId"],
            "outputText": text(),
        }),
        "completed" => approval(json!({
            "status": "completed",
            "output": state.get("output").cloned().unwrap_or(Value::Null),
            "outputText": text(),
            "durationMs": duration(),
            "display": wire_result_display(&state["display"]),
        })),
        "failed" => {
            let error = &state["error"];
            approval(json!({
                "status": "failed",
                "error": {
                    "message": error.get("message").cloned().unwrap_or_else(|| json!("")),
                    "code": error.get("code").cloned().unwrap_or(Value::Null),
                    "details": error.get("details").cloned().unwrap_or(Value::Null),
                },
                "output": state.get("output").cloned().unwrap_or(Value::Null),
                "outputText": text(),
                "durationMs": duration(),
                "display": wire_result_display(&state["display"]),
            }))
        }
        "cancelled" => approval(json!({
            "status": "cancelled",
            "reason": state["reason"],
            "outputText": text(),
            "durationMs": duration(),
            "display": wire_result_display(&state["display"]),
        })),
        "skipped" => approval(json!({
            "status": "skipped",
            "reason": state["reason"],
            "display": wire_result_display(&state["display"]),
        })),
        _ => json!({"status": "running", "outputText": text()}),
    }
}

/// The output models each projected kind settles into, by the field names
/// they declare (snake and camel, as pydantic accepts either).
fn output_fields(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "shell" => &["stdout", "stderr", "output", "truncated"],
        "file_edit" => &[
            "file",
            "old_string",
            "oldString",
            "new_string",
            "newString",
            "occurrences",
        ],
        "file_search" => &[
            "matches",
            "match_count",
            "matchCount",
            "was_truncated",
            "wasTruncated",
            "parsed_matches",
            "parsedMatches",
        ],
        "file_read" => &[
            "file_path",
            "filePath",
            "content",
            "num_lines",
            "numLines",
            "start_line",
            "startLine",
            "requested_offset",
            "requestedOffset",
            "requested_limit",
            "requestedLimit",
            "total_lines",
            "totalLines",
            "was_truncated",
            "wasTruncated",
        ],
        "todo" => &["todos"],
        "file_write" => &[
            "file_path",
            "filePath",
            "content",
            "file_existed",
            "fileExisted",
            "previous_content",
            "previousContent",
        ],
        "user_question" => &["answers", "cancelled"],
        "web_search" => &["query", "answer", "sources"],
        "web_fetch" => &[
            "url",
            "content",
            "content_type",
            "contentType",
            "was_truncated",
            "wasTruncated",
        ],
        "skill" => &["name", "content", "skill_dir", "skillDir"],
        "subagent" => &["response", "turns_used", "turnsUsed", "completed"],
        _ => return None,
    })
}

/// Reference `_normalize_effect_output`: a completed effect's output that no
/// longer fits its kind's model (a hook replaced the result) degrades to null.
fn normalize_effect_output(entry: &mut Value) {
    if entry["state"]["status"] != "completed" {
        return;
    }
    let Some(fields) = entry["detail"]["kind"].as_str().and_then(output_fields) else {
        return;
    };
    let Some(output) = entry["state"]["output"].as_object() else {
        return;
    };
    if !output.is_empty() && !output.keys().any(|key| fields.contains(&key.as_str())) {
        entry["state"]["output"] = Value::Null;
    }
}
