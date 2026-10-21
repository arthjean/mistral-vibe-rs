//! `json.dumps`.
//!
//! `serde_json` writes no space after a separator, emits non-ASCII raw, and
//! spells floats its own way; the reference's fingerprints, headers and
//! model-facing text are `json.dumps` output, so they are written here. The
//! workspace builds `serde_json` with `preserve_order`, so an object keeps the
//! order its keys arrived in, a repeated key at its first position with its
//! last value, as the dictionary `json.loads` builds does.

use std::fmt::Write as _;

use serde_json::Value;

use super::number::repr_float;

/// The `json.dumps` keyword arguments a caller sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dumps {
    /// Escape every character outside printable ASCII as `\uXXXX`.
    pub ensure_ascii: bool,
    /// Write every object's keys in code point order.
    pub sort_keys: bool,
    /// Put each item on its own line, nested this many spaces deeper.
    pub indent: Option<usize>,
    /// The item and key separators; [`None`] is Python's default, `", "` and
    /// `": "`, with the item separator's space dropped once `indent` is set.
    pub separators: Option<(&'static str, &'static str)>,
}

impl Dumps {
    /// `json.dumps(value)`.
    pub const DEFAULT: Self = Self {
        ensure_ascii: true,
        sort_keys: false,
        indent: None,
        separators: None,
    };
}

/// `json.dumps(value, **options)`.
#[must_use]
pub fn dumps(value: &Value, options: Dumps) -> String {
    let (item, key) = options.separators.unwrap_or(match options.indent {
        Some(_) => (",", ": "),
        None => (", ", ": "),
    });
    let mut writer = Writer {
        options,
        item,
        key,
        out: String::new(),
    };
    writer.value(value, 0);
    writer.out
}

struct Writer {
    options: Dumps,
    item: &'static str,
    key: &'static str,
    out: String,
}

impl Writer {
    fn value(&mut self, value: &Value, depth: usize) {
        match value {
            Value::Null => self.out.push_str("null"),
            Value::Bool(true) => self.out.push_str("true"),
            Value::Bool(false) => self.out.push_str("false"),
            Value::Number(number) => match number.as_f64().filter(|_| number.is_f64()) {
                Some(float) => self.out.push_str(&float_text(float)),
                None => {
                    let _ = write!(self.out, "{number}");
                }
            },
            Value::String(text) => self.string(text),
            Value::Array(items) if items.is_empty() => self.out.push_str("[]"),
            Value::Object(fields) if fields.is_empty() => self.out.push_str("{}"),
            Value::Array(items) => {
                self.out.push('[');
                for (index, item) in items.iter().enumerate() {
                    self.separate(index, depth + 1);
                    self.value(item, depth + 1);
                }
                self.newline(depth);
                self.out.push(']');
            }
            Value::Object(fields) => {
                let mut entries = fields.iter().collect::<Vec<_>>();
                if self.options.sort_keys {
                    entries.sort_by_key(|(name, _)| *name);
                }
                self.out.push('{');
                for (index, (name, field)) in entries.into_iter().enumerate() {
                    self.separate(index, depth + 1);
                    self.string(name);
                    self.out.push_str(self.key);
                    self.value(field, depth + 1);
                }
                self.newline(depth);
                self.out.push('}');
            }
        }
    }

    /// What precedes the item at `index`: the item separator after the first,
    /// and the line break and indentation when `indent` is set.
    fn separate(&mut self, index: usize, depth: usize) {
        if index > 0 {
            self.out.push_str(self.item);
        }
        self.newline(depth);
    }

    fn newline(&mut self, depth: usize) {
        if let Some(width) = self.options.indent {
            self.out.push('\n');
            self.out.push_str(&" ".repeat(width * depth));
        }
    }

    /// A string: the quote and the backslash escaped, the five short control
    /// escapes, `\u00XX` for the rest of the C0 range, and with
    /// `ensure_ascii` every character past `~` as `\uXXXX`, a supplementary
    /// one as its surrogate pair.
    fn string(&mut self, text: &str) {
        self.out.push('"');
        for character in text.chars() {
            match character {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{c}' => self.out.push_str("\\f"),
                ' '..='~' => self.out.push(character),
                _ if !self.options.ensure_ascii && character > '\u{1f}' => {
                    self.out.push(character);
                }
                _ => {
                    let mut units = [0_u16; 2];
                    for unit in character.encode_utf16(&mut units) {
                        let _ = write!(self.out, "\\u{unit:04x}");
                    }
                }
            }
        }
        self.out.push('"');
    }
}

/// A float as `json.dumps` writes it: its `repr`, with the non-finite values
/// in JavaScript's spelling.
fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        repr_float(value)
    }
}

/// Reference: Python `bool(value)` on what `json.loads` returns: `None`,
/// `False`, a zero and an empty string, list or dict are false, and every
/// other value is true.
#[must_use]
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// Whether two values are equal as Python's `==` compares the objects
/// `json.loads` decodes them to.
///
/// Python's booleans are integers, so `False == 0` and `True == 1.0`; an
/// integer and a float compare by value; a dict compares regardless of key
/// order. Every other pair compares by kind and content.
#[must_use]
pub fn python_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| python_eq(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|candidate| python_eq(value, candidate))
                })
        }
        (left, right) => match (python_number(left), python_number(right)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        },
    }
}

/// A boolean or a number as the value Python compares it by.
fn python_number(value: &Value) -> Option<PythonNumber> {
    match value {
        Value::Bool(flag) => Some(PythonNumber::Integer(i128::from(*flag))),
        Value::Number(number) => number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
            .map(PythonNumber::Integer)
            .or_else(|| number.as_f64().map(PythonNumber::Float)),
        _ => None,
    }
}

/// A number as Python compares it: an integer exactly, a float by value.
#[derive(Clone, Copy)]
enum PythonNumber {
    Integer(i128),
    Float(f64),
}

impl PartialEq for PythonNumber {
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Integer(left), Self::Integer(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Integer(integer), Self::Float(float))
            | (Self::Float(float), Self::Integer(integer)) => {
                float.fract() == 0.0 && integer as f64 == float
            }
        }
    }
}
