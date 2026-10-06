//! The layer an operator writes in the environment rather than in a file.
//!
//! A `VIBE_*` variable addresses one configuration key by its path, so the
//! layer is a table built from names rather than parsed from a document.
//! Reference `EnvironmentLayer` reads it through pydantic-settings with the
//! `VIBE_` prefix, case-insensitive names, `__` as the nesting delimiter, empty
//! values ignored and undeclared names ignored. This port reads it the same
//! way: a variable that names no declared field contributes nothing, and a
//! nested name contributes only where the field's published schema admits the
//! path.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;
use toml::{Table, Value};

use super::{ConfigError, ConfigMutation, patch, registry};

/// The prefix every environment override carries, matched without regard to
/// case.
const PREFIX: &str = "VIBE_";
/// What separates the segments of a nested name.
const NESTING_DELIMITER: &str = "__";

pub(super) fn environment_table(
    environment: &BTreeMap<String, String>,
) -> Result<Table, ConfigError> {
    let mut mutations = Vec::new();
    for (key, raw) in environment {
        let Some(name) = strip_prefix(key) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let path = name
            .split(NESTING_DELIMITER)
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let Some(spec) = path.first().and_then(|field| registry::field(field)) else {
            continue;
        };
        let parsed = match path.as_slice() {
            [_] => typed_value(key, spec, raw)?,
            [_, nested @ ..] => {
                if nested.iter().any(String::is_empty) {
                    return Err(ConfigError::InvalidEnvironmentKey(key.clone()));
                }
                let Some(value) = nested_value(spec, nested, raw) else {
                    continue;
                };
                value
            }
            [] => continue,
        };
        mutations.push(ConfigMutation::set(path, parsed));
    }
    Ok(patch::apply_all(&Table::new(), &mutations)?)
}

/// The variable name without its prefix, or `None` when it carries none.
/// pydantic-settings lowercases the whole name when it ignores case, so
/// `vibe_theme` and `Vibe_Theme` address `theme` as `VIBE_THEME` does.
fn strip_prefix(key: &str) -> Option<&str> {
    let head = key.get(..PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX)
        .then(|| &key[PREFIX.len()..])
        .filter(|name| !name.is_empty())
}

/// The value a variable naming a top-level field contributes, typed by the
/// kind the field declares.
fn typed_value(
    variable: &str,
    spec: &registry::FieldSpec,
    raw: &str,
) -> Result<Value, ConfigError> {
    let invalid = |expected| ConfigError::InvalidEnvironmentValue {
        variable: variable.to_owned(),
        field: spec.name.to_owned(),
        expected,
    };
    match spec.kind {
        // The reference reads these through pydantic-settings, which accepts
        // this vocabulary for a boolean and nothing else.
        registry::FieldKind::Bool => parse_flag(raw).ok_or_else(|| invalid("boolean")),
        registry::FieldKind::Int => raw
            .trim()
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| invalid("integer")),
        registry::FieldKind::Float => parse_number(raw).ok_or_else(|| invalid("number")),
        // A complex value arrives as JSON, which is what pydantic-settings
        // parses a non-scalar environment override as.
        registry::FieldKind::List | registry::FieldKind::Complex => {
            json_value(raw).ok_or_else(|| invalid("JSON document"))
        }
        // An enum choice and a free string are both carried verbatim: the
        // reference validates the choice when the document is validated, not
        // when the environment layer is built.
        registry::FieldKind::Enum | registry::FieldKind::Str => Ok(Value::String(raw.to_owned())),
    }
}

/// The value a nested name contributes, or `None` where the field's schema
/// declares its keys and the path leaves them, which pydantic ignores as it
/// ignores any extra key of a nested model.
///
/// The leaf is typed by the schema where it declares a scalar type, so a
/// string setting keeps text that happens to spell a number, and is read as a
/// TOML literal otherwise, which covers a JSON value.
fn nested_value(spec: &registry::FieldSpec, nested: &[String], raw: &str) -> Option<Value> {
    let schema = registry::json_schema();
    let mut cursor = schema.get("properties")?.get(spec.name)?.clone();
    for segment in nested {
        let next = cursor
            .get("properties")
            .and_then(|properties| properties.get(segment))
            .cloned()
            .or_else(|| match cursor.get("additionalProperties") {
                Some(JsonValue::Object(items)) => Some(JsonValue::Object(items.clone())),
                Some(JsonValue::Bool(true)) => Some(JsonValue::Object(serde_json::Map::new())),
                Some(_) => None,
                // A schema that names no keys admits any.
                None => cursor
                    .get("properties")
                    .is_none()
                    .then(|| JsonValue::Object(serde_json::Map::new())),
            })?;
        cursor = next;
    }
    let leaf = match cursor.get("type") {
        Some(JsonValue::String(kind)) => match kind.as_str() {
            "string" => Some(Value::String(raw.to_owned())),
            "boolean" => parse_flag(raw),
            "integer" => raw.trim().parse::<i64>().ok().map(Value::Integer),
            "number" => parse_number(raw),
            _ => None,
        },
        _ => None,
    };
    Some(leaf.unwrap_or_else(|| permissive_environment_value(raw)))
}

fn parse_flag(raw: &str) -> Option<Value> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "on" | "t" | "true" | "y" | "yes" => Some(Value::Boolean(true)),
        "0" | "off" | "f" | "false" | "n" | "no" => Some(Value::Boolean(false)),
        _ => None,
    }
}

fn parse_number(raw: &str) -> Option<Value> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(Value::Float)
}

fn json_value(raw: &str) -> Option<Value> {
    serde_json::from_str::<JsonValue>(raw)
        .ok()
        .and_then(|value| Value::try_from(value).ok())
}

fn permissive_environment_value(raw: &str) -> Value {
    json_value(raw)
        .or_else(|| {
            format!("value = {raw}")
                .parse::<Table>()
                .ok()
                .and_then(|mut parsed| parsed.remove("value"))
        })
        .unwrap_or_else(|| Value::String(raw.to_owned()))
}
