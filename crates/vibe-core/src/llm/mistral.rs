//! The Mistral backend, which the reference drives through the Mistral Python
//! client (`vibe/core/llm/backend/mistral.py` over `mistralai` 2.6).
//!
//! What reaches the wire is the client's: the request model serializes its
//! nullable fields as `null`, an assistant turn always carries `prefix` and
//! `tool_calls`, reasoning travels as a `thinking` chunk only when thinking is
//! on, and the effort collapses to `none` or `high`. The client retries 429
//! and the 5xx it knows on its own schedule and retries connection failures
//! and timeouts, but not a peer that hung up mid-answer; the backend reports
//! every retryable status and every such failure as it happens. Its event
//! reader splits on blank lines of any newline style and ends on `[DONE]`.

use serde_json::{Map, Value, json};

use super::adapter::{lax_count, truthy};
use super::error::{
    BackendErrorSource, BackendFailure, KeyOrigin, LocalFailure, LocalKind, TransportFailure,
};
use super::generic::{CallFacts, merge_headers};
use super::pump::{ChunkPump, ChunkStream, EventSource, Settle};
use super::python_json::Ordered;
use super::retry::{
    Clock, MISTRAL_RETRYABLE_STATUSES, RetryObserver, RetryReason, SleepKind, mistral_delay,
    mistral_retry_after_millis,
};
use super::transport::{HttpClient, HttpResponse, HttpSettings};
use super::types::{Chunk, Message, Role, StopInfo, Tool, ToolCall, ToolChoice, Usage};
use super::{BackendContext, ModelRequest};
use crate::provider::config::ProviderConfig;

const PATH: &str = "/v1/chat/completions";

pub struct MistralBackend {
    provider: ProviderConfig,
    server_url: String,
    client: HttpClient,
    api_key: Option<String>,
    api_key_origin: Option<KeyOrigin>,
    budget_millis: i64,
    context: BackendContext,
}

/// The server URL the client takes: the API base without its version path.
/// Reference `get_server_url_from_api_base`, whose greedy match cuts at the
/// last `/v<digits>`.
#[must_use]
pub fn server_url(api_base: &str) -> Option<String> {
    let rest = api_base
        .strip_prefix("https://")
        .or_else(|| api_base.strip_prefix("http://"))?;
    let scheme_length = api_base.len() - rest.len();
    let bytes = api_base.as_bytes();
    // The host part needs at least one character before the version path.
    (scheme_length + 1..api_base.len())
        .rev()
        .find(|&position| {
            bytes[position] == b'/'
                && bytes.get(position + 1) == Some(&b'v')
                && bytes.get(position + 2).is_some_and(u8::is_ascii_digit)
                && !api_base[scheme_length..position].contains('\n')
        })
        .map(|position| api_base[..position].to_owned())
}

impl MistralBackend {
    /// # Errors
    ///
    /// A custom reasoning field, which this client cannot honor, an API base
    /// without a version path, and an HTTP client that fails to initialize.
    pub(super) fn new(
        provider: ProviderConfig,
        context: BackendContext,
    ) -> Result<Self, LocalFailure> {
        if provider.reasoning_field_name != "reasoning_content" {
            return Err(LocalFailure::new(
                LocalKind::Configuration,
                format!(
                    "the Mistral backend reads reasoning from thinking chunks and cannot use the \
                     reasoning field `{}`",
                    provider.reasoning_field_name
                ),
            ));
        }
        let server_url = server_url(&provider.api_base).ok_or_else(|| {
            LocalFailure::new(
                LocalKind::Configuration,
                format!(
                    "the API base `{}` is not of the form <server>/v<version>",
                    provider.api_base
                ),
            )
        })?;
        let api = context.api;
        let client = HttpClient::new(HttpSettings {
            read_timeout: api.timeout,
            connect_timeout: api.timeout.min(api.connect_timeout),
            follow_redirects: true,
        })
        .map_err(|failure| LocalFailure::new(LocalKind::Configuration, failure.to_string()))?;
        let resolved = context.credentials.resolve(&provider.api_key_env_var);
        let (api_key, api_key_origin) = match resolved {
            Some((key, origin)) => (Some(key), Some(origin)),
            None => (None, None),
        };
        #[allow(clippy::cast_possible_truncation)]
        let budget_millis = (api.retry_max_elapsed_time.as_secs_f64() * 1000.0) as i64;
        Ok(Self {
            provider,
            server_url,
            client,
            api_key,
            api_key_origin,
            budget_millis,
            context,
        })
    }

    fn clock(&self) -> &dyn Clock {
        self.context.clock.as_ref()
    }

    fn facts(&self, request: &ModelRequest<'_>) -> CallFacts {
        CallFacts::new(
            &self.provider,
            self.server_url.clone(),
            request,
            self.api_key_origin.clone(),
        )
    }

    fn headers(&self, request: &ModelRequest<'_>, streaming: bool) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "user-agent".to_owned(),
                "mistral-client-python/2.6.0".to_owned(),
            ),
            (
                "Accept".to_owned(),
                if streaming {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .to_owned(),
            ),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        if let Some(key) = &self.api_key {
            headers.push(("Authorization".to_owned(), format!("Bearer {key}")));
        }
        merge_headers(&mut headers, request.extra_headers);
        headers
    }

    /// Sends the request under the client's retry schedule, reporting each
    /// retryable status and connection failure. What comes back is the last
    /// answer, retryable or not, once the budget is spent.
    async fn send(
        &self,
        request: &ModelRequest<'_>,
        streaming: bool,
        facts: &CallFacts,
        retries: &dyn RetryObserver,
    ) -> Result<(HttpResponse, Option<Vec<u8>>), BackendFailure> {
        let body = serde_json::to_vec(&body(request, streaming))
            .map_err(|error| BackendFailure::local(LocalKind::Type, error.to_string()))?;
        let headers = self.headers(request, streaming);
        let url = format!("{}{PATH}", self.server_url);
        let started = millis(self.clock().monotonic());
        let mut attempt: u32 = 0;
        loop {
            let outcome = self.client.post(&url, &headers, body.clone()).await;
            let retry_after = match outcome {
                Ok(mut response) => {
                    if !MISTRAL_RETRYABLE_STATUSES.contains(&response.status) {
                        return Ok((response, None));
                    }
                    let read = response.read_all().await.ok();
                    retries.retrying(&RetryReason::for_status(response.status));
                    let over = millis(self.clock().monotonic()) - started > self.budget_millis;
                    if over {
                        return Ok((response, Some(read.unwrap_or_default())));
                    }
                    response
                        .header("retry-after")
                        .and_then(|value| mistral_retry_after_millis(value, self.clock().now()))
                }
                Err(failure) => {
                    let retryable = failure.kind.is_network() || failure.kind.is_timeout();
                    if !retryable {
                        return Err(facts.request_error(&failure));
                    }
                    retries.retrying(&RetryReason::for_failure(&facts.request_error(&failure)));
                    if millis(self.clock().monotonic()) - started > self.budget_millis {
                        return Err(facts.request_error(&failure));
                    }
                    None
                }
            };
            let delay = mistral_delay(retry_after, attempt, self.clock().jitter());
            self.clock().sleep(delay, SleepKind::Retry).await;
            attempt += 1;
        }
    }

    /// Reads a refused answer the way the client raises it.
    async fn refusal(
        facts: &CallFacts,
        mut response: HttpResponse,
        read: Option<Vec<u8>>,
    ) -> BackendFailure {
        let body = match read {
            Some(body) => body,
            None => match response.read_all().await {
                Ok(body) => body,
                Err(failure) => return facts.request_error(&failure),
            },
        };
        let content_type = response
            .header("content-type")
            .unwrap_or("application/octet-stream");
        if response.status == 422 && content_type_matches(content_type, "application/json") {
            return validation_report(&body);
        }
        facts.status_error(BackendErrorSource::Client, &response, &body)
    }

    /// One non-streaming call.
    ///
    /// # Errors
    ///
    /// The client refusing the answer, and a request that got none.
    pub(super) async fn complete(
        &self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<Chunk, BackendFailure> {
        let facts = self.facts(request);
        let (mut response, read) = self.send(request, false, &facts, retries).await?;
        let json_answer = response.status == 200
            && content_type_matches(
                response
                    .header("content-type")
                    .unwrap_or("application/octet-stream"),
                "application/json",
            );
        if !json_answer {
            return Err(Self::refusal(&facts, response, read).await);
        }
        let body = match read {
            Some(body) => body,
            None => response
                .read_all()
                .await
                .map_err(|failure| facts.request_error(&failure))?,
        };
        let text = String::from_utf8_lossy(&body).into_owned();
        whole_answer(&text)
            .map_err(|failure| BackendFailure::local(LocalKind::MistralResponse, failure.message))
    }

    /// One streaming call.
    ///
    /// # Errors
    ///
    /// As [`MistralBackend::complete`], for everything before the stream
    /// opens; later failures arrive through the stream.
    pub(super) async fn complete_streaming<'a>(
        &'a self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<ChunkStream<'a>, BackendFailure> {
        let facts = self.facts(request);
        let (response, read) = self.send(request, true, &facts, retries).await?;
        let event_stream = response.status == 200
            && content_type_matches(
                response
                    .header("content-type")
                    .unwrap_or("application/octet-stream"),
                "text/event-stream",
            );
        if !event_stream {
            return Err(Self::refusal(&facts, response, read).await);
        }
        let correlation_id = response.header("mistral-correlation-id").map(str::to_owned);
        let transport_facts = facts.clone();
        let pump = ChunkPump::new(
            response.body,
            EventReader {
                buffer: Vec::new(),
                started: false,
                done: false,
                correlation_id,
            },
            Box::new(move |failure: TransportFailure| transport_facts.request_error(&failure)),
        );
        Ok(pump.into_stream(Settle::none()))
    }
}

fn millis(seconds: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let millis = (seconds * 1000.0).round_ties_even() as i64;
    millis
}

/// Reference `match_content_type`: the exact value, a wildcard, or the media
/// type with its parameters stripped.
fn content_type_matches(content_type: &str, pattern: &str) -> bool {
    if pattern == content_type || pattern == "*" || pattern == "*/*" {
        return true;
    }
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let media = if media.matches('/').count() == 1 {
        media
    } else {
        "text/plain".to_owned()
    };
    if media == pattern {
        return true;
    }
    media.split_once('/').is_some_and(|(major, minor)| {
        pattern == format!("{major}/*") || pattern == format!("*/{minor}")
    })
}

/// A 422 the client reads as a validation report, or fails to read as one.
fn validation_report(body: &[u8]) -> BackendFailure {
    let well_formed = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|report| match report.get("detail") {
            None | Some(Value::Null) => true,
            Some(Value::Array(items)) => items.iter().all(|item| {
                item.as_object().is_some_and(|item| {
                    item.get("msg").is_some_and(Value::is_string)
                        && item.get("type").is_some_and(Value::is_string)
                        && item
                            .get("loc")
                            .and_then(Value::as_array)
                            .is_some_and(|loc| {
                                loc.iter()
                                    .all(|part| part.is_string() || part.is_i64() || part.is_u64())
                            })
                })
            }),
            Some(_) => false,
        });
    if well_formed {
        BackendFailure::local(
            LocalKind::MistralValidation,
            "the provider refused the request as invalid",
        )
    } else {
        BackendFailure::local(
            LocalKind::MistralResponse,
            "the provider's validation report could not be read",
        )
    }
}

/// The reasoning effort the Mistral API takes for a thinking level.
fn effort(thinking: &str) -> Option<&'static str> {
    match thinking {
        "low" => Some("none"),
        "medium" | "high" | "max" => Some("high"),
        _ => None,
    }
}

/// The request body as the client's request model serializes it.
fn body(request: &ModelRequest<'_>, streaming: bool) -> Value {
    let include_reasoning = request.model.thinking != "off";
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.model.name));
    body.insert("temperature".to_owned(), json!(request.temperature));
    body.insert("max_tokens".to_owned(), json!(request.max_tokens));
    body.insert("stream".to_owned(), Value::Bool(streaming));
    body.insert(
        "messages".to_owned(),
        Value::Array(
            request
                .messages
                .iter()
                .map(|message| mistral_message(message, include_reasoning))
                .collect(),
        ),
    );
    body.insert(
        "tools".to_owned(),
        request
            .tools
            .filter(|tools| !tools.is_empty())
            .map_or(Value::Null, |tools| {
                Value::Array(tools.iter().map(Tool::chat_declaration).collect())
            }),
    );
    if let Some(choice) = request.tool_choice {
        body.insert(
            "tool_choice".to_owned(),
            match choice {
                ToolChoice::Tool(tool) => {
                    json!({"type": "function", "function": {"name": tool.name}})
                }
                keyword => json!(keyword.keyword()),
            },
        );
    }
    body.insert(
        "metadata".to_owned(),
        request
            .metadata
            .map_or(Value::Null, |metadata| Value::Object(metadata.clone())),
    );
    body.insert(
        "reasoning_effort".to_owned(),
        json!(effort(&request.model.thinking)),
    );
    Value::Object(body)
}

fn mistral_message(message: &Message, include_reasoning: bool) -> Value {
    match message.role {
        Role::System => json!({"role": "system", "content": message.text()}),
        Role::User if !message.images.is_empty() => {
            let mut parts = Vec::new();
            if !message.text().is_empty() {
                parts.push(json!({"type": "text", "text": message.text()}));
            }
            parts.extend(message.images.iter().map(
                |picture| json!({"type": "image_url", "image_url": {"url": picture.data_uri()}}),
            ));
            json!({"role": "user", "content": parts})
        }
        Role::User => json!({"role": "user", "content": message.content}),
        Role::Assistant => {
            let reasoning = message
                .reasoning_content
                .as_deref()
                .filter(|reasoning| include_reasoning && !reasoning.is_empty());
            let content = match reasoning {
                Some(reasoning) => {
                    let mut chunks = vec![json!({
                        "type": "thinking",
                        "thinking": [{"type": "text", "text": reasoning}],
                    })];
                    if !message.text().is_empty() {
                        chunks.push(json!({"type": "text", "text": message.text()}));
                    }
                    Value::Array(chunks)
                }
                None => json!(message.text()),
            };
            let calls: Vec<Value> = message
                .tool_calls
                .iter()
                .flatten()
                .map(|call| {
                    let mut converted = Map::new();
                    converted.insert(
                        "function".to_owned(),
                        json!({
                            "name": call.name.as_deref().unwrap_or_default(),
                            "arguments": call.arguments.as_deref().unwrap_or_default(),
                        }),
                    );
                    if let Some(id) = &call.id {
                        converted.insert("id".to_owned(), json!(id));
                    }
                    converted.insert("type".to_owned(), json!("function"));
                    if let Some(index) = call.index {
                        converted.insert("index".to_owned(), json!(index));
                    }
                    Value::Object(converted)
                })
                .collect();
            json!({
                "role": "assistant",
                "content": content,
                "tool_calls": calls,
                "prefix": false,
            })
        }
        Role::Tool => json!({
            "role": "tool",
            "content": message.content,
            "tool_call_id": message.tool_call_id,
            "name": message.name,
        }),
    }
}

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

/// The tool calls of a delta or a message; `ordered` is the same value with
/// its key order, which arguments given as an object are written back in.
fn parse_tool_calls(
    calls: &[Value],
    ordered: Option<&Ordered>,
) -> Result<Vec<ToolCall>, LocalFailure> {
    let refused = |what: &str| LocalFailure::new(LocalKind::Validation, what.to_owned());
    calls
        .iter()
        .enumerate()
        .map(|(position, call)| {
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
                Some(Value::Object(arguments)) => ordered
                    .and_then(|tree| tree.at(position))
                    .and_then(|call| call.get("function"))
                    .and_then(|function| function.get("arguments"))
                    .map_or_else(
                        || Ordered::from(&Value::Object(arguments.clone())).dumps(false),
                        |arguments| arguments.dumps(false),
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
    let ordered = Ordered::parse(text);
    let tool_calls = match delta.get("tool_calls") {
        Some(Value::Array(calls)) if !calls.is_empty() => {
            let tree = ordered
                .as_ref()
                .and_then(|tree| tree.get("choices"))
                .and_then(|choices| choices.at(0))
                .and_then(|choice| choice.get("delta"))
                .and_then(|delta| delta.get("tool_calls"));
            Some(parse_tool_calls(calls, tree)?)
        }
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
fn whole_answer(text: &str) -> Result<Chunk, LocalFailure> {
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
    let ordered = Ordered::parse(text);
    let tool_calls = match message.and_then(|message| message.get("tool_calls")) {
        Some(Value::Array(calls)) if !calls.is_empty() => {
            let tree = ordered
                .as_ref()
                .and_then(|tree| tree.get("choices"))
                .and_then(|choices| choices.at(0))
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("tool_calls"));
            Some(parse_tool_calls(calls, tree)?)
        }
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
struct EventReader {
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
