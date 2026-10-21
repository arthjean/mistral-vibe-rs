//! Typed effect details for the calls the harness reports generically.
//!
//! The harness publishes every tool call under its namespaced key with a
//! generic detail (`file_system.read_file`, `vibe.todo`); the adapter rewrites
//! the ones a client renders richly into their typed detail and settles their
//! results into the matching output models. Reference
//! `vibe/app_server/_unified_tool_projection.py`, over the wire models of
//! `vibe/app_server/_effect_models.py`.
//!
//! This module dispatches each call to its family's projection and holds the
//! shapes they share; a namespaced tool with no richer projection only gets
//! its human label.

use serde_json::{Map, Value, json};

use files::{project_read, project_search_replace, project_shell, project_skill, project_write};
use processes::{
    project_process_list, project_process_output, project_process_start, project_process_stop,
    project_process_write,
};
use questions::project_question;
use search::{project_grep, project_web_fetch, project_web_search};
use session::{project_cron, project_scratchpad, project_todo};

mod files;
mod processes;
mod questions;
mod search;
mod session;

/// How a projected call's result settles.
type SettleResult = Box<dyn Fn(&Value) -> Option<Value>>;

/// A call rewritten into its typed detail, and how its result settles.
struct Projected {
    detail: Value,
    result: Option<SettleResult>,
}

/// Re-projects one wire effect entry (already in wire shape, detail generic).
/// An entry whose call this surface does not know, or whose arguments do not
/// fit, stays as it is.
pub fn project_effect(entry: &mut Value) {
    let Some(projected) = project_call(&entry["detail"]) else {
        return;
    };
    if entry["state"]["status"] == "completed"
        && let Some(settle) = &projected.result
    {
        match settle(&entry["state"]) {
            Some(state) => entry["state"] = state,
            None => return,
        }
    }
    entry["detail"] = projected.detail;
}

/// Re-projects the effect an approval callback describes.
pub fn project_callback_effect(effect: &mut Value) {
    if let Some(projected) = project_call(effect) {
        *effect = projected.detail;
    }
}

fn project_call(detail: &Value) -> Option<Projected> {
    if detail["kind"] != "tool" {
        return None;
    }
    let name = detail["toolName"].as_str()?;
    let input = &detail["input"];
    let projector: Option<fn(&str, &Value) -> Option<Projected>> = match name {
        "read_file" | "file_system.read_file" => Some(project_read),
        "write_file" | "file_system.write_file" => Some(project_write),
        "search_replace" | "file_system.search_replace" => Some(project_search_replace),
        "bash" | "file_system.bash" => Some(project_shell),
        "skill.read" => Some(project_skill),
        "process.start" => Some(project_process_start),
        "process.stop" => Some(project_process_stop),
        "process.output" => Some(project_process_output),
        "process.list" => Some(project_process_list),
        "process.write" => Some(project_process_write),
        "ui.ask_user_question" => Some(project_question),
        "web_search" => Some(project_web_search),
        "web_fetch" => Some(project_web_fetch),
        "grep" => Some(project_grep),
        "todo" | "vibe.todo" => Some(project_todo),
        "vibe.cron" => Some(project_cron),
        "vibe.unified_harness_scratchpad" => Some(project_scratchpad),
        _ => None,
    };
    if let Some(projector) = projector {
        return projector(name, input);
    }
    label(name).map(|(verb, noun, settled)| project_labeled(detail, verb, noun, settled))
}

// --------------------------------------------------------------------------
// Shapes
// --------------------------------------------------------------------------

fn call_display(
    summary: impl Into<String>,
    verb: &str,
    message: impl Into<String>,
    settled_verb: &str,
    settled_message: impl Into<String>,
    status_text: impl Into<String>,
) -> Value {
    json!({
        "summary": summary.into(),
        "content": null,
        "suffix": "",
        "verb": verb,
        "message": message.into(),
        "settledVerb": settled_verb,
        "settledMessage": settled_message.into(),
        "statusText": status_text.into(),
    })
}

/// Reference `EffectResultDisplay` with every default.
#[must_use]
pub fn result_display(success: bool, verb: &str, message: impl Into<String>) -> Value {
    json!({
        "success": success,
        "verb": verb,
        "message": message.into(),
        "warnings": [],
        "approvalNote": null,
        "suffix": "",
    })
}

fn detail(kind: &str, tool_name: &str, input: Value, display: Value) -> Value {
    json!({"toolName": tool_name, "display": display, "kind": kind, "input": input})
}

fn structured(state: &Value) -> Option<&Map<String, Value>> {
    state["output"]["structured_content"].as_object()
}

fn meta(state: &Value) -> Option<&Map<String, Value>> {
    state["output"]["_meta"].as_object()
}

/// Reference `_completed_state`: the projected output and display, carrying
/// over the approval note and warnings the incoming display had.
fn completed_state(
    state: &Value,
    output: Value,
    mut display: Value,
    output_text: Option<String>,
) -> Value {
    let incoming = &state["display"];
    let mut warnings: Vec<Value> = display["warnings"].as_array().cloned().unwrap_or_default();
    for warning in incoming["warnings"].as_array().into_iter().flatten() {
        if !warnings.contains(warning) {
            warnings.push(warning.clone());
        }
    }
    display["warnings"] = Value::Array(warnings);
    if incoming["approvalNote"].is_string() {
        display["approvalNote"] = incoming["approvalNote"].clone();
    }
    let mut updated = state.clone();
    updated["output"] = output;
    updated["display"] = display;
    if let Some(text) = output_text {
        updated["outputText"] = json!(text);
    }
    updated
}

fn str_field<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn nonempty<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    str_field(object, key).filter(|value| !value.is_empty())
}

fn plural(count: u64, singular: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {many}")
    }
}

// --------------------------------------------------------------------------
// Labels
// --------------------------------------------------------------------------

fn project_labeled(
    detail: &Value,
    verb: &'static str,
    noun: &'static str,
    settled: &'static str,
) -> Projected {
    let tool_name = detail["toolName"].as_str().unwrap_or_default().to_owned();
    let summary = format!("{verb} {noun}").trim().to_owned();
    let mut projected = detail.clone();
    projected["display"] = call_display(summary.clone(), verb, noun, settled, noun, summary);
    Projected {
        detail: projected,
        result: Some(Box::new(move |state| {
            if state["display"]["message"] != json!(tool_name) {
                return Some(state.clone());
            }
            let mut updated = state.clone();
            updated["display"]["verb"] = json!(settled);
            updated["display"]["message"] = json!(noun);
            Some(updated)
        })),
    }
}

/// The human header of a namespaced tool with no richer projection.
fn label(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match name {
        "ui.ask_user_question" => ("Asking", "a question", "Asked"),
        "self.sleep" => ("Sleeping", "", "Slept"),
        "subagent.list" => ("Listing", "subagents", "Listed"),
        "subagent.wait" => ("Waiting", "for a subagent", "Waited"),
        "subagent.send_message" => ("Sending", "a message to a subagent", "Sent"),
        "subagent.interrupt" => ("Interrupting", "a subagent", "Interrupted"),
        "subagent.stop" => ("Stopping", "a subagent", "Stopped"),
        _ => return crate::connector_labels::label(name),
    })
}
