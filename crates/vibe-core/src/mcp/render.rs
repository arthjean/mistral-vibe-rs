//! What an MCP call hands back: reference `MCPToolResult`, and the text the
//! model reads for it.
//!
//! Reference `_parse_call_result` (`vibe/core/tools/mcp/tools.py`) keeps the
//! structured content when the server sent any and otherwise joins the text
//! blocks, and ignores the result's error flag: a result the server marked as
//! failed still reaches the model as `ok: True`. The agent loop renders the
//! model one `field: value` line per field, which spells the structured
//! content as Python's `repr` of a dictionary, keys in the order the server
//! sent them.

use std::fmt::Write as _;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};

use crate::tools::reference_text::{boolean, joined};

/// A JSON value that remembers the order its object keys arrived in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ordered {
    Null,
    Bool(bool),
    Integer(i128),
    Float(f64),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(OrderedVisitor)
    }
}

struct OrderedVisitor;

impl<'de> Visitor<'de> for OrderedVisitor {
    type Value = Ordered;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
        Ok(Ordered::Integer(i128::from(value)))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Ordered, E> {
        Ok(Ordered::Integer(i128::from(value)))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Ordered, E> {
        Ok(Ordered::Float(value))
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
        let mut entries: Vec<(String, Ordered)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Ordered>()? {
            // A repeated key keeps its first position and its last value, as
            // a Python dictionary built from the pairs does.
            match entries.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = value,
                None => entries.push((key, value)),
            }
        }
        Ok(Ordered::Object(entries))
    }
}

impl Ordered {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(flag) => Self::Bool(*flag),
            Value::Number(number) => number
                .as_i64()
                .map(i128::from)
                .or_else(|| number.as_u64().map(i128::from))
                .map_or_else(
                    || Self::Float(number.as_f64().unwrap_or_default()),
                    Self::Integer,
                ),
            Value::String(text) => Self::String(text.clone()),
            Value::Array(items) => Self::Array(items.iter().map(Self::from_value).collect()),
            Value::Object(fields) => Self::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from_value(value)))
                    .collect(),
            ),
        }
    }

    /// Python's `repr` of the value the reference parses this JSON into.
    pub(crate) fn repr(&self) -> String {
        let mut rendered = String::new();
        self.write_repr(&mut rendered);
        rendered
    }

    fn write_repr(&self, rendered: &mut String) {
        match self {
            Self::Null => rendered.push_str("None"),
            Self::Bool(flag) => rendered.push_str(boolean(*flag)),
            Self::Integer(integer) => {
                let _ = write!(rendered, "{integer}");
            }
            Self::Float(float) => rendered.push_str(&python_float(*float)),
            Self::String(text) => rendered.push_str(&python_string(text)),
            Self::Array(items) => {
                rendered.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        rendered.push_str(", ");
                    }
                    item.write_repr(rendered);
                }
                rendered.push(']');
            }
            Self::Object(entries) => {
                rendered.push('{');
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        rendered.push_str(", ");
                    }
                    rendered.push_str(&python_string(key));
                    rendered.push_str(": ");
                    value.write_repr(rendered);
                }
                rendered.push('}');
            }
        }
    }
}

/// Python's `repr` of a float: the shortest text that reads back as the same
/// value, fixed-point between `1e-4` and `1e16` and scientific outside.
pub(crate) fn python_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent = exponent.parse::<i32>().unwrap_or_default();
    let negative = mantissa.starts_with('-');
    let digits = mantissa
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    let mut rendered = String::from(if negative { "-" } else { "" });
    if (-4..16).contains(&exponent) {
        if exponent >= 0 {
            let integer_len = usize::try_from(exponent).unwrap_or_default() + 1;
            let padded = if digits.len() < integer_len {
                format!("{digits:0<integer_len$}")
            } else {
                digits.clone()
            };
            let (integer, fraction) = padded.split_at(integer_len);
            rendered.push_str(integer);
            rendered.push('.');
            rendered.push_str(if fraction.is_empty() { "0" } else { fraction });
        } else {
            rendered.push_str("0.");
            for _ in 0..(-exponent - 1) {
                rendered.push('0');
            }
            rendered.push_str(&digits);
        }
    } else {
        let (first, rest) = digits.split_at(1.min(digits.len()));
        rendered.push_str(first);
        if !rest.is_empty() {
            rendered.push('.');
            rendered.push_str(rest);
        }
        let _ = write!(
            rendered,
            "e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.unsigned_abs()
        );
    }
    rendered
}

/// Python's `repr` of a string: single quotes unless the text holds one and no
/// double quote, and an escape for every character `str.isprintable` refuses.
pub(crate) fn python_string(text: &str) -> String {
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
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            other if other == quote => {
                rendered.push('\\');
                rendered.push(other);
            }
            other if !printable(other) => {
                let code = u32::from(other);
                let _ = match code {
                    0..=0xff => write!(rendered, "\\x{code:02x}"),
                    0x100..=0xffff => write!(rendered, "\\u{code:04x}"),
                    _ => write!(rendered, "\\U{code:08x}"),
                };
            }
            other => rendered.push(other),
        }
    }
    rendered.push(quote);
    rendered
}

/// `str.isprintable` for the characters a tool result plausibly carries: the
/// control ranges, the separators other than the ASCII space, and the common
/// format characters.
fn printable(character: char) -> bool {
    !matches!(
        u32::from(character),
        0x00..=0x1f
            | 0x7f..=0xa0
            | 0xad
            | 0x0600..=0x0605
            | 0x061c
            | 0x06dd
            | 0x070f
            | 0x1680
            | 0x180e
            | 0x2000..=0x200f
            | 0x2028..=0x202f
            | 0x205f..=0x2064
            | 0x2066..=0x206f
            | 0x3000
            | 0xd800..=0xf8ff
            | 0xfeff
            | 0xfff9..=0xfffb
            | 0xe0001
            | 0xe0020..=0xe007f
            | 0xf0000..=0x10ffff
    )
}

/// Reference `MCPToolResult` for one answered call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct McpToolResult {
    pub(crate) server: String,
    pub(crate) tool: String,
    pub(crate) text: Option<String>,
    pub(crate) structured: Option<Ordered>,
}

impl McpToolResult {
    /// Reference `_parse_call_result` over a validated `CallToolResult`.
    ///
    /// `raw` is the JSON-RPC message the result arrived in; the structured
    /// content is read from it again so its keys keep the server's order.
    pub(crate) fn parse(server: String, tool: String, result: &Value, raw: &str) -> Self {
        let structured = result
            .get("structuredContent")
            .filter(|structured| structured.is_object())
            .map(|structured| {
                structured_in_order(raw).unwrap_or_else(|| Ordered::from_value(structured))
            });
        if structured.is_some() {
            return Self {
                server,
                tool,
                text: None,
                structured,
            };
        }
        let parts = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self {
            server,
            tool,
            text: (!parts.is_empty()).then(|| parts.join("\n")),
            structured: None,
        }
    }

    /// The text the model reads: one `field: value` line per field.
    pub(crate) fn model_text(&self) -> String {
        joined(&[
            ("ok", boolean(true).to_owned()),
            ("server", self.server.clone()),
            ("tool", self.tool.clone()),
            (
                "text",
                self.text.clone().unwrap_or_else(|| "None".to_owned()),
            ),
            (
                "structured",
                self.structured
                    .as_ref()
                    .map_or_else(|| "None".to_owned(), Ordered::repr),
            ),
        ])
    }

    /// The typed result, `MCPToolResult.model_dump(mode="json")`.
    pub(crate) fn typed(&self) -> Value {
        json!({
            "ok": true,
            "server": self.server,
            "tool": self.tool,
            "text": self.text,
            "structured": self.structured.as_ref().map(Ordered::to_value),
        })
    }
}

impl Ordered {
    fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(flag) => Value::Bool(*flag),
            Self::Integer(integer) => i64::try_from(*integer)
                .map(Value::from)
                .or_else(|_| u64::try_from(*integer).map(Value::from))
                .unwrap_or(Value::Null),
            Self::Float(float) => json!(float),
            Self::String(text) => Value::String(text.clone()),
            Self::Array(items) => Value::Array(items.iter().map(Self::to_value).collect()),
            Self::Object(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_value()))
                    .collect(),
            ),
        }
    }
}

fn structured_in_order(raw: &str) -> Option<Ordered> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        result: Option<ResultPart>,
    }
    #[derive(serde::Deserialize)]
    struct ResultPart {
        #[serde(rename = "structuredContent")]
        structured_content: Option<Ordered>,
    }
    serde_json::from_str::<Envelope>(raw)
        .ok()?
        .result?
        .structured_content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_render_as_python_repr() {
        for (value, expected) in [
            (2.5, "2.5"),
            (1.0, "1.0"),
            (100.0, "100.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e16, "1e+16"),
            (1.234_567_890_123_456_8e17, "1.2345678901234568e+17"),
            (9_999_999_999_999_998.0, "9999999999999998.0"),
            (-0.5, "-0.5"),
            (0.1, "0.1"),
            (123.456, "123.456"),
        ] {
            assert_eq!(python_float(value), expected, "{value}");
        }
    }

    #[test]
    fn structured_content_keeps_the_server_key_order() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[],"structuredContent":{"z":1,"a":[true,null,"x'y",2.5]}}}"#;
        let message: Value = serde_json::from_str(raw).expect("fixture");
        let result = McpToolResult::parse(
            "stdio:server".to_owned(),
            "tool".to_owned(),
            &message["result"],
            raw,
        );
        assert_eq!(
            result.model_text(),
            "ok: True\nserver: stdio:server\ntool: tool\ntext: None\nstructured: {'z': 1, 'a': [True, None, \"x'y\", 2.5]}"
        );
    }

    #[test]
    fn text_blocks_join_and_an_error_flag_is_ignored() {
        let result = json!({
            "content": [
                {"type": "text", "text": "one"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "text", "text": "two"},
            ],
            "isError": true,
        });
        let parsed = McpToolResult::parse("s".to_owned(), "t".to_owned(), &result, "");
        assert_eq!(parsed.text.as_deref(), Some("one\ntwo"));
        let image_only = json!({"content": [{"type": "image", "data": "", "mimeType": "x"}]});
        let parsed = McpToolResult::parse("s".to_owned(), "t".to_owned(), &image_only, "");
        assert_eq!(parsed.text, None);
    }

    #[test]
    fn strings_escape_what_python_refuses_to_print() {
        assert_eq!(
            python_string("a\u{0}b\u{a0}c\u{2028}"),
            "'a\\x00b\\xa0c\\u2028'"
        );
        assert_eq!(python_string("it's"), "\"it's\"");
        assert_eq!(python_string("\"it's\""), "'\"it\\'s\"'");
        assert_eq!(python_string("é"), "'é'");
    }
}
