//! The functions a `run_typescript` program reaches through `tools.<group>`.
//!
//! Reference `features/programmatic_tool_calling/tools.rs` assembles them in
//! a fixed order: the `file_system` group, `self.sleep`, the background
//! process group, the subagent group, then the groups the app server provides
//! (`vibe.todo` only: the scratchpad, cron and skills stay direct). The
//! argument and result schemas are the reference's, as its schema generator
//! emits them (`features/file_system/mod.rs`,
//! `features/background_processes/tools.rs`, `features/subagents/tools.rs`);
//! the descriptions and the connector guidance are this port's own prose.

use std::borrow::Cow;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::catalog;
use crate::discovery::Function;
use crate::state::ToolResult;

/// One function a program can call.
#[derive(Debug, Clone)]
pub struct Callable {
    pub group: Cow<'static, str>,
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
    pub input: Value,
    pub output: Value,
    /// The harness tool a call runs.
    pub runtime: Cow<'static, str>,
    /// What the group tells a model about itself, for a group the host
    /// provides; the harness's own groups describe themselves.
    pub guidance: Option<String>,
}

impl Callable {
    /// `group.name`, the name a program's call records.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.group, self.name)
    }

    /// The arguments the harness tool takes for the ones the program passed,
    /// which already satisfy `input`.
    #[must_use]
    pub fn runtime_arguments(&self, arguments: Value) -> Value {
        if self.runtime == "file_system.search_replace" {
            return catalog::harness_arguments("edit", arguments);
        }
        arguments
    }
}

/// Runs the calls a program makes to the groups the host provides, by their
/// `group.tool` route: reference `RustProvidedToolCallAction`, which the
/// host's provided-tool executor answers (`_host.py`
/// `execute_provided_tool`).
pub trait ProvidedTools: Send + Sync {
    /// Whether `route` is one of the host's.
    fn routes(&self, route: &str) -> bool;

    /// Runs `route` with `arguments`, which already fit its input schema.
    fn call(&self, route: &str, arguments: Value) -> BoxFuture<'static, ToolResult>;
}

/// Every function, in the order the reference offers them.
#[must_use]
pub fn inventory() -> Vec<Callable> {
    let process_status = json!({"type": "string", "enum": ["running", "completed", "failed", "stopped", "orphaned"]});
    let nullable_code = json!({"type": ["integer", "null"]});
    let operation = json!({"oneOf": [
        {"type": "object", "properties": {"type": {"const": "success"}}, "required": ["type"], "additionalProperties": false},
        {"type": "object", "properties": {"error": {"type": "string"}, "type": {"const": "error"}}, "required": ["error", "type"], "additionalProperties": false},
    ]});
    let named_agent = json!({
        "type": "object",
        "properties": {"agentName": {"type": "string", "minLength": 1}},
        "required": ["agentName"],
        "additionalProperties": false,
    });
    let listed_agent = |status: &str, pending: bool, failed: bool| {
        let mut properties = json!({
            "agentName": {"type": "string"},
            "agentType": {"type": ["string", "null"]},
            "status": {"const": status},
            "pendingTurn": {"const": pending},
        });
        let mut required = vec!["agentName", "status", "pendingTurn"];
        if failed {
            properties["error"] = json!({"type": "string"});
            required.push("error");
        }
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    };
    let direct = |name: &str| {
        catalog::model_tools()
            .into_iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.parameters)
            .unwrap_or(Value::Null)
    };
    vec![
        Callable {
            group: Cow::Borrowed("file_system"),
            name: Cow::Borrowed("bash"),
            description: Cow::Borrowed(
                "Run a shell command in the working directory and return its exit code and output.",
            ),
            input: direct("bash"),
            output: object(&[
                ("command", json!({"type": "string"})),
                ("stdout", json!({"type": "string"})),
                ("stderr", json!({"type": "string"})),
                ("returncode", json!({"type": "integer"})),
                ("was_truncated", json!({"type": "boolean"})),
            ]),
            runtime: Cow::Borrowed("file_system.bash"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("file_system"),
            name: Cow::Borrowed("read_file"),
            description: Cow::Borrowed(
                "Read a text file, optionally from a zero-based line offset and up to a number of lines.",
            ),
            input: direct("read_file"),
            output: object(&[
                ("path", json!({"type": "string"})),
                ("content", json!({"type": "string"})),
                ("file_size_bytes", json!({"type": "integer"})),
                ("returned_bytes", json!({"type": "integer"})),
                ("offset", json!({"type": "integer"})),
                ("lines_read", json!({"type": "integer"})),
                ("was_truncated", json!({"type": "boolean"})),
            ]),
            runtime: Cow::Borrowed("file_system.read_file"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("file_system"),
            name: Cow::Borrowed("write_file"),
            description: Cow::Borrowed("Create a UTF-8 text file or replace its contents."),
            input: direct("write_file"),
            output: object(&[
                ("path", json!({"type": "string"})),
                ("bytes_written", json!({"type": "integer"})),
                ("file_existed", json!({"type": "boolean"})),
            ]),
            runtime: Cow::Borrowed("file_system.write_file"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("file_system"),
            name: Cow::Borrowed("edit"),
            description: Cow::Borrowed(
                "Replace an exact piece of text in a file. Read the file first: old_string must match it character for character and occur once unless replace_all is set.",
            ),
            input: direct("edit"),
            output: object(&[
                ("file", json!({"type": "string"})),
                ("lines_changed", json!({"type": "integer"})),
                (
                    "warnings",
                    json!({"type": "array", "items": {"type": "string"}}),
                ),
            ]),
            runtime: Cow::Borrowed("file_system.search_replace"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("self"),
            name: Cow::Borrowed("sleep"),
            description: Cow::Borrowed(
                "Pause for a few seconds while background processes or subagents keep working.",
            ),
            input: json!({
                "type": "object",
                "properties": {"seconds": {
                    "type": "number",
                    "exclusiveMinimum": 0,
                    "exclusiveMaximum": 60,
                    "description": "How long to pause, in seconds.",
                }},
                "required": ["seconds"],
                "additionalProperties": false,
            }),
            output: object(&[("seconds", json!({"type": "number"}))]),
            runtime: Cow::Borrowed("self.sleep"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("process"),
            name: Cow::Borrowed("start"),
            description: Cow::Borrowed(
                "Start a command in the background and return its process ID at once.",
            ),
            input: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "minLength": 1},
                    "cwd": {"type": "string", "default": "", "minLength": 1},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}, "default": {}},
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
            output: object(&[
                ("processId", json!({"type": "string"})),
                ("status", process_status.clone()),
            ]),
            runtime: Cow::Borrowed("process.start"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("process"),
            name: Cow::Borrowed("output"),
            description: Cow::Borrowed(
                "Read the terminal output of a background process by byte cursor, from the start or the newest end, optionally waiting for more.",
            ),
            input: json!({
                "type": "object",
                "properties": {
                    "processId": {"type": "string", "minLength": 1},
                    "from": {"type": "string", "enum": ["start", "end"], "default": "start"},
                    "cursor": {"type": "integer", "default": 0, "minimum": 0},
                    "waitMs": {"type": "integer", "default": 0, "minimum": 0, "maximum": 30000},
                    "maxBytes": {"type": "integer", "default": 16000, "minimum": 1},
                },
                "required": ["processId"],
                "additionalProperties": false,
            }),
            output: json!({
                "type": "object",
                "properties": {
                    "processId": {"type": "string"},
                    "status": process_status.clone(),
                    "exitCode": nullable_code.clone(),
                    "output": {"type": "string"},
                    "outputStartCursor": {"type": ["integer", "null"], "minimum": 0},
                    "nextCursor": {"type": "integer", "minimum": 0},
                    "bytesAvailable": {"type": "integer", "minimum": 0},
                    "hasMore": {"type": "boolean", "description": "More retained output lies beyond this page."},
                    "truncatedBefore": {"type": "boolean", "description": "Part of the requested range was dropped before it could be read."},
                },
                "required": ["processId", "status", "exitCode", "output", "nextCursor", "bytesAvailable", "hasMore", "truncatedBefore"],
                "additionalProperties": false,
            }),
            runtime: Cow::Borrowed("process.output"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("process"),
            name: Cow::Borrowed("write"),
            description: Cow::Borrowed(
                "Send one payload to a running background process: text, control keys, or base64 bytes.",
            ),
            input: json!({"oneOf": [
                {
                    "type": "object",
                    "properties": {"processId": {"type": "string", "minLength": 1}, "text": {"type": "string"}},
                    "required": ["processId", "text"],
                    "additionalProperties": false,
                },
                {
                    "type": "object",
                    "properties": {
                        "processId": {"type": "string", "minLength": 1},
                        "control": {
                            "type": "array",
                            "items": {"type": "string", "enum": ["ctrl_c", "ctrl_d", "ctrl_z", "esc", "tab", "enter", "backspace", "up", "down", "left", "right"]},
                            "minItems": 1,
                        },
                    },
                    "required": ["processId", "control"],
                    "additionalProperties": false,
                },
                {
                    "type": "object",
                    "properties": {
                        "processId": {"type": "string", "minLength": 1},
                        "bytesBase64": {"type": "string", "minLength": 1},
                    },
                    "required": ["processId", "bytesBase64"],
                    "additionalProperties": false,
                },
            ]}),
            output: json!({
                "type": "object",
                "properties": {
                    "processId": {"type": "string"},
                    "status": process_status.clone(),
                    "bytesWritten": {"type": "integer", "minimum": 0},
                },
                "required": ["processId", "status", "bytesWritten"],
                "additionalProperties": false,
            }),
            runtime: Cow::Borrowed("process.write"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("process"),
            name: Cow::Borrowed("list"),
            description: Cow::Borrowed(
                "List this session's background processes and where each stands.",
            ),
            input: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            output: object(&[(
                "processes",
                json!({"type": "array", "items": object(&[
                    ("processId", json!({"type": "string"})),
                    ("command", json!({"type": "string"})),
                    ("status", process_status.clone()),
                    ("exitCode", nullable_code.clone()),
                    ("outputPath", json!({"type": ["string", "null"]})),
                ])}),
            )]),
            runtime: Cow::Borrowed("process.list"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("process"),
            name: Cow::Borrowed("stop"),
            description: Cow::Borrowed(
                "Stop a background process together with the processes it started.",
            ),
            input: json!({
                "type": "object",
                "properties": {"processId": {"type": "string", "minLength": 1}},
                "required": ["processId"],
                "additionalProperties": false,
            }),
            output: object(&[
                ("processId", json!({"type": "string"})),
                ("status", process_status),
                ("exitCode", nullable_code),
            ]),
            runtime: Cow::Borrowed("process.stop"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("subagent"),
            name: Cow::Borrowed("list"),
            description: Cow::Borrowed("List this session's subagents and the state of each."),
            input: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            output: object(&[(
                "agents",
                json!({"type": "array", "items": {"oneOf": [
                    listed_agent("running", true, false),
                    listed_agent("idle", false, false),
                    listed_agent("stopped", false, false),
                    listed_agent("failed", false, true),
                ]}}),
            )]),
            runtime: Cow::Borrowed("subagent.list"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("subagent"),
            name: Cow::Borrowed("spawn"),
            description: Cow::Borrowed(
                "Create a subagent and start its first turn without waiting for it. Each agentName can be used once per session, even after that subagent is closed.",
            ),
            input: json!({
                "type": "object",
                "properties": {
                    "agentName": {"type": "string", "minLength": 1},
                    "message": {"type": "string", "minLength": 1},
                    "agentType": {"type": "string", "default": "", "minLength": 1},
                },
                "required": ["agentName", "message"],
                "additionalProperties": false,
            }),
            output: operation.clone(),
            runtime: Cow::Borrowed("subagent.spawn"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("subagent"),
            name: Cow::Borrowed("wait"),
            description: Cow::Borrowed(
                "Wait for a subagent's current turn to end and return its final answer.",
            ),
            input: json!({
                "type": "object",
                "properties": {
                    "agentName": {"type": "string", "minLength": 1},
                    "timeoutMs": {"type": "integer", "minimum": 1},
                },
                "required": ["agentName", "timeoutMs"],
                "additionalProperties": false,
            }),
            output: json!({"oneOf": [
                {"type": "object", "properties": {"value": {"type": "string"}, "type": {"const": "success"}}, "required": ["value", "type"], "additionalProperties": false},
                {"type": "object", "properties": {"error": {"type": "string"}, "type": {"const": "error"}}, "required": ["error", "type"], "additionalProperties": false},
            ]}),
            runtime: Cow::Borrowed("subagent.wait"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("subagent"),
            name: Cow::Borrowed("sendMessage"),
            description: Cow::Borrowed(
                "Give a subagent another message: it steers the running turn, or starts the next one.",
            ),
            input: json!({
                "type": "object",
                "properties": {
                    "agentName": {"type": "string", "minLength": 1},
                    "message": {"type": "string", "minLength": 1},
                },
                "required": ["agentName", "message"],
                "additionalProperties": false,
            }),
            output: operation.clone(),
            runtime: Cow::Borrowed("subagent.send_message"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("subagent"),
            name: Cow::Borrowed("interrupt"),
            description: Cow::Borrowed("Stop a subagent's running turn and keep the subagent."),
            input: named_agent.clone(),
            output: operation.clone(),
            runtime: Cow::Borrowed("subagent.interrupt"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("subagent"),
            name: Cow::Borrowed("close"),
            description: Cow::Borrowed(
                "Close a subagent for good and free what it holds. Its name stays taken.",
            ),
            input: named_agent,
            output: operation,
            runtime: Cow::Borrowed("subagent.stop"),
            guidance: None,
        },
        Callable {
            group: Cow::Borrowed("vibe"),
            name: Cow::Borrowed("todo"),
            description: Cow::Borrowed("Read or replace this session's task list."),
            input: direct("todo"),
            output: todo_result_schema(),
            runtime: Cow::Borrowed("vibe.todo"),
            guidance: None,
        },
    ]
}

/// An object schema whose properties are all required.
fn object(properties: &[(&str, Value)]) -> Value {
    json!({
        "type": "object",
        "properties": properties.iter().map(|(name, schema)| ((*name).to_owned(), schema.clone())).collect::<serde_json::Map<_, _>>(),
        "required": properties.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
    })
}

fn todo_result_schema() -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "verb": {"type": "string"},
            "todos": {"type": "array", "items": {"$ref": "#/$defs/TodoItem"}},
            "total_count": {"type": "integer"},
        },
        "required": ["verb", "todos", "total_count"],
    });
    let input = catalog::model_tools()
        .into_iter()
        .find(|tool| tool.name == "todo")
        .map(|tool| tool.parameters)
        .unwrap_or(Value::Null);
    schema["$defs"] = input["$defs"].clone();
    schema
}

/// What each group tells a model about itself.
fn guidance(group: &str) -> &'static str {
    match group {
        "file_system" => "Files and shell commands in the workspace: read, write, edit and run.",
        "process" => "Commands that keep running in the background, and their output.",
        "self" => "Pacing this agent itself, such as pausing while background work runs.",
        "subagent" => "Subagents this session creates and directs.",
        "vibe" => "Tools Vibe provides itself.",
        _ => "",
    }
}

/// The documents `search_tool_functions` searches.
#[must_use]
pub fn documents(callables: &[Callable]) -> Vec<Function> {
    callables
        .iter()
        .map(|callable| {
            Function::new(
                &callable.group,
                callable
                    .guidance
                    .as_deref()
                    .unwrap_or_else(|| guidance(&callable.group)),
                &callable.name,
                &callable.description,
                &callable.input,
                Some(&callable.output).filter(|output| !output.is_null()),
            )
        })
        .collect()
}

/// The groups and their functions, as the sandbox exposes them.
#[must_use]
pub fn namespaces(callables: &[Callable]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for callable in callables {
        match groups
            .iter_mut()
            .find(|(group, _)| *group == callable.group)
        {
            Some((_, functions)) => functions.push(callable.name.to_string()),
            None => groups.push((callable.group.to_string(), vec![callable.name.to_string()])),
        }
    }
    groups
}

/// Why `arguments` do not fit `callable`, shaped as Ajv reports it, or
/// `None` when they do. Only the top-level `properties`, `required` and
/// `additionalProperties` are checked, as the reference does.
#[must_use]
pub fn argument_error(callable: &Callable, arguments: &Value) -> Option<Value> {
    let mut schema = serde_json::Map::new();
    schema.insert(
        "$schema".to_owned(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
    schema.insert("type".to_owned(), json!("object"));
    for keyword in ["properties", "required", "additionalProperties"] {
        if let Some(value) = callable.input.get(keyword) {
            schema.insert(keyword.to_owned(), value.clone());
        }
    }
    let validator = jsonschema::draft7::options()
        .should_validate_formats(true)
        .build(&Value::Object(schema))
        .ok()?;
    let error = validator.validate(arguments).err()?;
    Some(json!({
        "type": "jsonSchemaArgumentValidationError",
        "functionName": callable.qualified(),
        "argumentsValidationErrors": ajv_errors(&error),
    }))
}

fn ajv_errors(error: &jsonschema::ValidationError<'_>) -> Vec<Value> {
    use jsonschema::error::{TypeKind, ValidationErrorKind as Kind};
    let instance_path = error.instance_path().to_string();
    let schema_path = format!("#{}", error.schema_path());
    let keyword = error.kind().keyword();
    let entry = |params: Value, message: String| {
        json!({
            "instancePath": instance_path,
            "schemaPath": schema_path,
            "keyword": keyword,
            "params": params,
            "message": message,
        })
    };
    let (params, message) = match error.kind() {
        Kind::AdditionalProperties { unexpected } => {
            return unexpected
                .iter()
                .take(1)
                .map(|property| {
                    entry(
                        json!({"additionalProperty": property}),
                        "must NOT have additional properties".to_owned(),
                    )
                })
                .collect();
        }
        Kind::Required { property } => (
            json!({"missingProperty": property}),
            format!(
                "must have required property '{}'",
                property.as_str().unwrap_or_default()
            ),
        ),
        Kind::Constant { expected_value } => (
            json!({"allowedValue": expected_value}),
            "must be equal to constant".to_owned(),
        ),
        Kind::Enum { options } => (
            json!({"allowedValues": options}),
            "must be equal to one of the allowed values".to_owned(),
        ),
        Kind::ExclusiveMaximum { limit } => (
            json!({"comparison": "<", "limit": limit}),
            format!("must be < {limit}"),
        ),
        Kind::ExclusiveMinimum { limit } => (
            json!({"comparison": ">", "limit": limit}),
            format!("must be > {limit}"),
        ),
        Kind::Format { format } => (
            json!({"format": format}),
            format!("must match format \"{format}\""),
        ),
        Kind::Maximum { limit } => (
            json!({"comparison": "<=", "limit": limit}),
            format!("must be <= {limit}"),
        ),
        Kind::Minimum { limit } => (
            json!({"comparison": ">=", "limit": limit}),
            format!("must be >= {limit}"),
        ),
        Kind::MaxItems { limit } => (
            json!({"limit": limit}),
            format!("must NOT have more than {limit} items"),
        ),
        Kind::MinItems { limit } => (
            json!({"limit": limit}),
            format!("must NOT have fewer than {limit} items"),
        ),
        Kind::MaxLength { limit } => (
            json!({"limit": limit}),
            format!("must NOT have more than {limit} characters"),
        ),
        Kind::MinLength { limit } => (
            json!({"limit": limit}),
            format!("must NOT have fewer than {limit} characters"),
        ),
        Kind::MaxProperties { limit } => (
            json!({"limit": limit}),
            format!("must NOT have more than {limit} properties"),
        ),
        Kind::MinProperties { limit } => (
            json!({"limit": limit}),
            format!("must NOT have fewer than {limit} properties"),
        ),
        Kind::MultipleOf { multiple_of } => (
            json!({"multipleOf": multiple_of}),
            format!("must be multiple of {multiple_of}"),
        ),
        Kind::Pattern { pattern } => (
            json!({"pattern": pattern}),
            format!("must match pattern \"{pattern}\""),
        ),
        Kind::Type { kind } => {
            let expected: Vec<String> = match kind {
                TypeKind::Single(single) => vec![single.to_string()],
                TypeKind::Multiple(several) => several
                    .into_iter()
                    .map(|single| single.to_string())
                    .collect(),
            };
            let params = if let [single] = expected.as_slice() {
                json!({"type": single})
            } else {
                json!({"type": expected})
            };
            (params, format!("must be {}", expected.join(",")))
        }
        _ => (json!({}), error.to_string()),
    };
    vec![entry(params, message)]
}

/// What a program's call settles to: the value it resolves with, or the
/// error it rejects with.
///
/// # Errors
///
/// The tool failed: `{name, message, details}`, plus the structured content
/// it carried.
pub fn settled_value(result: &ToolResult) -> Result<Value, Value> {
    match result {
        ToolResult::Success {
            content,
            structured_content,
            ..
        } => {
            if let Some(value) = structured_content.as_ref().filter(|value| !value.is_null()) {
                return Ok(crate::engine::sorted(value));
            }
            let text: String = content
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect();
            if !text.is_empty() {
                return Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)));
            }
            Ok(Value::Array(content.clone()))
        }
        ToolResult::Failure {
            code,
            message,
            details,
            ..
        } => Err(json!({"name": code, "message": message, "details": details})),
    }
}

#[cfg(test)]
mod programmatic_tests;
