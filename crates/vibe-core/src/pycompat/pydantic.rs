//! What pydantic answers for a payload: how its lax mode reads a string as a
//! boolean or a number, and how `str(ValidationError)` reports a refusal.
//!
//! The lax readers were measured against the pinned checkout's pydantic
//! (`TypeAdapter(bool | int | float).validate_python(text)`).

use std::fmt::Write as _;

use serde_json::Value;

use super::repr::repr_json;

/// A string read as a `bool`: one of the six words for each value, in any
/// ASCII case and with nothing around it, since pydantic strips no whitespace
/// before it compares.
#[must_use]
pub fn lax_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
        "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
        _ => None,
    }
}

/// A string read as an `int`: surrounding `White_Space`, a sign, ASCII digits
/// a single underscore may separate, and a fraction made only of zeros, so
/// `"12.0"` reads as twelve and `"12e0"`, `"12."` and `"_1"` read as nothing.
/// Pydantic's integers are unbounded; this one saturates at the `i128` range.
#[must_use]
pub fn lax_int(text: &str) -> Option<i128> {
    let trimmed = text.trim();
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
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

/// A string read as a `float`: surrounding `White_Space` and Rust's float
/// grammar, which takes `inf`, `infinity` and `nan` in any case, once every
/// underscore is dropped. Unlike Python's `float()`, an underscore needs no
/// digit around it; the text only may not start or end with one, nor hold two
/// in a row, so `"1_.5"` and `"in_f"` read where `"_1"` does not.
#[must_use]
pub fn lax_float(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.starts_with('_') || trimmed.ends_with('_') || trimmed.contains("__") {
        return None;
    }
    trimmed.replace('_', "").parse::<f64>().ok()
}

/// The name of the Python type `json.loads` returns for `value`, which is
/// what a validation error reports as its `input_type`.
#[must_use]
pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// The values a `literal_error` says the input should be: each one's `repr`,
/// the last two joined by `or` and the others by commas.
#[must_use]
pub fn expected_literals(values: &[Value]) -> String {
    let rendered = values.iter().map(repr_json).collect::<Vec<_>>();
    match rendered.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// One error a `ValidationError` carries.
#[derive(Debug, Clone, Copy)]
pub struct LineError<'a> {
    /// The path to the refused value, its parts joined by `.`; empty for an
    /// error on the whole model.
    pub location: &'a str,
    /// The error type, such as `string_type` or `value_error`.
    pub kind: &'a str,
    pub message: &'a str,
    pub input: &'a Value,
}

/// `str(ValidationError)` for `errors` raised validating `title`.
///
/// Each error reads `location`, then its message, type and input on an
/// indented line; the input is its `repr`, cut past fifty characters to its
/// first twenty-five and last twenty-four around an ellipsis. No line points
/// at the error documentation: importing the reference sets
/// `PYDANTIC_ERRORS_INCLUDE_URL=0` (`vibe/__init__.py`).
#[must_use]
pub fn validation_error(title: &str, errors: &[LineError<'_>]) -> String {
    let mut rendered = format!(
        "{} validation error{} for {title}",
        errors.len(),
        if errors.len() == 1 { "" } else { "s" }
    );
    for error in errors {
        if !error.location.is_empty() {
            rendered.push('\n');
            rendered.push_str(error.location);
        }
        let _ = write!(
            rendered,
            "\n  {} [type={}, input_value={}, input_type={}]",
            error.message,
            error.kind,
            input_value(error.input),
            type_name(error.input),
        );
    }
    rendered
}

fn input_value(input: &Value) -> String {
    let repr = repr_json(input);
    let characters = repr.chars().collect::<Vec<_>>();
    if characters.len() <= 50 {
        return repr;
    }
    let head = characters[..25].iter().collect::<String>();
    let tail = characters[characters.len() - 24..]
        .iter()
        .collect::<String>();
    format!("{head}...{tail}")
}
