//! Strict validation of the JSON and TOML documents a plugin ships.
//!
//! The reference validates every plugin document with pydantic models in
//! strict mode that forbid unknown keys (`vibe/core/plugins/_native.py`): a
//! value must already have the declared type, nothing is coerced, and a key
//! the model does not declare is an error. These helpers reproduce that
//! verdict. Their messages are this port's own words: the reference publishes
//! pydantic's report, which the parity corpus records as prose.

use serde_json::{Map, Value};

/// One rejected field, located the way pydantic locates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub location: String,
    pub message: String,
}

/// Every error a document produced, joined into one message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidationErrors(pub Vec<FieldError>);

impl ValidationErrors {
    pub fn push(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.0.push(FieldError {
            location: location.into(),
            message: message.into(),
        });
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The report as one line: `<count> validation error(s) for <model>`,
    /// then each location and message.
    #[must_use]
    pub fn render(&self, model: &str) -> String {
        let count = self.0.len();
        let noun = if count == 1 { "error" } else { "errors" };
        let details = self
            .0
            .iter()
            .map(|error| {
                if error.location.is_empty() {
                    error.message.clone()
                } else {
                    format!("{}: {}", error.location, error.message)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("{count} validation {noun} for {model}: {details}")
    }
}

/// Joins a location under a parent.
#[must_use]
pub fn at(parent: &str, field: &str) -> String {
    if parent.is_empty() {
        field.to_owned()
    } else {
        format!("{parent}.{field}")
    }
}

/// Requires an object and reports every key outside `allowed`.
pub fn object<'a>(
    value: &'a Value,
    location: &str,
    allowed: &[&str],
    errors: &mut ValidationErrors,
) -> Option<&'a Map<String, Value>> {
    let Value::Object(map) = value else {
        errors.push(location, "expected an object");
        return None;
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            errors.push(at(location, key), "is not a declared field");
        }
    }
    Some(map)
}

/// A required string.
pub fn required_string(
    map: &Map<String, Value>,
    parent: &str,
    key: &str,
    errors: &mut ValidationErrors,
) -> Option<String> {
    match map.get(key) {
        None => {
            errors.push(at(parent, key), "is required");
            None
        }
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            errors.push(at(parent, key), "must be a string");
            None
        }
    }
}

/// An optional string, an explicit `null` reading as absent. The outer
/// `Option` is `None` when the value was rejected.
pub fn optional_string(
    map: &Map<String, Value>,
    parent: &str,
    key: &str,
    errors: &mut ValidationErrors,
) -> Option<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => {
            errors.push(at(parent, key), "must be a string or null");
            None
        }
    }
}

/// A list of strings, absent or `null` reading as `default_when_absent`.
pub fn string_list(
    map: &Map<String, Value>,
    parent: &str,
    key: &str,
    nullable: bool,
    errors: &mut ValidationErrors,
) -> Option<Option<Vec<String>>> {
    match map.get(key) {
        None => Some(None),
        Some(Value::Null) if nullable => Some(None),
        Some(Value::Array(items)) => {
            let mut strings = Vec::new();
            let mut valid = true;
            for (index, item) in items.iter().enumerate() {
                match item {
                    Value::String(text) => strings.push(text.clone()),
                    _ => {
                        errors.push(at(&at(parent, key), &index.to_string()), "must be a string");
                        valid = false;
                    }
                }
            }
            valid.then_some(Some(strings))
        }
        Some(_) => {
            errors.push(at(parent, key), "must be a list");
            None
        }
    }
}

/// A mapping of strings to strings.
pub fn string_map(
    map: &Map<String, Value>,
    parent: &str,
    key: &str,
    errors: &mut ValidationErrors,
) -> Option<Option<Vec<(String, String)>>> {
    match map.get(key) {
        None => Some(None),
        Some(Value::Object(entries)) => {
            let mut pairs = Vec::new();
            let mut valid = true;
            for (name, item) in entries {
                match item {
                    Value::String(text) => pairs.push((name.clone(), text.clone())),
                    _ => {
                        errors.push(at(&at(parent, key), name), "must be a string");
                        valid = false;
                    }
                }
            }
            valid.then_some(Some(pairs))
        }
        Some(_) => {
            errors.push(at(parent, key), "must be an object");
            None
        }
    }
}

/// An integer literal equal to `expected`; booleans are not integers here.
pub fn integer_literal(
    map: &Map<String, Value>,
    parent: &str,
    key: &str,
    expected: i64,
    required: bool,
    errors: &mut ValidationErrors,
) -> bool {
    match map.get(key) {
        None if !required => true,
        None => {
            errors.push(at(parent, key), "is required");
            false
        }
        Some(Value::Number(number)) if number.as_i64() == Some(expected) && !number.is_f64() => {
            true
        }
        Some(_) => {
            errors.push(at(parent, key), format!("must be {expected}"));
            false
        }
    }
}

/// A string literal from a closed set.
pub fn string_choice(
    map: &Map<String, Value>,
    parent: &str,
    key: &str,
    choices: &[&str],
    errors: &mut ValidationErrors,
) -> Option<Option<String>> {
    match map.get(key) {
        None => Some(None),
        Some(Value::String(text)) if choices.contains(&text.as_str()) => Some(Some(text.clone())),
        Some(_) => {
            errors.push(
                at(parent, key),
                format!("must be one of {}", choices.join(", ")),
            );
            None
        }
    }
}

/// The number of characters Python counts in a string.
#[must_use]
pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Converts a TOML value into the JSON value `tomllib` would produce for the
/// pydantic model to read. A datetime becomes its string, which no plugin
/// model accepts where a string is declared.
#[must_use]
pub fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => {
            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
        }
        toml::Value::Boolean(flag) => Value::Bool(*flag),
        toml::Value::Datetime(datetime) => {
            let mut map = Map::new();
            map.insert("$datetime".to_owned(), Value::String(datetime.to_string()));
            Value::Object(map)
        }
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, item)| (key.clone(), toml_to_json(item)))
                .collect(),
        ),
    }
}
