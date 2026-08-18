//! The layer an operator writes in the environment rather than in a file.
//!
//! A `VIBE_*` variable addresses one configuration key by its path, so the
//! layer is a table built from names rather than parsed from a document. What
//! a variable is worth depends on what the registry declares the field to be:
//! a declared field is read under its own type and refused when it does not
//! parse, and an undeclared one is read permissively, because refusing it would
//! make an unknown name fatal rather than ignored.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;
use toml::{Table, Value};

use super::{ConfigError, ConfigMutation, patch, registry};

pub(super) fn environment_table(
    environment: &BTreeMap<String, String>,
) -> Result<Table, ConfigError> {
    let mut mutations = Vec::new();
    for (key, raw) in environment {
        let Some(name) = key.strip_prefix("VIBE_") else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let path = name
            .split("__")
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        if path.iter().any(String::is_empty) {
            return Err(ConfigError::InvalidEnvironmentKey(key.clone()));
        }
        let parsed = environment_value(key, &path, raw)?;
        mutations.push(ConfigMutation::set(path, parsed));
    }
    Ok(patch::apply_all(&Table::new(), &mutations)?)
}

/// The value a `VIBE_*` variable contributes, typed by the field it targets.
///
/// A registered top-level field is coerced to its declared kind, so a string
/// field keeps the text verbatim and a numeric field rejects text that is not a
/// number. Anything else, including the `__` nested paths this port accepts and
/// the reference does not, keeps the permissive TOML-literal reading it has
/// always had.
fn environment_value(variable: &str, path: &[String], raw: &str) -> Result<Value, ConfigError> {
    let spec = match path {
        [name] => registry::field(name),
        _ => None,
    };
    let Some(spec) = spec else {
        return Ok(permissive_environment_value(raw));
    };
    let invalid = |expected| ConfigError::InvalidEnvironmentValue {
        variable: variable.to_owned(),
        field: spec.name.to_owned(),
        expected,
    };
    match spec.kind {
        // The reference reads these through pydantic-settings, which accepts
        // this vocabulary for a boolean and nothing else.
        registry::FieldKind::Bool => match raw.to_ascii_lowercase().as_str() {
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(Value::Boolean(true)),
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(Value::Boolean(false)),
            _ => Err(invalid("boolean")),
        },
        registry::FieldKind::Int => raw
            .trim()
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| invalid("integer")),
        registry::FieldKind::Float => raw
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Value::Float)
            .ok_or_else(|| invalid("number")),
        // A complex value arrives as JSON, which is what pydantic-settings
        // parses a non-scalar environment override as.
        registry::FieldKind::List | registry::FieldKind::Complex => {
            serde_json::from_str::<JsonValue>(raw)
                .ok()
                .and_then(|value| Value::try_from(value).ok())
                .ok_or_else(|| invalid("JSON document"))
        }
        // An enum choice and a free string are both carried verbatim: the
        // reference validates the choice when the document is validated, not
        // when the environment layer is built.
        registry::FieldKind::Enum | registry::FieldKind::Str => Ok(Value::String(raw.to_owned())),
    }
}

fn permissive_environment_value(raw: &str) -> Value {
    format!("value = {raw}")
        .parse::<Table>()
        .ok()
        .and_then(|mut parsed| parsed.remove("value"))
        .unwrap_or_else(|| Value::String(raw.to_owned()))
}
