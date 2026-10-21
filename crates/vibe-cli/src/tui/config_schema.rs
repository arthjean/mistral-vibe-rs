//! What a published configuration field schema says about the field, read the
//! same way by the settings panel and by `/config set`.

use serde_json::Value;

/// The JSON types a field schema declares, whether as one word or as a list
/// such as `["string", "null"]`.
pub(super) fn declared_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Whether a field holds a credential, which the terminal client neither shows
/// nor edits: the schema marks it write-only or a password, or a segment of its
/// dotted path names one by convention.
pub(super) fn field_is_secret<'a>(path: impl IntoIterator<Item = &'a str>, schema: &Value) -> bool {
    schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || schema.get("format").and_then(Value::as_str) == Some("password")
        || path.into_iter().any(|segment| {
            let normalized = segment.to_ascii_lowercase();
            normalized.contains("password")
                || normalized.contains("secret")
                || normalized.contains("api_key")
                || normalized.contains("token")
        })
}

#[cfg(test)]
mod config_schema_tests;
