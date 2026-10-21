//! How the Mistral client reads an answer back: a whole body, or a stream of
//! events.
//!
//! A body or an event is validated as the client's response models validate
//! it, content arrives as text or as a list of text and `thinking` chunks, and
//! a stream is split on blank lines of any newline style and ends on
//! `[DONE]`.

use serde_json::{Map, Value};

use crate::llm::adapter::{lax_count, truthy};
use crate::llm::error::{BackendFailure, LocalFailure, LocalKind};
use crate::llm::pump::EventSource;
use crate::llm::types::{Chunk, Message, StopInfo, ToolCall, Usage};
use crate::pycompat::{self, Dumps};

/// The text and the reasoning of a content value.
fn parse_content(content: &Value) -> Result<(String, Option<String>), LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::Validation, what.to_owned());
    match content {
        Value::String(text) => Ok((text.clone(), None)),
        Value::Array(chunks) => {
            let mut text = String::new();
            let mut reasoning = String::new();
            for chunk in chunks {
                let chunk = chunk
                    .as_object()
                    .ok_or_else(|| refused("a content chunk is not an object"))?;
                match chunk.get("type").and_then(Value::as_str).unwrap_or("text") {
                    "text" => text.push_str(
                        chunk
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| refused("a text chunk carries no text"))?,
                    ),
                    "thinking" => {
                        let inner = chunk
                            .get("thinking")
                            .and_then(Value::as_array)
                            .ok_or_else(|| refused("a thinking chunk carries no list"))?;
                        for part in inner {
                            if let Some(part) = part.as_object()
                                && part.get("type").and_then(Value::as_str).unwrap_or("text")
                                    == "text"
                                && let Some(piece) = part.get("text").and_then(Value::as_str)
                            {
                                reasoning.push_str(piece);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok((text, (!reasoning.is_empty()).then_some(reasoning)))
        }
        _ => Err(refused("content is neither text nor a list of chunks")),
    }
}

/// The tool calls of a delta or a message. Arguments given as an object are
/// written back as `json.dumps` text, keys in the order the provider sent them.
fn parse_tool_calls(calls: &[Value]) -> Result<Vec<ToolCall>, LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::Validation, what.to_owned());
    calls
        .iter()
        .map(|call| {
            let call = call
                .as_object()
                .ok_or_else(|| refused("a tool call is not an object"))?;
            let function = call
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| refused("a tool call carries no function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| refused("a tool call carries no name"))?;
            let arguments = match function.get("arguments") {
                Some(Value::String(arguments)) => arguments.clone(),
                Some(arguments @ Value::Object(_)) => pycompat::dumps(
                    arguments,
                    Dumps {
                        ensure_ascii: false,
                        ..Dumps::DEFAULT
                    },
                ),
                _ => return Err(refused("a tool call carries no arguments")),
            };
            let id = match call.get("id") {
                None => Some("null".to_owned()),
                Some(Value::Null) => None,
                Some(Value::String(id)) => Some(id.clone()),
                Some(_) => return Err(refused("a tool call id is not a string")),
            };
            let index = match call.get("index") {
                None => Some(0),
                Some(Value::Null) => None,
                Some(value) => Some(lax_count(value)?),
            };
            Ok(ToolCall {
                id,
                index,
                name: Some(name.to_owned()),
                arguments: Some(arguments),
            })
        })
        .collect()
}

/// `usage.prompt_tokens_details.cached_tokens`, read leniently: anything odd
/// counts as zero.
fn cached_tokens(usage: &Map<String, Value>) -> u64 {
    usage
        .get("prompt_tokens_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("cached_tokens"))
        .and_then(|value| match value {
            Value::String(text) => text.trim().parse::<u64>().ok(),
            other if truthy(other) => lax_count(other).ok(),
            _ => Some(0),
        })
        .unwrap_or(0)
}

fn usage_of(usage: Option<&Value>) -> Result<Usage, LocalFailure> {
    let Some(usage) = usage.filter(|usage| !usage.is_null()) else {
        return Ok(Usage::default());
    };
    let usage = usage
        .as_object()
        .ok_or_else(|| LocalFailure::new(LocalKind::Validation, "usage is not an object"))?;
    let count = |name: &str| match usage.get(name) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => lax_count(value),
    };
    Ok(Usage {
        prompt_tokens: count("prompt_tokens")?,
        completion_tokens: count("completion_tokens")?,
        cached_tokens: cached_tokens(usage),
    })
}

/// One streamed event's chunk.
fn event_chunk(text: &str, correlation_id: Option<&String>) -> Result<Chunk, LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::Validation, what.to_owned());
    let data: Value = serde_json::from_str(text).map_err(|_| refused("an event is not JSON"))?;
    let data = data
        .as_object()
        .ok_or_else(|| refused("an event is not an object"))?;
    if !data.get("id").is_some_and(Value::is_string)
        || !data.get("model").is_some_and(Value::is_string)
    {
        return Err(refused("an event carries no id or model"));
    }
    let choices = data
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| refused("an event carries no choices"))?;
    for choice in choices {
        let choice = choice
            .as_object()
            .ok_or_else(|| refused("a choice is not an object"))?;
        if !choice.get("index").is_some_and(Value::is_number)
            || !choice.get("delta").is_some_and(Value::is_object)
            || !choice
                .get("finish_reason")
                .is_some_and(|reason| reason.is_null() || reason.is_string())
        {
            return Err(refused("a choice is malformed"));
        }
    }
    let usage = usage_of(data.get("usage"))?;
    let Some(choice) = choices.first().and_then(Value::as_object) else {
        return Ok(Chunk {
            message: Message::assistant().with_content(""),
            usage: Some(usage),
            correlation_id: correlation_id.cloned(),
            stop: None,
        });
    };
    let delta = choice
        .get("delta")
        .and_then(Value::as_object)
        .ok_or_else(|| refused("a choice carries no delta"))?;
    let (content, reasoning) = match delta.get("content") {
        Some(content) if truthy(content) => parse_content(content)?,
        _ => (String::new(), None),
    };
    let tool_calls = match delta.get("tool_calls") {
        Some(Value::Array(calls)) if !calls.is_empty() => Some(parse_tool_calls(calls)?),
        _ => None,
    };
    Ok(Chunk {
        message: Message {
            content: Some(content),
            reasoning_content: reasoning,
            tool_calls,
            ..Message::assistant()
        },
        usage: Some(usage),
        correlation_id: correlation_id.cloned(),
        stop: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(StopInfo::reason),
    })
}

/// A whole answer.
pub(super) fn whole_answer(text: &str) -> Result<Chunk, LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::MistralResponse, what.to_owned());
    let data: Value = serde_json::from_str(text).map_err(|_| refused("the answer is not JSON"))?;
    let data = data
        .as_object()
        .ok_or_else(|| refused("the answer is not an object"))?;
    for field in ["id", "object", "model"] {
        if !data.get(field).is_some_and(Value::is_string) {
            return Err(refused("the answer is missing a field"));
        }
    }
    if !data.get("created").is_some_and(Value::is_number)
        || !data.get("usage").is_some_and(Value::is_object)
    {
        return Err(refused("the answer is missing a field"));
    }
    let choices = data
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| refused("the answer carries no choices"))?;
    let choice = choices
        .first()
        .and_then(Value::as_object)
        .ok_or_else(|| LocalFailure::new(LocalKind::Value, "the answer carries no choice"))?;
    let reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("a choice carries no finish reason"))?;
    let message = match choice.get("message") {
        None | Some(Value::Null) => None,
        Some(Value::Object(message)) => Some(message),
        Some(_) => return Err(refused("a choice's message is not an object")),
    };
    let (content, reasoning) = match message.and_then(|message| message.get("content")) {
        Some(content) if truthy(content) => parse_content(content)?,
        _ => (String::new(), None),
    };
    let tool_calls = match message.and_then(|message| message.get("tool_calls")) {
        Some(Value::Array(calls)) if !calls.is_empty() => Some(parse_tool_calls(calls)?),
        _ => None,
    };
    Ok(Chunk {
        message: Message {
            content: Some(content),
            reasoning_content: reasoning,
            tool_calls,
            ..Message::assistant()
        },
        usage: Some(usage_of(data.get("usage"))?),
        correlation_id: None,
        stop: Some(StopInfo::reason(reason)),
    })
}

/// The client's event framing: blocks separated by a blank line in any
/// newline style, `data` lines joined by newlines, `[DONE]` as the sentinel.
pub(super) struct EventReader {
    buffer: Vec<u8>,
    started: bool,
    done: bool,
    correlation_id: Option<String>,
}

/// The separators a block ends with, longest first where they share a prefix.
const BOUNDARIES: &[&[u8]] = &[
    b"\r\n\r\n",
    b"\r\n\r",
    b"\r\n\n",
    b"\r\r\n",
    b"\n\r\n",
    b"\r\r",
    b"\n\r",
    b"\n\n",
];

enum Block {
    Skip,
    Done,
    Event(String),
}

impl EventReader {
    /// A reader for one stream, whose every chunk carries the stream's
    /// correlation identifier.
    pub(super) const fn new(correlation_id: Option<String>) -> Self {
        Self {
            buffer: Vec::new(),
            started: false,
            done: false,
            correlation_id,
        }
    }

    fn block(raw: &[u8]) -> Result<Block, LocalFailure> {
        let text = std::str::from_utf8(raw)
            .map_err(|_| LocalFailure::new(LocalKind::Value, "an event is not UTF-8"))?;
        let mut data = String::new();
        let mut publish = false;
        for line in split_lines(text) {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.find(':') {
                Some(position) => {
                    let value = &line[position + 1..];
                    (&line[..position], value.strip_prefix(' ').unwrap_or(value))
                }
                None => (line, ""),
            };
            match field {
                "event" | "id" | "retry" => publish = true,
                "data" => {
                    data.push_str(value);
                    data.push('\n');
                    publish = true;
                }
                _ => {}
            }
        }
        if data == "[DONE]\n" {
            return Ok(Block::Done);
        }
        if data.is_empty() || !publish {
            return Ok(Block::Skip);
        }
        data.pop();
        Ok(Block::Event(data))
    }

    fn release(&mut self, raw: &[u8]) -> Result<Option<Chunk>, BackendFailure> {
        match Self::block(raw)? {
            Block::Skip => Ok(None),
            Block::Done => {
                self.done = true;
                Ok(None)
            }
            Block::Event(data) => event_chunk(&data, self.correlation_id.as_ref())
                .map(Some)
                .map_err(Into::into),
        }
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..index]);
                start = index + 1;
            }
            b'\r' => {
                lines.push(&text[start..index]);
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    lines.push(&text[start..]);
    lines
}

impl EventSource for EventReader {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Chunk>, BackendFailure> {
        let mut bytes = bytes;
        if !self.started && self.buffer.is_empty() {
            bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        }
        self.started = true;
        self.buffer.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut position = 0;
        let mut index = 0;
        while index < self.buffer.len() && !self.done {
            let byte = self.buffer[index];
            if byte == b'\r' || byte == b'\n' {
                let boundary = BOUNDARIES
                    .iter()
                    .find(|boundary| self.buffer[index..].starts_with(boundary));
                if let Some(boundary) = boundary {
                    let raw = self.buffer[position..index].to_vec();
                    position = index + boundary.len();
                    index = position;
                    if let Some(chunk) = self.release(&raw)? {
                        chunks.push(chunk);
                    }
                    continue;
                }
            }
            index += 1;
        }
        self.buffer.drain(..position);
        Ok(chunks)
    }

    fn finish(&mut self) -> Result<Vec<Chunk>, BackendFailure> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        let raw = std::mem::take(&mut self.buffer);
        Ok(self.release(&raw)?.into_iter().collect())
    }

    fn done(&self) -> bool {
        self.done
    }
}
