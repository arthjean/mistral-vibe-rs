//! The canonical byte forms a plugin identity is digested from.
//!
//! Reference `vibe/core/plugins/_canonical.py`: strings are normalized to NFC
//! before they are compared or digested, and a JSON value is serialized with
//! the JSON Canonicalization Scheme (RFC 8785, the `rfc8785` package) before
//! it is hashed. Two hosts that read the same manifest therefore agree on its
//! digest whatever key order or Unicode composition the file used.

use serde_json::Value;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// A value RFC 8785 cannot serialize: a float that is not finite, or an
/// integer outside the interval IEEE 754 doubles represent exactly.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CanonicalJsonError(String);

/// The NFC form of a string. Reference `normalize_nfc`; a Rust string is
/// always well formed, so the encoding check the reference runs first cannot
/// fail here.
#[must_use]
pub fn normalize_nfc(value: &str) -> String {
    value.nfc().collect()
}

/// A JSON value with every string, key included, in NFC. Reference
/// `normalize_json`.
#[must_use]
pub fn normalize_json(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(normalize_nfc(text)),
        Value::Array(items) => Value::Array(items.iter().map(normalize_json).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (normalize_nfc(key), normalize_json(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The RFC 8785 serialization of a JSON value.
///
/// # Errors
///
/// A non-finite float, or an integer beyond 2^53 - 1 in magnitude, which the
/// reference's serializer refuses as well.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, CanonicalJsonError> {
    let mut output = String::new();
    write_value(&mut output, value)?;
    Ok(output.into_bytes())
}

/// The SHA-256 of a value's RFC 8785 bytes, in hex. Reference
/// `canonical_json_digest`.
///
/// # Errors
///
/// Whatever [`canonical_json`] refuses.
pub fn canonical_json_digest(value: &Value) -> Result<String, CanonicalJsonError> {
    Ok(sha256_hex(&canonical_json(value)?))
}

/// The SHA-256 of some bytes, in lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

fn write_value(output: &mut String, value: &Value) -> Result<(), CanonicalJsonError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(true) => output.push_str("true"),
        Value::Bool(false) => output.push_str("false"),
        Value::Number(number) => write_number(output, number)?,
        Value::String(text) => write_string(output, text),
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(output, item)?;
            }
            output.push(']');
        }
        Value::Object(map) => {
            // Keys sort by their UTF-16 code units, which is not the order of
            // their UTF-8 bytes once a key leaves the Basic Multilingual Plane.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            output.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_string(output, key);
                output.push(':');
                write_value(output, item)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn write_string(output: &mut String, text: &str) {
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => output.push(other),
        }
    }
    output.push('"');
}

fn write_number(
    output: &mut String,
    number: &serde_json::Number,
) -> Result<(), CanonicalJsonError> {
    if let Some(integer) = number.as_u64() {
        if integer > MAX_SAFE_INTEGER {
            return Err(CanonicalJsonError(format!(
                "{integer} exceeds safe integer range"
            )));
        }
        output.push_str(&integer.to_string());
        return Ok(());
    }
    if let Some(integer) = number.as_i64() {
        if integer.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(CanonicalJsonError(format!(
                "{integer} exceeds safe integer range"
            )));
        }
        output.push_str(&integer.to_string());
        return Ok(());
    }
    let Some(float) = number.as_f64() else {
        return Err(CanonicalJsonError(format!("{number} is not a number")));
    };
    output.push_str(&es6_number(float)?);
    Ok(())
}

/// ECMAScript's `Number.prototype.toString` for a finite double, which
/// RFC 8785 adopts: the shortest round-tripping digits, laid out in fixed
/// notation between 1e-7 and 1e21 and in exponent notation outside.
fn es6_number(value: f64) -> Result<String, CanonicalJsonError> {
    if !value.is_finite() {
        return Err(CanonicalJsonError(format!(
            "{value} is not a finite number"
        )));
    }
    if value == 0.0 {
        return Ok("0".to_owned());
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` writes the shortest round-tripping digits as `d.ddde<exp>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .ok_or_else(|| CanonicalJsonError(format!("{value} has no exponent")))?;
    let exponent: i32 = exponent
        .parse()
        .map_err(|_| CanonicalJsonError(format!("{value} has an unreadable exponent")))?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let count = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    // `n` places the decimal point: the value is 0.digits * 10^n.
    let n = exponent + 1;
    let body = if count <= n && n <= 21 {
        format!(
            "{digits}{}",
            "0".repeat(usize::try_from(n - count).unwrap_or(0))
        )
    } else if 0 < n && n <= 21 {
        let split = usize::try_from(n).unwrap_or(0);
        format!("{}.{}", &digits[..split], &digits[split..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(usize::try_from(-n).unwrap_or(0)))
    } else {
        let exponent = n - 1;
        let exponent_sign = if exponent < 0 { "-" } else { "+" };
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        format!(
            "{first}{fraction}e{exponent_sign}{}",
            exponent.unsigned_abs()
        )
    };
    Ok(format!("{sign}{body}"))
}

#[cfg(test)]
mod canonical_tests;
