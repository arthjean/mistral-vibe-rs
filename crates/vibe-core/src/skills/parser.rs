//! The frontmatter parser, reproducing the reference's boundary rules over a
//! real YAML parse.
//!
//! The boundary contract is parity surface and stays hand-written: the fence
//! is `^-{3,}\s*$` applied per line, the document splits three ways on it, any
//! non-whitespace text before the first fence rejects the file, a leading byte
//! order mark is stripped first, and fewer than two fences reject the file
//! (`vibe/core/skills/parser.py:15-40` at the pinned commit). Only the YAML
//! block itself is delegated, to the event parser `serde-saphyr` re-exports,
//! because the reference resolves plain scalars the PyYAML way and no YAML 1.2
//! deserializer answers that: `no` unquoted is a boolean there while `"no"`
//! quoted and the single letter `y` both stay strings. The resolver below
//! reproduces that vocabulary over the event stream, where quoting is still
//! visible.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Number, Value};
use serde_saphyr::granit_parser::{Event, Parser, ScalarStyle};
use thiserror::Error;

/// Which contract a skill document broke, mirroring the three rejection
/// classes the reference distinguishes: the fence layout, the YAML text, and a
/// document that parsed to something other than a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillParseErrorKind {
    Boundary,
    Yaml,
    Mapping,
}

/// A skill document the parser rejected, with the reason a diagnostic can
/// carry verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct SkillParseError {
    pub kind: SkillParseErrorKind,
    pub message: String,
}

impl SkillParseError {
    fn boundary() -> Self {
        Self {
            kind: SkillParseErrorKind::Boundary,
            message: "the frontmatter must open and close with a `---` fence, with nothing \
                      before the first one"
                .to_owned(),
        }
    }

    fn yaml(reason: impl std::fmt::Display) -> Self {
        Self {
            kind: SkillParseErrorKind::Yaml,
            message: format!("the frontmatter is not parseable YAML: {reason}"),
        }
    }

    fn mapping() -> Self {
        Self {
            kind: SkillParseErrorKind::Mapping,
            message: "the frontmatter must be a YAML mapping".to_owned(),
        }
    }
}

/// The fence line, matched per line so a fence of three or more hyphens with
/// trailing whitespace splits the document exactly where the reference splits
/// it.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time constant that compiles"
)]
static FRONTMATTER_BOUNDARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^-{3,}\s*$").expect("the fence pattern compiles"));

/// Splits a `SKILL.md` document into its frontmatter mapping and its body.
///
/// The body is returned verbatim, exactly as the text after the closing fence;
/// trimming it into the published prompt is the caller's contract.
pub fn parse_skill_markdown(
    content: &str,
) -> Result<(Map<String, Value>, String), SkillParseError> {
    let content = content.trim_start_matches('\u{feff}');
    let mut splits = FRONTMATTER_BOUNDARY.splitn(content, 3);
    let prefix = splits.next().unwrap_or_default();
    let (Some(frontmatter), Some(body)) = (splits.next(), splits.next()) else {
        return Err(SkillParseError::boundary());
    };
    if !prefix.trim().is_empty() {
        return Err(SkillParseError::boundary());
    }
    match yaml_document(frontmatter)? {
        Value::Null => Ok((Map::new(), body.to_owned())),
        Value::Object(mapping) => Ok((mapping, body.to_owned())),
        _ => Err(SkillParseError::mapping()),
    }
}

/// One container under construction while the event stream is walked, with
/// the anchor it will register on completion.
enum Node {
    Sequence(Vec<Value>, usize),
    Mapping(Map<String, Value>, Option<String>, usize),
}

/// Parses one YAML document into a JSON value, resolving plain scalars the
/// PyYAML way. An empty stream is the null document, which the caller reads as
/// an empty mapping.
fn yaml_document(text: &str) -> Result<Value, SkillParseError> {
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Value> = None;
    let mut anchors: BTreeMap<usize, Value> = BTreeMap::new();
    let mut documents = 0_usize;
    for item in Parser::new_from_str(text) {
        let (event, _span) = item.map_err(SkillParseError::yaml)?;
        match event {
            Event::StreamStart | Event::StreamEnd | Event::DocumentEnd | Event::Comment(..) => {}
            Event::DocumentStart(..) => {
                documents += 1;
                if documents > 1 {
                    return Err(SkillParseError::yaml(
                        "the frontmatter holds more than one document",
                    ));
                }
            }
            Event::Scalar(value, style, anchor, _tag) => {
                let resolved = if style == ScalarStyle::Plain {
                    resolve_plain(&value)
                } else {
                    Value::String(value.into_owned())
                };
                if anchor != 0 {
                    anchors.insert(anchor, resolved.clone());
                }
                attach(&mut stack, &mut root, resolved)?;
            }
            Event::SequenceStart(_, anchor, _tag) => stack.push(Node::Sequence(Vec::new(), anchor)),
            Event::MappingStart(_, anchor, _tag) => {
                stack.push(Node::Mapping(Map::new(), None, anchor));
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (built, anchor) = match stack.pop() {
                    Some(Node::Sequence(items, anchor)) => (Value::Array(items), anchor),
                    Some(Node::Mapping(entries, _, anchor)) => (Value::Object(entries), anchor),
                    None => return Err(SkillParseError::yaml("unbalanced container events")),
                };
                if anchor != 0 {
                    anchors.insert(anchor, built.clone());
                }
                attach(&mut stack, &mut root, built)?;
            }
            Event::Alias(anchor) => {
                let value = anchors
                    .get(&anchor)
                    .cloned()
                    .ok_or_else(|| SkillParseError::yaml("alias to an undefined anchor"))?;
                attach(&mut stack, &mut root, value)?;
            }
            // `Event` is non-exhaustive; a construct this walker does not
            // know is a rejection, never a silently wrong tree.
            _ => return Err(SkillParseError::yaml("unsupported YAML construct")),
        }
    }
    Ok(root.unwrap_or(Value::Null))
}

/// Places one completed value: into the open container, or as the document
/// root. Inside a mapping, values alternate between key and value positions,
/// and a later duplicate key overwrites the earlier one, as PyYAML resolves
/// it.
fn attach(
    stack: &mut [Node],
    root: &mut Option<Value>,
    value: Value,
) -> Result<(), SkillParseError> {
    match stack.last_mut() {
        None => {
            if root.is_some() {
                return Err(SkillParseError::yaml("more than one root node"));
            }
            *root = Some(value);
        }
        Some(Node::Sequence(items, _)) => items.push(value),
        Some(Node::Mapping(entries, pending, _)) => match pending.take() {
            None => *pending = Some(mapping_key(&value)),
            Some(key) => {
                entries.insert(key, value);
            }
        },
    }
    Ok(())
}

/// A mapping key as JSON spells it, which is also how the corpus records a
/// non-string key: `1: one` keys on `"1"`.
fn mapping_key(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Resolves one plain scalar with PyYAML's vocabulary: its boolean set is the
/// YAML 1.1 words in their three exact capitalizations, without the
/// single-letter `y` and `n` the 1.1 specification also names, so those stay
/// strings here exactly as they do upstream.
fn resolve_plain(text: &str) -> Value {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => {
            return Value::Bool(true);
        }
        "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => {
            return Value::Bool(false);
        }
        _ => {}
    }
    resolve_number(text).unwrap_or_else(|| Value::String(text.to_owned()))
}

/// The integer forms PyYAML resolves: binary, hexadecimal, legacy
/// leading-zero octal, and decimal, each with `_` separators.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time constant that compiles"
)]
static PLAIN_INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[-+]?(0b[0-1_]+|0x[0-9a-fA-F_]+|0[0-7_]+|0|[1-9][0-9_]*)$")
        .expect("the integer pattern compiles")
});

/// The float forms PyYAML resolves, minus the sexagesimal and non-finite
/// spellings JSON cannot carry: a dot is mandatory and an exponent needs an
/// explicit sign, so `1e5` stays a string upstream and here alike.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time constant that compiles"
)]
static PLAIN_FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[-+]?([0-9][0-9_]*\.[0-9_]*|\.[0-9_]+)([eE][-+][0-9]+)?$")
        .expect("the float pattern compiles")
});

fn resolve_number(text: &str) -> Option<Value> {
    if PLAIN_INT.is_match(text) {
        let negative = text.starts_with('-');
        let body = text.strip_prefix(['-', '+']).unwrap_or(text);
        let digits: String = body.chars().filter(|character| *character != '_').collect();
        let magnitude = if let Some(binary) = digits.strip_prefix("0b") {
            i64::from_str_radix(binary, 2)
        } else if let Some(hexadecimal) = digits.strip_prefix("0x") {
            i64::from_str_radix(hexadecimal, 16)
        } else if digits != "0" && digits.starts_with('0') {
            i64::from_str_radix(&digits, 8)
        } else {
            digits.parse()
        };
        return match magnitude {
            Ok(magnitude) => Some(Value::Number(Number::from(if negative {
                -magnitude
            } else {
                magnitude
            }))),
            // An integer wider than 64 bits is arbitrary-precision upstream;
            // this port has no such number, so the scalar stays a string.
            Err(_) => None,
        };
    }
    if PLAIN_FLOAT.is_match(text) {
        let digits: String = text.chars().filter(|character| *character != '_').collect();
        return digits
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number);
    }
    None
}
