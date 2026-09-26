//! Validated extraction of JSON-RPC request parameters.
//!
//! Every dispatcher used to carry its own copy of these checks, which drifted:
//! the same key could be size-bounded in one method and unbounded in the next.
//! The checks live here once, and each domain adapts [`ParamError`] to its own
//! error type.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use vibe_protocol::{InvalidParamsIssue, PathSegment};

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

pub(crate) fn optional_u64(
    params: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<u64>, ParamError> {
    params
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| ParamError(format!("{key} must be an integer")))
        })
        .transpose()
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

/// `value` read as pydantic's lax mode reads an `int`.
pub(crate) fn python_int(value: &Value) -> Result<i128, &'static str> {
    match value {
        Value::Bool(flag) => Ok(i128::from(*flag)),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                return Ok(i128::from(value));
            }
            if let Some(value) = number.as_u64() {
                return Ok(i128::from(value));
            }
            let value = number.as_f64().unwrap_or(f64::NAN);
            if !value.is_finite() {
                return Err("Input should be a finite number");
            }
            if value.fract() != 0.0 {
                return Err("Input should be a valid integer, got a number with a fractional part");
            }
            // An integer literal past the 64-bit range reaches this side of the
            // envelope as a float, which is the only way to tell it holds no
            // fraction; the saturation is the documented one above.
            #[allow(clippy::cast_possible_truncation)]
            Ok(value as i128)
        }
        Value::String(text) => parse_integer_text(text.trim())
            .ok_or("Input should be a valid integer, unable to parse string as an integer"),
        _ => Err("Input should be a valid integer"),
    }
}

/// Decimal digits with an optional sign, single underscores between digits,
/// and an optional fraction made only of zeros.
fn parse_integer_text(text: &str) -> Option<i128> {
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (body, None),
    };
    if fraction
        .is_some_and(|fraction| fraction.is_empty() || fraction.bytes().any(|byte| byte != b'0'))
    {
        return None;
    }
    let mut value: i128 = 0;
    let mut previous_underscore = true;
    let mut digits = 0_usize;
    for byte in whole.bytes() {
        match byte {
            b'0'..=b'9' => {
                value = value
                    .saturating_mul(10)
                    .saturating_add(i128::from(byte - b'0'));
                previous_underscore = false;
                digits += 1;
            }
            b'_' if !previous_underscore => previous_underscore = true,
            _ => return None,
        }
    }
    if digits == 0 || previous_underscore {
        return None;
    }
    Some(if negative { -value } else { value })
}

/// Validates a request's parameters the way the reference's `validate_wire`
/// does: every violation is collected rather than the first, each under the
/// path to the value, declared fields first and unknown ones after. The paths
/// and the count are the contract a client reads; the sentences are this
/// port's own.
#[derive(Default)]
pub(crate) struct WireCheck {
    issues: Vec<InvalidParamsIssue>,
}

impl WireCheck {
    pub(crate) fn report(&mut self, path: Vec<PathSegment>, message: &str) {
        self.issues.push(InvalidParamsIssue {
            path,
            message: message.to_owned(),
        });
    }

    /// A string the request must carry.
    pub(crate) fn required_string(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
    ) -> Option<String> {
        match object.get(field) {
            None => {
                self.report(child_path(parent, field), "Field required");
                None
            }
            Some(value) => self.string(value, child_path(parent, field)),
        }
    }

    /// A string the request may leave out or send as null.
    pub(crate) fn optional_string(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
    ) -> Option<String> {
        match object.get(field) {
            None | Some(Value::Null) => None,
            Some(value) => self.string(value, child_path(parent, field)),
        }
    }

    fn string(&mut self, value: &Value, path: Vec<PathSegment>) -> Option<String> {
        if let Value::String(text) = value {
            return Some(text.clone());
        }
        self.report(path, "Input should be a valid string");
        None
    }

    /// A list of strings the request may leave out or send as null.
    pub(crate) fn optional_strings(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
    ) -> Option<Vec<String>> {
        let path = child_path(parent, field);
        match object.get(field) {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                let mut strings = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let mut item_path = path.clone();
                    item_path.push(PathSegment::Index(index));
                    strings.extend(self.string(item, item_path));
                }
                Some(strings)
            }
            Some(_) => {
                self.report(path, "Input should be a valid list");
                None
            }
        }
    }

    /// A boolean the request may leave out, read leniently as pydantic does.
    pub(crate) fn optional_bool(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
    ) -> Option<bool> {
        match object.get(field) {
            None | Some(Value::Null) => None,
            Some(Value::Bool(flag)) => Some(*flag),
            Some(Value::Number(number)) if number.as_u64() == Some(0) => Some(false),
            Some(Value::Number(number)) if number.as_u64() == Some(1) => Some(true),
            Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" | "t" | "y" => Some(true),
                "false" | "0" | "no" | "off" | "f" | "n" => Some(false),
                _ => {
                    self.report(child_path(parent, field), "Input should be a valid boolean");
                    None
                }
            },
            Some(_) => {
                self.report(child_path(parent, field), "Input should be a valid boolean");
                None
            }
        }
    }

    /// An integer between `min` and `max`, `default` when left out.
    pub(crate) fn bounded_int(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
        default: i64,
        (min, max): (i64, i64),
    ) -> Option<i64> {
        let Some(value) = object.get(field) else {
            return Some(default);
        };
        let path = child_path(parent, field);
        match python_int(value) {
            Ok(number) if number < i128::from(min) => {
                self.report(
                    path,
                    &format!("Input should be greater than or equal to {min}"),
                );
                None
            }
            Ok(number) if number > i128::from(max) => {
                self.report(
                    path,
                    &format!("Input should be less than or equal to {max}"),
                );
                None
            }
            Ok(number) => i64::try_from(number).ok(),
            Err(message) => {
                self.report(path, message);
                None
            }
        }
    }

    /// A nested object the request may leave out, answered as an empty one.
    pub(crate) fn optional_object<'a>(
        &mut self,
        object: &'a Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
    ) -> Option<&'a Map<String, Value>> {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        match object.get(field) {
            None => Some(EMPTY.get_or_init(Map::new)),
            Some(Value::Object(inner)) => Some(inner),
            Some(_) => {
                self.report(
                    child_path(parent, field),
                    "Input should be a valid dictionary or object to extract fields from",
                );
                None
            }
        }
    }

    /// Reports every field of `object` that `declared` does not name.
    pub(crate) fn extras(
        &mut self,
        object: &Map<String, Value>,
        declared: &[&str],
        parent: &[PathSegment],
    ) {
        for key in object.keys() {
            if !declared.contains(&key.as_str()) {
                self.report(child_path(parent, key), "Extra inputs are not permitted");
            }
        }
    }

    /// The violations found, or none.
    pub(crate) fn finish(self) -> Result<(), Vec<InvalidParamsIssue>> {
        if self.issues.is_empty() {
            Ok(())
        } else {
            Err(self.issues)
        }
    }
}

pub(crate) fn child_path(parent: &[PathSegment], field: &str) -> Vec<PathSegment> {
    let mut path = parent.to_vec();
    path.push(PathSegment::Field(field.to_owned()));
    path
}

/// The parameters as the JSON object they were sent as.
pub(crate) fn object_of(params: &BTreeMap<String, Value>) -> Map<String, Value> {
    params
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}
