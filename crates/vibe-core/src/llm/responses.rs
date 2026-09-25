//! The OpenAI Responses dialect.
//!
//! Reference `OpenAIResponsesAdapter` and `_OpenAIResponsesStreamParser`
//! (`vibe/core/llm/backend/openai_responses.py`). Requests ask for the
//! encrypted reasoning back and store nothing server side, so an assistant's
//! reasoning items are replayed verbatim before its text and calls. Streams
//! are event-typed: text, reasoning summaries and function-call arguments
//! arrive as deltas keyed by output index, a `commentary` message counts as
//! reasoning, a call is released once its name and arguments are known, and
//! the terminal event carries the usage and the reasoning items.
//!
//! Chunks are held back until the stream shows output, so a failure the
//! provider reports before any output is still retried.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::adapter::{PreparedRequest, RequestParts, bearer_headers, count_field, object_or_empty};
use super::error::{BackendFailure, LocalFailure, LocalKind};
use super::types::{Chunk, Message, Role, StopInfo, ToolCall, ToolChoice, Usage};

const ENDPOINT: &str = "/responses";

fn effort(thinking: &str) -> &str {
    match thinking {
        "off" => "none",
        "max" => "xhigh",
        other => other,
    }
}

fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        match message.role {
            Role::System => items.push(json!({"role": "system", "content": message.text()})),
            Role::User if !message.images.is_empty() => {
                let mut parts = Vec::new();
                if !message.text().is_empty() {
                    parts.push(json!({"type": "input_text", "text": message.text()}));
                }
                parts.extend(message.images.iter().map(
                    |picture| json!({"type": "input_image", "image_url": picture.data_uri()}),
                ));
                items.push(json!({"role": "user", "content": parts}));
            }
            Role::User => items.push(json!({"role": "user", "content": message.text()})),
            Role::Assistant => {
                items.extend(
                    message
                        .reasoning_payloads
                        .iter()
                        .flatten()
                        .filter(|item| {
                            item.get("type").and_then(Value::as_str) == Some("reasoning")
                        })
                        .map(|item| Value::Object(item.clone())),
                );
                // A message the model never wrote would sit between a
                // reasoning item and the call it belongs to.
                if !message.text().is_empty() {
                    items.push(json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": message.text()}],
                    }));
                }
                for call in message.tool_calls.iter().flatten() {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id.as_deref().unwrap_or_default(),
                        "name": call.name.as_deref().unwrap_or_default(),
                        "arguments": call.arguments.as_deref().unwrap_or_default(),
                    }));
                }
            }
            Role::Tool => items.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.as_deref().unwrap_or_default(),
                "output": message.text(),
            })),
        }
    }
    items
}

#[must_use]
pub fn prepare(parts: &RequestParts<'_>) -> PreparedRequest {
    let mut payload = Map::new();
    payload.insert("model".to_owned(), json!(parts.model_name));
    payload.insert(
        "input".to_owned(),
        Value::Array(input_items(parts.messages)),
    );
    payload.insert("store".to_owned(), Value::Bool(false));
    payload.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
    // Only the older chat models take a temperature on this API.
    if parts.model_name.starts_with("gpt-4") || parts.model_name.starts_with("gpt-3.5") {
        payload.insert("temperature".to_owned(), json!(parts.temperature));
    }
    payload.insert(
        "reasoning".to_owned(),
        json!({"effort": effort(parts.thinking)}),
    );
    if let Some(tools) = parts.declared_tools() {
        payload.insert(
            "tools".to_owned(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        })
                    })
                    .collect(),
            ),
        );
        if let Some(choice) = parts.tool_choice {
            payload.insert(
                "tool_choice".to_owned(),
                match choice {
                    ToolChoice::Tool(tool) => json!({"type": "function", "name": tool.name}),
                    keyword => json!(keyword.keyword()),
                },
            );
        }
    }
    if let Some(max_tokens) = parts.max_tokens {
        payload.insert("max_output_tokens".to_owned(), json!(max_tokens));
    }
    if parts.streaming {
        payload.insert("stream".to_owned(), Value::Bool(true));
    }
    PreparedRequest {
        path: ENDPOINT.to_owned(),
        headers: bearer_headers(parts.api_key),
        body: Value::Object(payload),
        base_url: None,
    }
}

/// An error a Responses stream reported in an event, with the HTTP status its
/// code stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    pub error_type: String,
    pub message: String,
    pub status: Option<u16>,
}

fn status_for(code: &str) -> Option<u16> {
    match code {
        "authentication_error" | "invalid_api_key" => Some(401),
        "too_many_requests" | "rate_limit" | "rate_limit_error" | "rate_limit_exceeded" => {
            Some(429)
        }
        "server_error" => Some(500),
        _ => None,
    }
}

fn empty() -> Chunk {
    Chunk {
        usage: Some(Usage::default()),
        ..Chunk::of(Message::assistant().with_content(""))
    }
}

fn text_chunk(text: String) -> Chunk {
    Chunk {
        usage: Some(Usage::default()),
        ..Chunk::of(Message::assistant().with_content(text))
    }
}

fn reasoning_chunk(text: String) -> Chunk {
    Chunk {
        usage: Some(Usage::default()),
        ..Chunk::of(Message {
            reasoning_content: Some(text),
            ..Message::assistant().with_content("")
        })
    }
}

fn call_chunk(
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    index: Option<u64>,
) -> Result<Chunk, LocalFailure> {
    let index = index.ok_or_else(missing_index)?;
    Ok(Chunk {
        usage: Some(Usage::default()),
        ..Chunk::of(Message {
            tool_calls: Some(vec![ToolCall {
                id: call_id,
                index: Some(index),
                name,
                arguments: Some(arguments),
            }]),
            ..Message::assistant().with_content("")
        })
    })
}

fn missing_index() -> LocalFailure {
    LocalFailure::new(LocalKind::Value, "a tool call piece carries no index")
}

fn refused(what: &str) -> LocalFailure {
    LocalFailure::new(LocalKind::Validation, what.to_owned())
}

/// An optional string field an event handler reads.
fn text(object: &Map<String, Value>, name: &str) -> Result<Option<String>, LocalFailure> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(Value::Null) if name == "code" || name == "message" => Ok(None),
        Some(_) => Err(refused("an event field is not a string")),
    }
}

/// An optional output index.
fn output_index(object: &Map<String, Value>) -> Result<Option<u64>, LocalFailure> {
    match object.get("output_index") {
        None => Ok(None),
        Some(value) => super::adapter::optional_index(Some(value)).and_then(|index| {
            index
                .ok_or_else(|| refused("an output index is null"))
                .map(Some)
        }),
    }
}

fn item_of(object: &Map<String, Value>) -> Result<Map<String, Value>, LocalFailure> {
    match object.get("item") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(item)) => Ok(item.clone()),
        Some(_) => Err(refused("an output item is not an object")),
    }
}

fn is_commentary(item: &Map<String, Value>) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("phase").and_then(Value::as_str) == Some("commentary")
}

fn usage_of(response: &Map<String, Value>) -> Result<Usage, LocalFailure> {
    let Some(usage) = object_or_empty(response, "usage")? else {
        return Ok(Usage::default());
    };
    let cached = match object_or_empty(usage, "input_tokens_details")? {
        Some(details) => count_field(details, "cached_tokens")?,
        None => 0,
    };
    Ok(Usage {
        prompt_tokens: count_field(usage, "input_tokens")?,
        completion_tokens: count_field(usage, "output_tokens")?,
        cached_tokens: cached,
    })
}

/// The reasoning items that carry encrypted content, which are the ones a
/// later request can replay.
fn reasoning_payloads(output: &[Value]) -> Result<Option<Vec<Map<String, Value>>>, LocalFailure> {
    let mut payloads = Vec::new();
    for item in output {
        let Some(item) = item.as_object() else {
            return Err(LocalFailure::new(
                LocalKind::Attribute,
                "an output item is not an object",
            ));
        };
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let encrypted = match item.get("encrypted_content") {
            None => false,
            Some(Value::String(content)) => !content.is_empty(),
            Some(_) => return Err(refused("encrypted reasoning is not a string")),
        };
        if encrypted {
            payloads.push(item.clone());
        }
    }
    Ok((!payloads.is_empty()).then_some(payloads))
}

fn output_of(response: &Map<String, Value>) -> Result<Vec<Value>, LocalFailure> {
    match response.get("output") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(refused("`output` is not a list")),
    }
}

#[derive(Debug, Clone, Default)]
struct CallState {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    name_emitted: bool,
    arguments_emitted: bool,
}

/// Reads one Responses answer, whole or streamed.
#[derive(Debug, Default)]
pub struct ResponsesReader {
    commentary: BTreeSet<u64>,
    calls: BTreeMap<u64, CallState>,
    pending: Vec<Chunk>,
    has_output: bool,
}

impl ResponsesReader {
    fn reset(&mut self) {
        self.commentary.clear();
        self.calls.clear();
    }

    /// The chunk one event or one whole body carries.
    ///
    /// # Errors
    ///
    /// An event that reports a failure, and fields of the wrong type.
    pub fn parse(&mut self, data: &Map<String, Value>) -> Result<Chunk, BackendFailure> {
        let kind = match data.get("type") {
            None => None,
            Some(Value::String(kind)) => Some(kind.as_str()),
            Some(_) => return Err(refused("an event type is not a string").into()),
        };
        if data.contains_key("output") && kind.is_none_or(str::is_empty) {
            return whole_response(data).map_err(Into::into);
        }
        let kind = kind.ok_or_else(|| refused("an event carries no type"))?;
        self.event(kind, data)
    }

    fn event(&mut self, kind: &str, data: &Map<String, Value>) -> Result<Chunk, BackendFailure> {
        Ok(match kind {
            "response.created" => {
                self.reset();
                empty()
            }
            "response.output_text.delta" => {
                let delta = text(data, "delta")?.unwrap_or_default();
                if self.commentary.contains(&output_index(data)?.unwrap_or(0)) {
                    reasoning_chunk(delta)
                } else {
                    text_chunk(delta)
                }
            }
            "response.reasoning_summary_text.delta" | "response.summary_text.delta" => {
                reasoning_chunk(text(data, "delta")?.unwrap_or_default())
            }
            "response.function_call_arguments.delta" => self.arguments_delta(data)?,
            "response.function_call_arguments.done" => {
                let index = output_index(data)?;
                self.finalize_call(
                    index,
                    text(data, "call_id")?,
                    text(data, "name")?,
                    text(data, "arguments")?,
                )?
            }
            "response.output_item.added" => self.item_added(data)?,
            "response.output_item.done" => {
                let item = item_of(data)?;
                match item.get("type").and_then(Value::as_str) {
                    Some("message") if is_commentary(&item) => {
                        self.commentary.insert(output_index(data)?.unwrap_or(0));
                        empty()
                    }
                    Some("function_call") => {
                        let call_id = text(&item, "call_id")?
                            .filter(|id| !id.is_empty())
                            .or(text(&item, "id")?);
                        self.finalize_call(
                            output_index(data)?,
                            call_id,
                            text(&item, "name")?,
                            text(&item, "arguments")?,
                        )?
                    }
                    _ => empty(),
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = match data.get("response") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(response)) => response.clone(),
                    Some(_) => return Err(refused("`response` is not an object").into()),
                };
                self.reset();
                Chunk {
                    message: Message {
                        reasoning_payloads: reasoning_payloads(&output_of(&response)?)?,
                        ..Message::assistant().with_content("")
                    },
                    usage: Some(usage_of(&response)?),
                    correlation_id: None,
                    stop: Some(StopInfo::reason(kind.trim_start_matches("response."))),
                }
            }
            "response.failed" | "error" => {
                self.reset();
                return Err(stream_error(data)?);
            }
            _ => empty(),
        })
    }

    fn arguments_delta(&mut self, data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
        let delta = text(data, "delta")?.unwrap_or_default();
        let name = text(data, "name")?;
        let call_id = text(data, "call_id")?;
        if delta.is_empty()
            && name.as_deref().is_none_or(str::is_empty)
            && call_id.as_deref().is_none_or(str::is_empty)
        {
            return Ok(empty());
        }
        let index = output_index(data)?.ok_or_else(missing_index)?;
        let state = self.calls.entry(index).or_default();
        if let Some(call_id) = call_id.filter(|id| !id.is_empty()) {
            state.call_id = Some(call_id);
        }
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            state.name = Some(name);
        }
        state.arguments.push_str(&delta);
        Ok(empty())
    }

    fn item_added(&mut self, data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
        let item = item_of(data)?;
        match item.get("type").and_then(Value::as_str) {
            Some("message") if is_commentary(&item) => {
                self.commentary.insert(output_index(data)?.unwrap_or(0));
                Ok(empty())
            }
            Some("function_call") => {
                let index = output_index(data)?;
                let call_id = text(&item, "call_id")?
                    .filter(|id| !id.is_empty())
                    .or(text(&item, "id")?);
                let name = text(&item, "name")?;
                if let Some(index) = index {
                    let state = self.calls.entry(index).or_default();
                    if let Some(call_id) = call_id.clone().filter(|id| !id.is_empty()) {
                        state.call_id = Some(call_id);
                    }
                    if let Some(name) = name.clone().filter(|name| !name.is_empty()) {
                        state.name = Some(name);
                    }
                    state.arguments = text(&item, "arguments")?.unwrap_or_default();
                    state.name_emitted = name.as_deref().is_some_and(|name| !name.is_empty());
                    state.arguments_emitted = false;
                }
                call_chunk(call_id, name, String::new(), index)
            }
            _ => Ok(empty()),
        }
    }

    fn finalize_call(
        &mut self,
        index: Option<u64>,
        call_id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    ) -> Result<Chunk, LocalFailure> {
        let index = index.ok_or_else(missing_index)?;
        let state = self.calls.entry(index).or_default();
        let call_id = call_id
            .filter(|id| !id.is_empty())
            .or(state.call_id.clone());
        let name = name.filter(|name| !name.is_empty()).or(state.name.clone());
        let arguments = arguments.unwrap_or_else(|| state.arguments.clone());
        let emit_name = name.is_some() && !state.name_emitted;
        let emit_arguments = !arguments.is_empty() && !state.arguments_emitted;
        if let Some(call_id) = call_id.clone() {
            state.call_id = Some(call_id);
        }
        if let Some(name) = name.clone() {
            state.name = Some(name);
        }
        state.arguments.clone_from(&arguments);
        state.name_emitted |= emit_name;
        state.arguments_emitted |= emit_arguments;
        if !emit_name && !emit_arguments {
            return Ok(empty());
        }
        call_chunk(
            call_id,
            name,
            if emit_arguments {
                arguments
            } else {
                String::new()
            },
            Some(index),
        )
    }

    /// The chunks one streamed event releases: none while the stream has
    /// shown no output yet, then everything held back at once.
    ///
    /// # Errors
    ///
    /// As [`ResponsesReader::parse`].
    pub fn stream_event(
        &mut self,
        data: &Map<String, Value>,
    ) -> Result<Vec<Chunk>, BackendFailure> {
        let chunk = self.parse(data)?;
        if self.has_output {
            return Ok(vec![chunk]);
        }
        if !shows_output(&chunk) {
            self.pending.push(chunk);
            return Ok(Vec::new());
        }
        self.has_output = true;
        let mut released = std::mem::take(&mut self.pending);
        released.push(chunk);
        Ok(released)
    }

    /// What was still held back when the stream ended.
    pub fn finish(&mut self) -> Vec<Chunk> {
        std::mem::take(&mut self.pending)
    }
}

fn shows_output(chunk: &Chunk) -> bool {
    let message = &chunk.message;
    message
        .content
        .as_deref()
        .is_some_and(|text| !text.is_empty())
        || message
            .reasoning_content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        || message
            .reasoning_payloads
            .as_ref()
            .is_some_and(|items| !items.is_empty())
        || message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
        || !message.images.is_empty()
        || chunk.stop.is_some()
}

fn stream_error(data: &Map<String, Value>) -> Result<BackendFailure, LocalFailure> {
    let response = match data.get("response") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(response)) => response.clone(),
        Some(_) => return Err(refused("`response` is not an object")),
    };
    let error = match response.get("error").filter(|error| !error.is_null()) {
        Some(error) => Some(error),
        None => data.get("error").filter(|error| !error.is_null()),
    };
    let error = match error {
        Some(Value::Object(error)) => error.clone(),
        Some(_) => return Err(refused("an error is not an object")),
        None => {
            let mut error = Map::new();
            error.insert(
                "code".to_owned(),
                data.get("code").cloned().unwrap_or(Value::Null),
            );
            error.insert(
                "message".to_owned(),
                data.get("message").cloned().unwrap_or(Value::Null),
            );
            error
        }
    };
    let pick = |name: &str| -> Result<Option<String>, LocalFailure> {
        match error.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok((!text.is_empty()).then(|| text.clone())),
            Some(_) => Err(refused("an error field is not a string")),
        }
    };
    let error_type = pick("code")?
        .or(pick("type")?)
        .unwrap_or_else(|| "unknown_error".to_owned());
    let message = pick("message")?.unwrap_or_else(|| "unknown streaming error".to_owned());
    Ok(BackendFailure::ResponsesStream(StreamError {
        status: status_for(&error_type),
        error_type,
        message,
    }))
}

fn whole_response(data: &Map<String, Value>) -> Result<Chunk, LocalFailure> {
    let output = match data.get("output") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => {
            return Err(LocalFailure::new(
                LocalKind::Value,
                "the Responses answer carries no output",
            ));
        }
        Some(_) => return Err(refused("`output` is not a list")),
    };
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut calls = Vec::new();
    for (index, item) in output.iter().enumerate() {
        let Some(item) = item.as_object() else {
            return Err(LocalFailure::new(
                LocalKind::Attribute,
                "an output item is not an object",
            ));
        };
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let commentary = is_commentary(item);
                let mut item_text = String::new();
                let mut item_reasoning = String::new();
                let blocks = match item.get("content") {
                    None => Vec::new(),
                    Some(Value::Array(blocks)) => blocks.clone(),
                    Some(_) => return Err(refused("message content is not a list")),
                };
                for block in &blocks {
                    let Some(block) = block.as_object() else {
                        return Err(refused("a content block is not an object"));
                    };
                    let block_type = block.get("type").and_then(Value::as_str);
                    let block_text = text(block, "text")?.unwrap_or_default();
                    if commentary
                        && matches!(
                            block_type,
                            Some("output_text" | "summary_text" | "reasoning_summary_text")
                        )
                    {
                        item_reasoning.push_str(&block_text);
                        continue;
                    }
                    if block_type == Some("output_text") {
                        item_text.push_str(&block_text);
                    }
                }
                if commentary {
                    if !item_reasoning.is_empty() {
                        reasoning_parts.push(item_reasoning);
                    }
                    continue;
                }
                if !item_text.is_empty() {
                    text_parts.push(item_text);
                }
            }
            Some("reasoning") => {
                let summaries = match item.get("summary") {
                    None => Vec::new(),
                    Some(Value::Array(summaries)) => summaries.clone(),
                    Some(_) => return Err(refused("a reasoning summary is not a list")),
                };
                for summary in &summaries {
                    let Some(summary) = summary.as_object() else {
                        return Err(refused("a summary is not an object"));
                    };
                    if matches!(
                        summary.get("type").and_then(Value::as_str),
                        Some("summary_text" | "reasoning_summary_text")
                    ) {
                        reasoning_parts.push(text(summary, "text")?.unwrap_or_default());
                    }
                }
            }
            Some("function_call") => {
                let call_id = text(item, "call_id")?
                    .filter(|id| !id.is_empty())
                    .or(text(item, "id")?);
                calls.push(ToolCall {
                    id: call_id,
                    index: u64::try_from(index).ok(),
                    name: text(item, "name")?,
                    arguments: Some(text(item, "arguments")?.unwrap_or_default()),
                });
            }
            _ => {}
        }
    }
    let reasoning = reasoning_parts.concat();
    Ok(Chunk {
        message: Message {
            content: Some(text_parts.concat()),
            reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
            reasoning_payloads: reasoning_payloads(&output)?,
            tool_calls: (!calls.is_empty()).then_some(calls),
            ..Message::assistant()
        },
        usage: Some(usage_of(data)?),
        correlation_id: None,
        stop: None,
    })
}
