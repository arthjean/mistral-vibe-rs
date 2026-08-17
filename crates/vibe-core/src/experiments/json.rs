//! A JSON value that keeps its object keys in the order they arrived in, and
//! the text Python's `json.dumps` gives one.
//!
//! The reference resolves an object- or array-valued variant by handing the
//! decoded payload to `json.dumps` and returning the string, so the *variant a
//! caller reads* is a serialization rather than a structure. Two properties of
//! that serialization are load-bearing and neither survives a round trip
//! through [`serde_json::Value`]:
//!
//! - **Key order.** `serde_json::Map` is a [`std::collections::BTreeMap`] here,
//!   so decoding and re-encoding sorts every object. Python keeps the order the
//!   document carried, which is the order the GrowthBook proxy wrote. A sorted
//!   re-encode would hand the configuration layer, the session metadata and the
//!   telemetry label a different string than the reference produces for the
//!   same response.
//! - **Separators and escaping.** `json.dumps` writes `", "` between items and
//!   `": "` after a key, and escapes every non-ASCII character as `\uXXXX`.
//!   [`serde_json::to_string`] writes neither space and emits non-ASCII raw.
//!
//! Enabling `serde_json`'s `preserve_order` feature would fix the first
//! property for the whole workspace and none of the second, so the value lives
//! here instead, private to the one boundary that needs it.
//!
//! Reference: `vibe/core/experiments/manager.py` at the pinned commit, where
//! `get_variant_or_none`, `_forced_variant_or_none` and `_variant_label` each
//! call `json.dumps` on a value the eval response carried.

use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// --------------------------------------------------------------------------
// The map
// --------------------------------------------------------------------------

/// A string-keyed map that keeps its keys in insertion order, as a Python dict
/// does.
///
/// The eval payload is small by construction: a response carries at most a
/// handful of features and a variant at most a small object, so a vector of
/// pairs is the whole implementation and a lookup is a scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderedMap<V> {
    entries: Vec<(String, V)>,
}

impl<V> OrderedMap<V> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The value one key carries, or [`None`].
    pub fn get(&self, key: &str) -> Option<&V> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value)
    }

    /// Inserts a key, replacing an existing one in place so a duplicate key
    /// keeps the position of its first appearance and the value of its last,
    /// which is what a Python dict literal and `json.loads` both do.
    pub fn insert(&mut self, key: String, value: V) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|(candidate, _)| *candidate == key)
        {
            existing.1 = value;
            return;
        }
        self.entries.push((key, value));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Keeps only the entries a predicate accepts, in place and in order.
    pub fn retain<F: FnMut(&str, &V) -> bool>(&mut self, mut keep: F) {
        self.entries.retain(|(key, value)| keep(key, value));
    }
}

impl<V> FromIterator<(String, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(iterator: I) -> Self {
        let mut map = Self::new();
        for (key, value) in iterator {
            map.insert(key, value);
        }
        map
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor<V>(std::marker::PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = OrderedMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut map = OrderedMap::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    map.insert(key, value);
                }
                Ok(map)
            }
        }

        deserializer.deserialize_map(MapVisitor(std::marker::PhantomData))
    }
}

// --------------------------------------------------------------------------
// The value
// --------------------------------------------------------------------------

/// A decoded JSON value, with object key order preserved.
///
/// [`JsonValue::Null`] is what an absent field decodes to, which is how the
/// reference's `force: Any = None` cannot tell an explicit `null` from a
/// missing key either.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum JsonValue {
    #[default]
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<JsonValue>),
    Object(OrderedMap<JsonValue>),
}

impl JsonValue {
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The string this value carries, or [`None`] when it is not a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// The object this value carries, or [`None`] when it is not one.
    #[must_use]
    pub const fn as_object(&self) -> Option<&OrderedMap<Self>> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// The text `json.dumps` gives this value with its default settings: a
    /// space after every separator, and every non-ASCII character escaped.
    #[must_use]
    pub fn python_json(&self) -> String {
        let mut rendered = String::new();
        self.write_python_json(&mut rendered);
        rendered
    }

    fn write_python_json(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Number(number) => out.push_str(&number.to_string()),
            Self::String(text) => write_python_string(out, text),
            Self::Array(items) => {
                out.push('[');
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        out.push_str(", ");
                    }
                    item.write_python_json(out);
                }
                out.push(']');
            }
            Self::Object(entries) => {
                out.push('{');
                for (position, (key, value)) in entries.iter().enumerate() {
                    if position > 0 {
                        out.push_str(", ");
                    }
                    write_python_string(out, key);
                    out.push_str(": ");
                    value.write_python_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// One string as `json.dumps` writes it: the two mandatory escapes, the five
/// short control escapes, `\uXXXX` for every other control character, and
/// `\uXXXX` for every non-ASCII character because `ensure_ascii` defaults to
/// true. A character outside the basic multilingual plane is written as the
/// surrogate pair Python writes.
fn write_python_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            other if other.is_ascii_control() => push_unicode_escape(out, other as u16),
            other if other.is_ascii() => out.push(other),
            other => {
                let mut units = [0_u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    push_unicode_escape(out, *unit);
                }
            }
        }
    }
    out.push('"');
}

/// One `\uXXXX` escape, in the lowercase hexadecimal Python writes.
fn push_unicode_escape(out: &mut String, unit: u16) {
    out.push_str("\\u");
    for shift in [12_u32, 8, 4, 0] {
        let nibble = u32::from((unit >> shift) & 0xf);
        // The mask keeps every nibble below 16, so the conversion is total.
        out.push(char::from_digit(nibble, 16).unwrap_or('0'));
    }
}

impl Serialize for JsonValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(number) => number.serialize(serializer),
            Self::String(text) => serializer.serialize_str(text),
            Self::Array(items) => {
                let mut sequence = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    sequence.serialize_element(item)?;
                }
                sequence.end()
            }
            Self::Object(entries) => entries.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = JsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("any JSON value")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(JsonValue::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(JsonValue::Null)
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                deserializer.deserialize_any(self)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(JsonValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(JsonValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(JsonValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(serde_json::Number::from_f64(value).map_or(JsonValue::Null, JsonValue::Number))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(JsonValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(JsonValue::String(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = access.next_element()? {
                    items.push(item);
                }
                Ok(JsonValue::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries = OrderedMap::new();
                while let Some((key, value)) = access.next_entry::<String, JsonValue>()? {
                    entries.insert(key, value);
                }
                Ok(JsonValue::Object(entries))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}
