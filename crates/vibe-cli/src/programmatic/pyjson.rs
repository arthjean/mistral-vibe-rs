//! JSON written the way the reference's programmatic output writes it.
//!
//! Reference `ProgrammaticOutput` (`vibe/cli/programmatic.py`) renders every
//! document with Python's `json.dump(..., ensure_ascii=False)`: `", "` and
//! `": "` between items on one line, or an indent of two with `indent=2`, and
//! keys in the order its models declare their fields. serde writes neither
//! that spacing nor, for an internally tagged enum, that order, so a value is
//! read back into [`Ordered`], which keeps the order serde wrote, and written
//! out again with Python's separators and float spelling.

use std::fmt::{self, Write as _};

use serde::Serialize;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A JSON value that remembers the order its object keys arrived in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ordered {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl Ordered {
    /// `value` as serde writes it, keys in declaration order.
    pub(crate) fn of(value: &impl Serialize) -> Result<Self, serde_json::Error> {
        let text = serde_json::to_string(value)?;
        serde_json::from_str(&text)
    }

    /// Moves `key` right after `anchor` in this object, which is where a
    /// pydantic model that inherits its base fields declares a discriminator
    /// serde writes first.
    pub(crate) fn move_after(&mut self, key: &str, anchor: &str) {
        let Self::Object(fields) = self else {
            return;
        };
        let Some(from) = fields.iter().position(|(name, _)| name == key) else {
            return;
        };
        let field = fields.remove(from);
        let to = fields
            .iter()
            .position(|(name, _)| name == anchor)
            .map_or(fields.len(), |index| index + 1);
        fields.insert(to, field);
    }

    /// The value under `key`, when this is an object that holds one.
    pub(crate) fn get(&self, key: &str) -> Option<&Self> {
        let Self::Object(fields) = self else {
            return None;
        };
        fields
            .iter()
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    /// The same, to change in place.
    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut Self> {
        let Self::Object(fields) = self else {
            return None;
        };
        fields
            .iter_mut()
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// Puts this object's keys in the order `keys` names them; a key it does
    /// not name keeps its place after them, in the order it arrived.
    pub(crate) fn order_by(&mut self, keys: &[&str]) {
        let Self::Object(fields) = self else {
            return;
        };
        fields.sort_by_key(|(name, _)| {
            keys.iter()
                .position(|key| key == name)
                .unwrap_or(keys.len())
        });
    }

    /// `json.dumps(value, ensure_ascii=False)`.
    pub(crate) fn compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// `json.dumps(value, indent=2, ensure_ascii=False)`.
    pub(crate) fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(2), 0);
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(number) => out.push_str(&python_number(number)),
            Self::String(text) => write_string(out, text),
            Self::Array(items) if items.is_empty() => out.push_str("[]"),
            Self::Object(fields) if fields.is_empty() => out.push_str("{}"),
            Self::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    separate(out, indent, depth + 1, index);
                    item.write(out, indent, depth + 1);
                }
                close(out, indent, depth);
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    separate(out, indent, depth + 1, index);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write(out, indent, depth + 1);
                }
                close(out, indent, depth);
                out.push('}');
            }
        }
    }
}

fn separate(out: &mut String, indent: Option<usize>, depth: usize, index: usize) {
    match indent {
        Some(width) => {
            if index > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(width * depth));
        }
        None if index > 0 => out.push_str(", "),
        None => {}
    }
}

fn close(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * depth));
    }
}

/// Python's string escaping with `ensure_ascii=False`, which is serde's: the
/// quote, the backslash, the five short control escapes and `\u00XX` for the
/// rest of the C0 range, nothing else.
fn write_string(out: &mut String, text: &str) {
    match serde_json::to_string(text) {
        Ok(encoded) => out.push_str(&encoded),
        Err(_) => out.push_str("\"\""),
    }
}

/// A number as Python's `float.__repr__` or `int.__repr__` spells it.
fn python_number(number: &serde_json::Number) -> String {
    if number.is_i64() || number.is_u64() {
        return number.to_string();
    }
    let Some(value) = number.as_f64() else {
        return number.to_string();
    };
    python_float(value)
}

/// `repr(float)`: the shortest round-trip digits, in fixed notation when the
/// decimal exponent lies in `-4..16` and in scientific notation, with a signed
/// exponent of at least two digits, otherwise.
fn python_float(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return scientific;
    };
    let (sign, mantissa) = mantissa
        .strip_prefix('-')
        .map_or(("", mantissa), |rest| ("-", rest));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if (-4..16).contains(&exponent) {
        let mut fixed = String::new();
        let point = exponent + 1;
        if point <= 0 {
            fixed.push_str("0.");
            fixed.push_str(&"0".repeat(usize::try_from(-point).unwrap_or(0)));
            fixed.push_str(&digits);
        } else {
            let point = usize::try_from(point).unwrap_or(0);
            if digits.len() <= point {
                fixed.push_str(&digits);
                fixed.push_str(&"0".repeat(point - digits.len()));
                fixed.push_str(".0");
            } else {
                fixed.push_str(&digits[..point]);
                fixed.push('.');
                fixed.push_str(&digits[point..]);
            }
        }
        return format!("{sign}{fixed}");
    }
    let mut out = String::from(sign);
    out.push_str(&digits[..1]);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    let _ = write!(
        out,
        "e{}{:02}",
        if exponent < 0 { '-' } else { '+' },
        exponent.unsigned_abs()
    );
    out
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OrderedVisitor)
    }
}

struct OrderedVisitor;

impl<'de> Visitor<'de> for OrderedVisitor {
    type Value = Ordered;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Ordered, E> {
        Ok(Ordered::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Ordered, E> {
        Ok(Ordered::Null)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Ordered, E> {
        Ok(Ordered::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Ordered, E> {
        Ok(Ordered::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Ordered, E> {
        Ok(Ordered::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Ordered, E> {
        Ok(serde_json::Number::from_f64(value).map_or(Ordered::Null, Ordered::Number))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Ordered, E> {
        Ok(Ordered::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Ordered, E> {
        Ok(Ordered::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Ordered, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Ordered::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
        let mut fields: Vec<(String, Ordered)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Ordered>()? {
            // A repeated key keeps its first position and its last value, as a
            // Python dict built from the same pairs does.
            match fields.iter_mut().find(|(name, _)| *name == key) {
                Some(field) => field.1 = value,
                None => fields.push((key, value)),
            }
        }
        Ok(Ordered::Object(fields))
    }
}

#[cfg(test)]
mod pyjson_tests;
