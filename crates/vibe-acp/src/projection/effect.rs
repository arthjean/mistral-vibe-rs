//! Effect entries as tool calls: the call, its progress, and the plan a todo
//! effect publishes.

mod locations;

use serde_json::{Map, Value, json};

use super::{is_completed, object, text_content, text_delta};
use locations::{effect_locations, resolved_path};

/// The ACP tool kind an effect kind renders as.
fn tool_kind(effect_kind: &str) -> &'static str {
    match effect_kind {
        "shell" | "process" => "execute",
        "file_edit" | "file_write" => "edit",
        "file_search" | "web_search" => "search",
        "file_read" | "skill" => "read",
        "web_fetch" => "fetch",
        "subagent" => "think",
        _ => "other",
    }
}

pub(super) fn detail(entry: &Value) -> &Value {
    entry.get("detail").unwrap_or(&Value::Null)
}

pub(super) fn effect_kind(entry: &Value) -> &str {
    detail(entry)
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("tool")
}

fn state(entry: &Value) -> &Value {
    entry.get("state").unwrap_or(&Value::Null)
}

fn state_status(entry: &Value) -> &str {
    state(entry)
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The typed input an effect was called with, as the reference's
/// `effect_input_json` publishes it.
fn effect_input(entry: &Value) -> Value {
    detail(entry).get("input").cloned().unwrap_or(Value::Null)
}

fn effect_title(entry: &Value) -> Value {
    let summary = detail(entry)
        .pointer("/display/summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.is_empty());
    match summary {
        Some(summary) => json!(summary),
        None => entry.get("title").cloned().unwrap_or(Value::Null),
    }
}

pub(super) fn effect_start(entry: &Value) -> Value {
    object(vec![
        ("sessionUpdate", json!("tool_call")),
        (
            "toolCallId",
            entry.get("id").cloned().unwrap_or(Value::Null),
        ),
        ("title", effect_title(entry)),
        ("kind", json!(tool_kind(effect_kind(entry)))),
        ("status", json!(effect_status(entry))),
        ("content", content_or_null(replayed_effect_content(entry))),
        ("locations", locations_or_null(effect_locations(entry))),
        ("rawInput", effect_input(entry)),
        ("rawOutput", raw_output(entry)),
        ("_meta", Value::Object(effect_meta(entry))),
    ])
}

pub(super) fn effect_progress_updates(previous: &Value, entry: &Value) -> Vec<Value> {
    let output_delta = text_delta(output_text(previous), output_text(entry));
    let became_terminal = !is_completed(previous) && is_completed(entry);
    let state_changed = state_status(previous) != state_status(entry);
    let detail_changed = detail(previous) != detail(entry);

    let mut content = Vec::new();
    if !output_delta.is_empty() {
        content.push(text_content(&output_delta));
    }
    if became_terminal || state_changed {
        content.extend(result_effect_content(entry, &output_delta));
    }
    if detail_changed {
        content.extend(call_effect_content(entry).unwrap_or_default());
    }
    let progress = object(vec![
        ("sessionUpdate", json!("tool_call_update")),
        (
            "toolCallId",
            entry.get("id").cloned().unwrap_or(Value::Null),
        ),
        (
            "title",
            if detail_changed {
                effect_title(entry)
            } else {
                Value::Null
            },
        ),
        ("kind", json!(tool_kind(effect_kind(entry)))),
        ("status", json!(effect_status(entry))),
        ("content", content_or_null(content)),
        (
            "locations",
            if detail_changed || became_terminal || state_changed {
                locations_or_null(effect_locations(entry))
            } else {
                Value::Null
            },
        ),
        (
            "rawInput",
            if detail_changed {
                effect_input(entry)
            } else {
                Value::Null
            },
        ),
        (
            "rawOutput",
            if became_terminal || state_changed {
                raw_output(entry)
            } else {
                Value::Null
            },
        ),
        ("_meta", Value::Object(effect_meta(entry))),
    ]);
    let mut updates = vec![progress];
    if became_terminal || state_changed {
        updates.extend(plan_update(entry));
    }
    updates
}

fn content_or_null(content: Vec<Value>) -> Value {
    if content.is_empty() {
        Value::Null
    } else {
        Value::Array(content)
    }
}

fn locations_or_null(locations: Option<Vec<Value>>) -> Value {
    locations.map_or(Value::Null, content_or_null)
}

fn replayed_effect_content(entry: &Value) -> Vec<Value> {
    if !is_completed(entry) {
        let mut content = call_effect_content(entry).unwrap_or_default();
        let output = output_text(entry);
        if !output.is_empty() {
            content.push(text_content(output));
        }
        return content;
    }
    let mut content = result_effect_content(entry, "");
    let output = output_text(entry);
    if !output.is_empty() {
        content.insert(0, text_content(output));
    }
    content
}

fn diff_content(path: &Value, old_text: &Value, new_text: &Value) -> Value {
    object(vec![
        ("type", json!("diff")),
        ("path", path.clone()),
        ("oldText", old_text.clone()),
        ("newText", new_text.clone()),
    ])
}

fn result_effect_content(entry: &Value, output_delta: &str) -> Vec<Value> {
    let mut content = Vec::new();
    match effect_kind(entry) {
        "shell" if state_status(entry) == "completed" => {
            // The transcript arrives as output; a trailer would repeat its tail.
            return content;
        }
        "file_edit" => {
            if let Some(output) = effect_output(entry) {
                let occurrences = output
                    .get("occurrences")
                    .and_then(Value::as_array)
                    .filter(|occurrences| !occurrences.is_empty());
                if let Some(occurrences) = occurrences {
                    for occurrence in occurrences {
                        content.push(diff_content(
                            output.get("file").unwrap_or(&Value::Null),
                            occurrence.get("oldText").unwrap_or(&Value::Null),
                            occurrence.get("newText").unwrap_or(&Value::Null),
                        ));
                    }
                } else if let (Some(old), Some(new)) = (
                    output.get("oldString").filter(|value| !value.is_null()),
                    output.get("newString").filter(|value| !value.is_null()),
                ) {
                    content.push(diff_content(
                        output.get("file").unwrap_or(&Value::Null),
                        old,
                        new,
                    ));
                }
            }
        }
        "file_write" => {
            if let Some(output) = effect_output(entry) {
                content.push(diff_content(
                    output.get("filePath").unwrap_or(&Value::Null),
                    &Value::Null,
                    output.get("content").unwrap_or(&Value::Null),
                ));
            }
        }
        _ => {}
    }
    let display = result_display_text(entry);
    if !display.is_empty() && display != output_delta {
        content.push(text_content(&display));
    }
    content
}

fn call_effect_content(entry: &Value) -> Option<Vec<Value>> {
    let input = detail(entry).get("input").unwrap_or(&Value::Null);
    match effect_kind(entry) {
        "file_edit" if input.get("changes").is_some() => {
            return Some(
                input
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|change| {
                        diff_content(
                            input.get("filePath").unwrap_or(&Value::Null),
                            change.get("oldString").unwrap_or(&Value::Null),
                            change.get("newString").unwrap_or(&Value::Null),
                        )
                    })
                    .collect(),
            );
        }
        "file_edit" if input.is_object() => {
            return Some(vec![diff_content(
                input.get("filePath").unwrap_or(&Value::Null),
                input.get("oldString").unwrap_or(&Value::Null),
                input.get("newString").unwrap_or(&Value::Null),
            )]);
        }
        "file_write" if input.is_object() => {
            return Some(vec![diff_content(
                input.get("filePath").unwrap_or(&Value::Null),
                &Value::Null,
                input.get("content").unwrap_or(&Value::Null),
            )]);
        }
        _ => {}
    }
    detail(entry)
        .pointer("/display/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| vec![text_content(text)])
}

/// The typed result of a completed effect, when it published one.
pub(super) fn effect_output(entry: &Value) -> Option<Value> {
    if state_status(entry) != "completed" {
        return None;
    }
    state(entry)
        .get("output")
        .filter(|output| !output.is_null())
        .cloned()
}

fn effect_status(entry: &Value) -> &'static str {
    match state_status(entry) {
        "pending" => "pending",
        "running" | "blocked" => "in_progress",
        "completed" => {
            if effect_kind(entry) == "subagent"
                && effect_output(entry)
                    .and_then(|output| output.get("completed").and_then(Value::as_bool))
                    == Some(false)
            {
                "failed"
            } else {
                "completed"
            }
        }
        _ => "failed",
    }
}

fn raw_output(entry: &Value) -> Value {
    let state = state(entry);
    match state_status(entry) {
        "completed" => state.get("output").cloned().unwrap_or(Value::Null),
        "failed" => match state.get("output").filter(|output| !output.is_null()) {
            Some(output) => output.clone(),
            None => state.get("error").cloned().unwrap_or(Value::Null),
        },
        "cancelled" | "skipped" => state.get("reason").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn output_text(entry: &Value) -> &str {
    match state_status(entry) {
        "running" | "blocked" | "completed" | "failed" | "cancelled" => state(entry)
            .get("outputText")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        _ => "",
    }
}

/// Reference `EffectResultDisplay.text`: the verb and the message.
fn result_display_text(entry: &Value) -> String {
    let display = match state_status(entry) {
        "completed" | "failed" | "cancelled" | "skipped" => state(entry).get("display"),
        _ => None,
    };
    let Some(display) = display.filter(|display| !display.is_null()) else {
        return String::new();
    };
    let field = |key: &str| display.get(key).and_then(Value::as_str).unwrap_or_default();
    format!("{} {}", field("verb"), field("message"))
        .trim()
        .to_owned()
}

fn effect_meta(entry: &Value) -> Map<String, Value> {
    let detail = detail(entry);
    let mut meta = Map::new();
    meta.insert(
        "tool_name".to_owned(),
        detail.get("toolName").cloned().unwrap_or(Value::Null),
    );
    meta.insert("effect_kind".to_owned(), json!(effect_kind(entry)));
    let input = detail.get("input").filter(|input| input.is_object());
    let output = effect_output(entry);
    match effect_kind(entry) {
        "shell" => {
            if output
                .as_ref()
                .and_then(|output| output.get("truncated"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                meta.insert("output_truncated".to_owned(), json!(true));
            }
        }
        "file_search" => {
            if let Some(input) = input {
                meta.insert(
                    "query".to_owned(),
                    input.get("pattern").cloned().unwrap_or(Value::Null),
                );
                meta.insert(
                    "search_path".to_owned(),
                    json!(resolved_path(
                        input.get("path").and_then(Value::as_str).unwrap_or(".")
                    )),
                );
            }
        }
        "web_search" => {
            if let Some(input) = input {
                meta.insert(
                    "query".to_owned(),
                    input.get("query").cloned().unwrap_or(Value::Null),
                );
            }
        }
        "skill" => {
            if let Some(input) = input {
                let name = output
                    .as_ref()
                    .and_then(|output| output.get("name"))
                    .or_else(|| input.get("name"))
                    .cloned()
                    .unwrap_or(Value::Null);
                meta.insert("skill_name".to_owned(), name);
            }
        }
        "subagent" => {
            if let Some(input) = input {
                meta.insert(
                    "agent".to_owned(),
                    input.get("agent").cloned().unwrap_or(Value::Null),
                );
                meta.insert(
                    "task".to_owned(),
                    input.get("task").cloned().unwrap_or(Value::Null),
                );
            }
            if let Some(child) = detail
                .get("childSessionId")
                .filter(|child| !child.is_null())
            {
                meta.insert("child_session_id".to_owned(), child.clone());
            }
            if let Some(output) = output {
                meta.insert(
                    "turn_count".to_owned(),
                    output.get("turnsUsed").cloned().unwrap_or(Value::Null),
                );
                meta.insert(
                    "response".to_owned(),
                    output.get("response").cloned().unwrap_or(Value::Null),
                );
            }
        }
        _ => {}
    }
    meta
}

pub(super) fn plan_update(entry: &Value) -> Option<Value> {
    if effect_kind(entry) != "todo" {
        return None;
    }
    let todos = effect_output(entry)
        .and_then(|output| output.get("todos").cloned())
        .or_else(|| {
            detail(entry)
                .get("input")
                .and_then(|input| input.get("todos"))
                .filter(|todos| !todos.is_null())
                .cloned()
        })
        .unwrap_or_else(|| json!([]));
    let entries = todos
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            if status == "cancelled" {
                return None;
            }
            let status = match status {
                "pending" | "in_progress" | "completed" => status,
                _ => "pending",
            };
            Some(json!({
                "content": item.get("content").cloned().unwrap_or(Value::Null),
                "status": status,
                "priority": item.get("priority").cloned().unwrap_or(json!("medium")),
            }))
        })
        .collect::<Vec<_>>();
    Some(json!({
        "sessionUpdate": "plan",
        "entries": entries,
        "_meta": {"effect_entry_id": entry.get("id").cloned().unwrap_or(Value::Null)},
    }))
}
