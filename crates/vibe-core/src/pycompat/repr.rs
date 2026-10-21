//! `repr` of the values the reference prints: strings, and whatever
//! `json.loads` hands back.

use std::fmt::Write as _;

use serde_json::Value;

use super::number::repr_float;
use super::text::isprintable;

/// `repr(str)`: single quotes unless the text holds one and no double quote,
/// the backslash and the chosen quote escaped, `\t`, `\n` and `\r` by name,
/// and every other character `str.isprintable` refuses as `\xhh`, `\uhhhh` or
/// `\Uhhhhhhhh`.
#[must_use]
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut rendered = String::with_capacity(text.len().saturating_add(2));
    rendered.push(quote);
    for character in text.chars() {
        match character {
            '\\' => rendered.push_str("\\\\"),
            '\t' => rendered.push_str("\\t"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            _ if character == quote => {
                rendered.push('\\');
                rendered.push(character);
            }
            _ if !isprintable(character) => {
                let code = u32::from(character);
                let _ = match code {
                    0..=0xff => write!(rendered, "\\x{code:02x}"),
                    0x100..=0xffff => write!(rendered, "\\u{code:04x}"),
                    _ => write!(rendered, "\\U{code:08x}"),
                };
            }
            _ => rendered.push(character),
        }
    }
    rendered.push(quote);
    rendered
}

/// `repr` of the value `json.loads` parses this JSON into: `None`, `True` and
/// `False`, integers as written, floats as [`repr_float`], and dictionaries in
/// key order.
#[must_use]
pub fn repr_json(value: &Value) -> String {
    let mut rendered = String::new();
    write_json(value, &mut rendered);
    rendered
}

/// `str` of the value `json.loads` parses this JSON into: a string is itself,
/// and anything else its [`repr_json`].
#[must_use]
pub fn str_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => repr_json(other),
    }
}

fn write_json(value: &Value, rendered: &mut String) {
    match value {
        Value::Null => rendered.push_str("None"),
        Value::Bool(true) => rendered.push_str("True"),
        Value::Bool(false) => rendered.push_str("False"),
        Value::Number(number) => match number.as_f64().filter(|_| number.is_f64()) {
            Some(float) => rendered.push_str(&repr_float(float)),
            None => {
                let _ = write!(rendered, "{number}");
            }
        },
        Value::String(text) => rendered.push_str(&repr_str(text)),
        Value::Array(items) => {
            rendered.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    rendered.push_str(", ");
                }
                write_json(item, rendered);
            }
            rendered.push(']');
        }
        Value::Object(fields) => {
            rendered.push('{');
            for (index, (key, field)) in fields.iter().enumerate() {
                if index > 0 {
                    rendered.push_str(", ");
                }
                rendered.push_str(&repr_str(key));
                rendered.push_str(": ");
                write_json(field, rendered);
            }
            rendered.push('}');
        }
    }
}
