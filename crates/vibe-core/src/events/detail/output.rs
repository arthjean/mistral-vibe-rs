//! The settled output of an effect, in the shape its kind declares.
//!
//! Reference `project_effect_output_value` (`vibe/app_server/_tool_projection.py`)
//! reads a tool's result through the output model of the effect kind
//! (`vibe/app_server/_effect_models.py`): a top-level field is taken under its
//! Python name or its camelCase alias, anything else is dropped, and the model
//! is published under its aliases. A result that does not validate publishes no
//! output at all. A kind with no output model publishes the result unchanged.

use serde_json::{Map, Value, json};

use super::ToolEffectKind;

/// The type a field validates as, in pydantic's lax mode.
#[derive(Clone, Copy)]
enum Kind {
    Str,
    Int,
    Bool,
    OptionalStr,
    OptionalInt,
    Choice(&'static [&'static str]),
    List(&'static [Field]),
}

#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    alias: &'static str,
    kind: Kind,
    /// The value a missing field takes, or `None` for a required one.
    default: Option<fn() -> Value>,
}

const fn required(name: &'static str, alias: &'static str, kind: Kind) -> Field {
    Field {
        name,
        alias,
        kind,
        default: None,
    }
}

const fn optional(
    name: &'static str,
    alias: &'static str,
    kind: Kind,
    default: fn() -> Value,
) -> Field {
    Field {
        name,
        alias,
        kind,
        default: Some(default),
    }
}

fn null() -> Value {
    Value::Null
}
fn empty() -> Value {
    json!("")
}
fn no() -> Value {
    json!(false)
}
fn nothing() -> Value {
    json!([])
}
fn read_limit() -> Value {
    json!(2000)
}
fn pending() -> Value {
    json!("pending")
}
fn medium() -> Value {
    json!("medium")
}

const SHELL: &[Field] = &[
    required("stdout", "stdout", Kind::Str),
    required("stderr", "stderr", Kind::Str),
    optional("output", "output", Kind::Str, empty),
    optional("truncated", "truncated", Kind::Bool, no),
];

const OCCURRENCE: &[Field] = &[
    optional("start_line", "startLine", Kind::OptionalInt, null),
    required("old_text", "oldText", Kind::Str),
    required("new_text", "newText", Kind::Str),
];

const FILE_EDIT: &[Field] = &[
    required("file", "file", Kind::Str),
    optional("old_string", "oldString", Kind::OptionalStr, null),
    optional("new_string", "newString", Kind::OptionalStr, null),
    optional(
        "occurrences",
        "occurrences",
        Kind::List(OCCURRENCE),
        nothing,
    ),
];

const SEARCH_MATCH: &[Field] = &[
    required("path", "path", Kind::Str),
    optional("line", "line", Kind::OptionalInt, null),
];

const FILE_SEARCH: &[Field] = &[
    required("matches", "matches", Kind::Str),
    required("match_count", "matchCount", Kind::Int),
    required("was_truncated", "wasTruncated", Kind::Bool),
    optional(
        "parsed_matches",
        "parsedMatches",
        Kind::List(SEARCH_MATCH),
        nothing,
    ),
];

const FILE_READ: &[Field] = &[
    required("file_path", "filePath", Kind::Str),
    required("content", "content", Kind::Str),
    required("num_lines", "numLines", Kind::Int),
    required("start_line", "startLine", Kind::Int),
    optional(
        "requested_offset",
        "requestedOffset",
        Kind::OptionalInt,
        null,
    ),
    optional(
        "requested_limit",
        "requestedLimit",
        Kind::OptionalInt,
        read_limit,
    ),
    optional("total_lines", "totalLines", Kind::OptionalInt, null),
    optional("was_truncated", "wasTruncated", Kind::Bool, no),
];

const TODO_ITEM: &[Field] = &[
    required("id", "id", Kind::Str),
    required("content", "content", Kind::Str),
    optional(
        "status",
        "status",
        Kind::Choice(&["pending", "in_progress", "completed", "cancelled"]),
        pending,
    ),
    optional(
        "priority",
        "priority",
        Kind::Choice(&["low", "medium", "high"]),
        medium,
    ),
];

const TODO: &[Field] = &[optional("todos", "todos", Kind::List(TODO_ITEM), nothing)];

const FILE_WRITE: &[Field] = &[
    required("file_path", "filePath", Kind::Str),
    required("content", "content", Kind::Str),
];

const ANSWER: &[Field] = &[
    required("question", "question", Kind::Str),
    required("answer", "answer", Kind::Str),
    optional("is_other", "isOther", Kind::Bool, no),
];

const USER_QUESTION: &[Field] = &[
    required("answers", "answers", Kind::List(ANSWER)),
    optional("cancelled", "cancelled", Kind::Bool, no),
];

const SOURCE: &[Field] = &[
    required("title", "title", Kind::Str),
    required("url", "url", Kind::Str),
];

const WEB_SEARCH: &[Field] = &[
    required("query", "query", Kind::Str),
    required("answer", "answer", Kind::Str),
    optional("sources", "sources", Kind::List(SOURCE), nothing),
];

const WEB_FETCH: &[Field] = &[
    required("url", "url", Kind::Str),
    required("content", "content", Kind::Str),
    required("content_type", "contentType", Kind::Str),
    optional("was_truncated", "wasTruncated", Kind::Bool, no),
];

const SKILL: &[Field] = &[
    required("name", "name", Kind::Str),
    required("content", "content", Kind::Str),
    optional("skill_dir", "skillDir", Kind::OptionalStr, null),
];

const SUBAGENT: &[Field] = &[
    required("response", "response", Kind::Str),
    required("turns_used", "turnsUsed", Kind::Int),
    required("completed", "completed", Kind::Bool),
];

/// The output model a kind declares, or `None` for a kind published as is.
const fn model(kind: ToolEffectKind) -> Option<&'static [Field]> {
    match kind {
        ToolEffectKind::Shell => Some(SHELL),
        ToolEffectKind::FileEdit => Some(FILE_EDIT),
        ToolEffectKind::FileSearch => Some(FILE_SEARCH),
        ToolEffectKind::FileRead => Some(FILE_READ),
        ToolEffectKind::Todo => Some(TODO),
        ToolEffectKind::FileWrite => Some(FILE_WRITE),
        ToolEffectKind::UserQuestion => Some(USER_QUESTION),
        ToolEffectKind::WebSearch => Some(WEB_SEARCH),
        ToolEffectKind::WebFetch => Some(WEB_FETCH),
        ToolEffectKind::Skill => Some(SKILL),
        ToolEffectKind::Subagent => Some(SUBAGENT),
        ToolEffectKind::Tool | ToolEffectKind::Worktree | ToolEffectKind::Process => None,
    }
}

/// Whether `kind` reads its output through a model.
#[must_use]
pub const fn has_output_model(kind: ToolEffectKind) -> bool {
    model(kind).is_some()
}

/// Reference `project_effect_output_value`.
#[must_use]
pub fn project_output(kind: ToolEffectKind, value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let Some(fields) = model(kind) else {
        return value.clone();
    };
    let Value::Object(object) = value else {
        return Value::Null;
    };
    // Reference `_project_model`: only the model's own fields are kept, and a
    // non-empty result that shares none of them is foreign data.
    let mut picked = Map::new();
    for field in fields {
        if let Some(found) = object.get(field.name).or_else(|| object.get(field.alias)) {
            picked.insert(field.alias.to_owned(), found.clone());
        }
    }
    if !object.is_empty() && picked.is_empty() {
        return Value::Null;
    }
    let Some(projected) = validate(fields, &picked) else {
        return Value::Null;
    };
    if kind == ToolEffectKind::FileEdit && !valid_edit(&projected) {
        return Value::Null;
    }
    projected
}

/// Reference `ShellEffectOutput.transcript`, read off a projected output.
#[must_use]
pub fn shell_transcript(output: &Value) -> String {
    let text = |key: &str| output.get(key).and_then(Value::as_str).unwrap_or_default();
    let (combined, stdout, stderr) = (text("output"), text("stdout"), text("stderr"));
    if !combined.is_empty() {
        return combined.to_owned();
    }
    if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
        return format!("{stdout}\n{stderr}");
    }
    format!("{stdout}{stderr}")
}

/// Reference `FileEditEffectOutput.validate_diff_content`.
fn valid_edit(output: &Value) -> bool {
    let old = output
        .get("oldString")
        .is_some_and(|value| !value.is_null());
    let new = output
        .get("newString")
        .is_some_and(|value| !value.is_null());
    if old != new {
        return false;
    }
    old || output
        .get("occurrences")
        .and_then(Value::as_array)
        .is_some_and(|occurrences| !occurrences.is_empty())
}

/// One model instance, keyed by alias on the way in and on the way out. The
/// model forbids extra fields, so a nested object carrying one does not
/// validate.
fn validate(fields: &[Field], object: &Map<String, Value>) -> Option<Value> {
    for key in object.keys() {
        if !fields
            .iter()
            .any(|field| field.name == key || field.alias == key)
        {
            return None;
        }
    }
    let mut out = Map::new();
    for field in fields {
        let value = match object.get(field.alias).or_else(|| object.get(field.name)) {
            Some(value) => coerce(field.kind, value)?,
            None => (field.default?)(),
        };
        out.insert(field.alias.to_owned(), value);
    }
    Some(Value::Object(out))
}

fn coerce(kind: Kind, value: &Value) -> Option<Value> {
    match kind {
        Kind::Str => value.as_str().map(|text| json!(text)),
        Kind::Int => integer(value),
        Kind::Bool => boolean(value),
        Kind::OptionalStr if value.is_null() => Some(Value::Null),
        Kind::OptionalStr => value.as_str().map(|text| json!(text)),
        Kind::OptionalInt if value.is_null() => Some(Value::Null),
        Kind::OptionalInt => integer(value),
        Kind::Choice(choices) => value
            .as_str()
            .filter(|text| choices.contains(text))
            .map(|text| json!(text)),
        Kind::List(item) => value
            .as_array()?
            .iter()
            .map(|entry| validate(item, entry.as_object()?))
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
    }
}

/// Pydantic's lax integer: a JSON integer, a float with no fraction, or a
/// string that reads as one.
fn integer(value: &Value) -> Option<Value> {
    match value {
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(value.clone()),
        Value::Number(number) => number
            .as_f64()
            .filter(|float| float.fract() == 0.0 && float.is_finite())
            .and_then(|float| format!("{float:.0}").parse::<i64>().ok())
            .map(|parsed| json!(parsed)),
        Value::String(text) => text.trim().parse::<i64>().ok().map(|parsed| json!(parsed)),
        _ => None,
    }
}

/// Pydantic's lax boolean.
fn boolean(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(flag) => Some(json!(flag)),
        Value::Number(number) => match number.as_f64() {
            Some(0.0) => Some(json!(false)),
            Some(1.0) => Some(json!(true)),
            _ => None,
        },
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Some(json!(false)),
            "1" | "on" | "t" | "true" | "y" | "yes" => Some(json!(true)),
            _ => None,
        },
        _ => None,
    }
}
