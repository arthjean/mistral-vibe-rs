//! `json.dumps` as the reference calls it, for the few strings a backend
//! writes into an answer rather than onto the wire: a tool call's arguments
//! rebuilt from a parsed object. The model reads them back verbatim in the
//! next request, so the spacing and the key order are part of the
//! conversation.
//!
//! Python's defaults separate items with `", "` and keys with `": "`, keep the
//! keys in the order the provider sent them, and with `ensure_ascii` escape
//! every character outside printable ASCII. [`Ordered`] is a JSON tree that
//! keeps that order, which `serde_json::Value` does not.

use std::fmt::{self, Write as _};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A JSON value whose objects keep their key order.
#[derive(Debug, Clone, PartialEq)]
pub enum Ordered {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl Ordered {
    /// Parses `text`, keeping key order.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    /// The value under `key` when this is an object; the last one wins, as
    /// `json.loads` keeps the last of repeated keys.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    #[must_use]
    pub fn at(&self, index: usize) -> Option<&Self> {
        match self {
            Self::Array(items) => items.get(index),
            _ => None,
        }
    }

    /// `json.dumps(value, ensure_ascii=ensure_ascii)`.
    #[must_use]
    pub fn dumps(&self, ensure_ascii: bool) -> String {
        let mut out = String::new();
        self.write(&mut out, ensure_ascii);
        out
    }

    fn write(&self, out: &mut String, ensure_ascii: bool) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Number(number) => out.push_str(&number.to_string()),
            Self::String(text) => write_string(out, text, ensure_ascii),
            Self::Array(items) => {
                out.push('[');
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        out.push_str(", ");
                    }
                    item.write(out, ensure_ascii);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                // Repeated keys collapse to their last value at their first
                // position, as a Python dict built from them would.
                let mut seen: Vec<&str> = Vec::new();
                out.push('{');
                for (name, _) in fields {
                    if seen.contains(&name.as_str()) {
                        continue;
                    }
                    if !seen.is_empty() {
                        out.push_str(", ");
                    }
                    seen.push(name);
                    write_string(out, name, ensure_ascii);
                    out.push_str(": ");
                    if let Some(value) = self.get(name) {
                        value.write(out, ensure_ascii);
                    }
                }
                out.push('}');
            }
        }
    }
}

impl From<&serde_json::Value> for Ordered {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(flag) => Self::Bool(*flag),
            serde_json::Value::Number(number) => Self::Number(number.clone()),
            serde_json::Value::String(text) => Self::String(text.clone()),
            serde_json::Value::Array(items) => Self::Array(items.iter().map(Self::from).collect()),
            serde_json::Value::Object(fields) => Self::Object(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Ordered, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = access.next_element()? {
            items.push(item);
        }
        Ok(Ordered::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Ordered, A::Error> {
        let mut fields = Vec::new();
        while let Some((name, value)) = access.next_entry::<String, Ordered>()? {
            fields.push((name, value));
        }
        Ok(Ordered::Object(fields))
    }
}

fn write_string(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(character),
            _ if !ensure_ascii && u32::from(character) >= 0x20 => out.push(character),
            _ => {
                let mut units = [0_u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}
