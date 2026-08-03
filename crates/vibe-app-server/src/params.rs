//! Validated extraction of JSON-RPC request parameters.
//!
//! Every dispatcher used to carry its own copy of these checks, which drifted:
//! the same key could be size-bounded in one method and unbounded in the next.
//! The checks live here once, and each domain adapts [`ParamError`] to its own
//! error type.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The largest string the boundary accepts for a single parameter.
pub(crate) const MAX_PARAM_STRING_BYTES: usize = 65_536;

/// Why a parameter was rejected. The message is already client-facing.
pub(crate) struct ParamError(String);

impl ParamError {
    #[must_use]
    pub(crate) fn message(self) -> String {
        self.0
    }
}

fn bounded_str(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|value| value.len() <= MAX_PARAM_STRING_BYTES && !value.contains('\0'))
}

pub(crate) fn required_string<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, ParamError> {
    params
        .get(key)
        .and_then(bounded_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ParamError(format!("{key} must be a non-empty string")))
}

/// Reads an optional string, treating an absent key and an explicit null the
/// same way.
pub(crate) fn optional_string<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ParamError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => bounded_str(value)
            .map(Some)
            .ok_or_else(|| ParamError(format!("{key} must be a string"))),
    }
}

pub(crate) fn required_bool(
    params: &BTreeMap<String, Value>,
    key: &str,
) -> Result<bool, ParamError> {
    params
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| ParamError(format!("{key} must be a boolean")))
}

pub(crate) fn required_object<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, ParamError> {
    params
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| ParamError(format!("{key} must be an object")))
}

/// Reads a bounded index or count, falling back to `default` when absent.
pub(crate) fn usize_param(
    params: &BTreeMap<String, Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ParamError> {
    let Some(value) = params.get(key) else {
        return Ok(default);
    };
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (*value >= min) && (*value <= max))
        .ok_or_else(|| ParamError(format!("{key} is out of range")))
}
