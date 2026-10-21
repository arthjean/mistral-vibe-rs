//! The tools a session keeps for itself: `todo`, the scratchpad and
//! `cron`.

use serde_json::{Map, Value, json};

use super::{
    Projected, call_display, completed_state, detail, nonempty, result_display, str_field,
    structured,
};

/// A todo item with the wire's defaults, or `None` when it does not validate.
fn todo_item(item: &Value) -> Option<Value> {
    let status = item.get("status").map_or(Some("pending"), Value::as_str)?;
    let priority = item.get("priority").map_or(Some("medium"), Value::as_str)?;
    if !["pending", "in_progress", "completed", "cancelled"].contains(&status)
        || !["low", "medium", "high"].contains(&priority)
    {
        return None;
    }
    Some(json!({
        "id": item.get("id")?.as_str()?,
        "content": item.get("content")?.as_str()?,
        "status": status,
        "priority": priority,
    }))
}

fn todo_items(value: &Value) -> Option<Vec<Value>> {
    value.as_array()?.iter().map(todo_item).collect()
}

pub(super) fn project_todo(name: &str, input: &Value) -> Option<Projected> {
    let action = nonempty(input, "action")?.to_owned();
    let todos = match input.get("todos") {
        None | Some(Value::Null) => None,
        Some(todos) => Some(todo_items(todos)?),
    };
    let display = match action.as_str() {
        "read" => call_display(
            "Reading todos",
            "Retrieving",
            "todos",
            "Retrieved",
            "todos",
            "Managing todos",
        ),
        "write" => {
            let count = todos.as_ref().map_or(0, Vec::len);
            call_display(
                format!("Writing {count} todos"),
                "Updating",
                format!("{count} todos"),
                "Updated",
                format!("{count} todos"),
                "Managing todos",
            )
        }
        other => call_display(
            format!("Unknown action: {other}"),
            "Running",
            format!("unknown todo action: {other}"),
            "Ran",
            format!("unknown todo action: {other}"),
            "Managing todos",
        ),
    };
    let detail = detail(
        "todo",
        name,
        json!({"action": action, "todos": todos}),
        display,
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let verb = result.get("verb").map_or(Some(""), Value::as_str)?;
            let todos = match result.get("todos") {
                None => Vec::new(),
                Some(todos) => todo_items(todos)?,
            };
            let total = result.get("total_count")?.as_u64()?;
            let display = result_display(true, verb, format!("{total} todos"));
            Some(completed_state(
                state,
                json!({"todos": todos}),
                display,
                None,
            ))
        })),
    })
}

pub(super) fn project_scratchpad(name: &str, input: &Value) -> Option<Projected> {
    let action = nonempty(input, "action")?;
    let path = str_field(input, "path");
    let (semantic, verb, settled, noun) = match action {
        "list" => (
            json!({"action": "list"}),
            "Listing",
            "Listed",
            "the scratchpad".to_owned(),
        ),
        "read" => {
            let path = path?;
            (
                json!({"action": "read", "path": path}),
                "Reading",
                "Read",
                path.to_owned(),
            )
        }
        "write" => {
            let path = path?;
            let content = str_field(input, "content")?;
            (
                json!({"action": "write", "path": path, "content": content}),
                "Saving",
                "Saved",
                path.to_owned(),
            )
        }
        _ => return None,
    };
    let display = call_display(
        format!("{verb} {noun}"),
        verb,
        noun.as_str(),
        settled,
        noun.as_str(),
        "Using the scratchpad",
    );
    Some(Projected {
        detail: detail("scratchpad", name, semantic, display),
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let verb = result
                .get("verb")?
                .as_str()
                .filter(|verb| !verb.is_empty())?;
            let settled = match result.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => path.to_owned(),
                _ => format!(
                    "{} files",
                    result
                        .get("files")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                ),
            };
            let mut updated = state.clone();
            updated["display"]["verb"] = json!(verb);
            updated["display"]["message"] = json!(settled);
            Some(updated)
        })),
    })
}

fn cron_schedule(interval: Option<i64>, cron: Option<&str>) -> Option<String> {
    match (interval, cron) {
        (None, Some(cron)) => Some(format!("cron {cron} (local time)")),
        (Some(mut seconds), None) => {
            let mut parts = Vec::new();
            for (divisor, unit) in [
                (86400, "day"),
                (3600, "hour"),
                (60, "minute"),
                (1, "second"),
            ] {
                let count = seconds / divisor;
                seconds %= divisor;
                if count != 0 {
                    parts.push(format!(
                        "{count} {unit}{}",
                        if count == 1 { "" } else { "s" }
                    ));
                }
            }
            Some(format!("every {}", parts.join(" ")))
        }
        _ => None,
    }
}

/// Reference `_cron_prompt_summary`.
fn cron_prompt_summary(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

pub(super) fn project_cron(name: &str, input: &Value) -> Option<Projected> {
    let action = str_field(input, "action")?.to_owned();
    let (verb, settled, message) = match action.as_str() {
        "schedule" => (
            "Scheduling",
            "Scheduled",
            format!(
                "{}: {}",
                cron_schedule(Some(input.get("interval_seconds")?.as_i64()?), None)?,
                cron_prompt_summary(nonempty(input, "prompt")?)
            ),
        ),
        "schedule_cron" => (
            "Scheduling",
            "Scheduled",
            format!(
                "{}: {}",
                cron_schedule(None, Some(nonempty(input, "cron")?))?,
                cron_prompt_summary(nonempty(input, "prompt")?)
            ),
        ),
        "list" => ("Listing", "Listed", "scheduled prompts".to_owned()),
        "cancel" => (
            "Cancelling",
            "Cancelled",
            format!("scheduled prompt {}", nonempty(input, "id")?),
        ),
        "clear" => ("Clearing", "Cleared", "scheduled prompts".to_owned()),
        _ => return None,
    };
    let display = call_display(
        format!("{verb} {message}"),
        verb,
        message.as_str(),
        settled,
        message.as_str(),
        format!("{verb} recurring prompts"),
    );
    let projected = json!({
        "toolName": name,
        "display": display,
        "kind": "tool",
        "input": input,
    });
    Some(Projected {
        detail: projected,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let loops = result.get("loops")?.as_array()?.clone();
            let cleared = result.get("cleared_count").and_then(Value::as_u64);
            let settled_message = match action.as_str() {
                "list" => {
                    let count = loops.len();
                    format!(
                        "{count} scheduled prompt{}",
                        if count == 1 { "" } else { "s" }
                    )
                }
                "cancel" if !loops.is_empty() => {
                    cron_prompt_summary(loops[0]["prompt"].as_str().unwrap_or_default())
                }
                "clear" => cleared.map_or_else(
                    || "scheduled prompts".to_owned(),
                    |count| {
                        format!(
                            "{count} scheduled loop{}",
                            if count == 1 { "" } else { "s" }
                        )
                    },
                ),
                _ => message.clone(),
            };
            let body = cron_result_text(result, &loops, cleared, &action)?;
            let mut updated = state.clone();
            updated["display"]["verb"] = json!(settled);
            updated["display"]["message"] = json!(settled_message);
            updated["output"] = json!(body);
            updated["outputText"] = json!(body);
            Some(updated)
        })),
    })
}

fn cron_result_text(
    result: &Map<String, Value>,
    loops: &[Value],
    cleared: Option<u64>,
    action: &str,
) -> Option<String> {
    if action == "clear" {
        return Some(cleared.map_or_else(
            || {
                result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            },
            |count| {
                format!(
                    "Cleared {count} scheduled loop{}",
                    if count == 1 { "" } else { "s" }
                )
            },
        ));
    }
    if loops.is_empty() {
        return Some("No scheduled prompts.".to_owned());
    }
    let mut sections = Vec::new();
    for entry in loops {
        let schedule = cron_schedule(
            entry.get("interval_seconds").and_then(Value::as_i64),
            entry.get("cron").and_then(Value::as_str),
        )?;
        let prompt = entry.get("prompt")?.as_str()?.replace('\n', "\n  ");
        let mut lines = vec![format!("Prompt: {prompt}"), format!("Schedule: {schedule}")];
        if action != "cancel" {
            let next = entry.get("next_fire_at")?.as_f64()?;
            lines.push(format!(
                "Next run: {}",
                crate::clock::local_timestamp(next)?
            ));
        }
        lines.push(format!("ID: {}", entry.get("id")?.as_str()?));
        sections.push(lines.join("\n"));
    }
    Some(sections.join("\n\n"))
}
