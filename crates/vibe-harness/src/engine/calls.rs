//! The calls a model answer asks for, each checked against the catalog into
//! what answers it, and what the model reads back of a result.

use serde_json::{Value, json};

use super::TurnEnvironment;
use crate::catalog::{self, ModelTool};
use crate::discovery::SearchRequest;
use crate::state::ToolResult;

/// A call the model asked for, checked against the catalog.
#[derive(Debug, Clone)]
pub(super) struct PreparedCall {
    pub(super) call_id: String,
    pub(super) model_name: String,
    /// The arguments as the next request replays them: what the model sent,
    /// or `{}` when that was not a JSON object.
    pub(super) wire_arguments: String,
    pub(super) plan: CallPlan,
}

/// What answers one call.
#[derive(Debug, Clone)]
pub(super) enum CallPlan {
    /// A harness tool, by its harness name, with the arguments it takes.
    Harness {
        name: &'static str,
        arguments: Value,
    },
    /// `run_typescript`, with the program.
    Program(String),
    /// `search_tool_functions`, which the engine answers itself.
    Search(SearchRequest),
    /// A call that cannot run; the model reads why as its result.
    Invalid(String),
}

pub(super) fn prepare_calls(
    calls: &[(String, String, String)],
    environment: &TurnEnvironment,
) -> Vec<PreparedCall> {
    let skills: Vec<&str> = environment
        .skills
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, raw))| {
            let call_id = if id.is_empty() {
                format!("tool_call_{index}")
            } else {
                id.clone()
            };
            let raw = if raw.is_empty() { "{}" } else { raw.as_str() };
            let parsed: Result<Value, String> =
                serde_json::from_str(raw).map_err(|error| error.to_string());
            let wire_arguments = match &parsed {
                Ok(Value::Object(_)) => raw.to_owned(),
                _ => "{}".to_owned(),
            };
            PreparedCall {
                call_id,
                model_name: name.clone(),
                wire_arguments,
                plan: plan(name, parsed, &environment.tools, &skills)
                    .unwrap_or_else(CallPlan::Invalid),
            }
        })
        .collect()
}

/// What answers the call `name` with the arguments the model sent, parsed,
/// or why it cannot run.
fn plan(
    name: &str,
    parsed: Result<Value, String>,
    tools: &[ModelTool],
    skills: &[&str],
) -> Result<CallPlan, String> {
    let tool = tools
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| format!("unsupported top-level tool {name:?}"))?;
    let arguments = parsed?;
    if !arguments.is_object() {
        return Err("tool arguments must be a JSON object".to_owned());
    }
    // Reference `tool_discovery::dispatch` reads the search arguments itself,
    // before any schema check.
    if name == "search_tool_functions" {
        return SearchRequest::parse(&arguments).map(CallPlan::Search);
    }
    validate(&tool.parameters, &arguments)?;
    if name == "run_typescript" {
        let code = arguments["code"].as_str().unwrap_or_default().to_owned();
        return Ok(CallPlan::Program(code));
    }
    if name == "skill" {
        crate::skills::check_request(skills, &arguments)?;
    }
    let harness = catalog::harness_name(name)
        .ok_or_else(|| format!("unsupported top-level tool {name:?}"))?;
    Ok(CallPlan::Harness {
        name: harness,
        arguments: catalog::harness_arguments(name, arguments),
    })
}

fn validate(schema: &Value, arguments: &Value) -> Result<(), String> {
    let validator = jsonschema::draft7::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|error| format!("invalid Core-owned JSON Schema: {error}"))?;
    validator.validate(arguments).map_err(|error| {
        format!("tool arguments do not satisfy the declared input schema: {error}")
    })
}

pub(super) fn invalid_call_content(message: &str) -> String {
    compact(&json!({
        "result": {
            "type": "error",
            "error": {"name": "InvalidToolCall", "message": message},
        },
    }))
}

/// What the model reads of a tool result: its text, else its structured
/// content with sorted keys, else the failure.
#[must_use]
pub fn model_content(result: &ToolResult) -> String {
    match result {
        ToolResult::Success {
            content,
            structured_content,
            ..
        } => {
            let texts: Vec<&str> = content
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect();
            if !texts.is_empty() {
                return texts.join("\n\n");
            }
            structured_content
                .as_ref()
                .map_or_else(String::new, |value| compact(&sorted(value)))
        }
        ToolResult::Failure { content, .. }
            if content.iter().any(|block| block["text"].is_string()) =>
        {
            content
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        ToolResult::Failure {
            code,
            message,
            details,
            ..
        } => compact(&json!({
            "result": {
                "type": "error",
                "error": {"name": code, "message": message, "details": details},
            },
        })),
    }
}

pub(super) fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// A JSON value with every object's keys in sorted order.
#[must_use]
pub(crate) fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&map[key])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}
