use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

use crate::bootstrap::BootstrapRuntime;
use crate::events::{ModelMessage, ModelToolCall};

pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WireResponse, TransportError>> + Send + 'a>>;
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;
pub type StreamTransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WireStreamResponse, TransportError>> + Send + 'a>>;

pub trait ProviderTransport: Send + Sync {
    fn send(&self, request: WireRequest) -> TransportFuture<'_>;

    fn send_stream(&self, request: WireRequest) -> StreamTransportFuture<'_> {
        Box::pin(async move {
            let response = self.send(request).await?;
            Ok(WireStreamResponse {
                status: response.status,
                headers: response.headers,
                chunks: Box::pin(futures_util::stream::iter(
                    response.chunks.into_iter().map(Ok),
                )),
            })
        })
    }
}

pub struct WireRequest {
    pub endpoint: String,
    pub headers: BTreeMap<String, SecretString>,
    pub body: Value,
    pub max_response_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub chunks: Vec<Vec<u8>>,
}

pub struct WireStreamResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub chunks: ByteStream,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("provider connection failed: {0}")]
    Connection(String),
    #[error("provider TLS failed: {0}")]
    Tls(String),
    #[error("provider stream failed: {0}")]
    Stream(String),
    #[error("provider response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
}

#[derive(Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(classify_reqwest_error)?;
        Ok(Self { client })
    }
}

impl ProviderTransport for HttpTransport {
    fn send(&self, request: WireRequest) -> TransportFuture<'_> {
        Box::pin(async move {
            let mut response = self.send_stream(request).await?;
            let mut chunks = Vec::new();
            while let Some(chunk) = response.chunks.next().await {
                chunks.push(chunk?);
            }
            Ok(WireResponse {
                status: response.status,
                headers: response.headers,
                chunks,
            })
        })
    }

    fn send_stream(&self, request: WireRequest) -> StreamTransportFuture<'_> {
        Box::pin(async move {
            let mut builder = self.client.post(&request.endpoint).json(&request.body);
            for (name, value) in request.headers {
                builder = builder.header(name, value.expose_secret());
            }
            let response = builder.send().await.map_err(classify_reqwest_error)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect();
            let seen = Arc::new(AtomicUsize::new(0));
            let limit = request.max_response_bytes;
            let chunks = response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(classify_reqwest_error)?;
                let total = seen
                    .fetch_add(chunk.len(), Ordering::Relaxed)
                    .saturating_add(chunk.len());
                if total > limit {
                    return Err(TransportError::ResponseTooLarge { limit });
                }
                Ok(chunk.to_vec())
            });
            Ok(WireStreamResponse {
                status,
                headers,
                chunks: Box::pin(chunks),
            })
        })
    }
}

fn classify_reqwest_error(error: reqwest::Error) -> TransportError {
    let is_request = error.is_builder() || error.is_request();
    let is_connect = error.is_connect();
    let message = error.without_url().to_string();
    if is_request {
        TransportError::Connection(message)
    } else if is_connect {
        if message.to_ascii_lowercase().contains("tls")
            || message.to_ascii_lowercase().contains("certificate")
        {
            TransportError::Tls(message)
        } else {
            TransportError::Connection(message)
        }
    } else {
        TransportError::Stream(message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderStyle {
    Mistral,
    Openai,
    Reasoning,
    OpenaiResponses,
    Anthropic,
    VertexAnthropic,
}

impl ProviderStyle {
    pub fn parse(value: &str) -> Result<Self, ProviderError> {
        match value {
            "mistral" => Ok(Self::Mistral),
            "openai" => Ok(Self::Openai),
            "reasoning" => Ok(Self::Reasoning),
            "openai-responses" => Ok(Self::OpenaiResponses),
            "anthropic" => Ok(Self::Anthropic),
            "vertex-anthropic" => Ok(Self::VertexAnthropic),
            other => Err(ProviderError::UnknownStyle(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    #[serde(skip)]
    pub turn_id: Option<String>,
    #[serde(skip)]
    pub model_override: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default = "default_streaming")]
    pub stream: bool,
    #[serde(default)]
    pub images: Vec<ImageInput>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: RequestLimits,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInput {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLimits {
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature_millis: Option<u16>,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            temperature_millis: None,
            max_response_bytes: default_max_response_bytes(),
        }
    }
}

const fn default_max_response_bytes() -> usize {
    2 * 1024 * 1024
}

const fn default_streaming() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_elapsed: Duration,
    pub initial_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_elapsed: Duration::from_secs(300),
            initial_delay: Duration::from_millis(250),
        }
    }
}

/// Told when a request is about to be retried, so a turn can report the wait
/// while it happens rather than after the backend gives up.
pub trait RetrySink: Send + Sync {
    fn retrying(&self, reason: &str);
}

/// The sink a call that observes nothing uses.
pub struct IgnoredRetries;

impl RetrySink for IgnoredRetries {
    fn retrying(&self, _reason: &str) {}
}

pub struct ProviderBackend<T> {
    style: ProviderStyle,
    endpoint: String,
    model: String,
    credential: SecretString,
    transport: T,
    retry: RetryPolicy,
}

impl<T> ProviderBackend<T>
where
    T: ProviderTransport,
{
    #[must_use]
    pub fn new(
        style: ProviderStyle,
        endpoint: impl Into<String>,
        model: impl Into<String>,
        credential: SecretString,
        transport: T,
    ) -> Self {
        Self {
            style,
            endpoint: endpoint.into(),
            model: model.into(),
            credential,
            transport,
            retry: RetryPolicy::default(),
        }
    }

    #[must_use]
    pub fn from_bootstrap(
        style: ProviderStyle,
        endpoint: impl Into<String>,
        runtime: &BootstrapRuntime,
        transport: T,
    ) -> Self {
        Self::new(
            style,
            endpoint,
            runtime.snapshot().model.clone(),
            runtime.credential.clone(),
            transport,
        )
    }

    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub async fn complete(&self, input: &ProviderInput) -> Result<AssistantMessage, ProviderError> {
        let mut stream = self.stream(input).await?;
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.chunks.next().await {
            chunks.push(chunk?);
        }
        aggregate_provider_chunks(chunks, stream.correlation_id)
    }

    pub async fn stream(&self, input: &ProviderInput) -> Result<ProviderStream, ProviderError> {
        self.stream_observed(input, &IgnoredRetries).await
    }

    /// Streams one completion, telling `retries` about every attempt it makes
    /// after the first.
    pub async fn stream_observed(
        &self,
        input: &ProviderInput,
        retries: &dyn RetrySink,
    ) -> Result<ProviderStream, ProviderError> {
        let started = Instant::now();
        let mut delay = self.retry.initial_delay;
        loop {
            let remaining = self
                .retry
                .max_elapsed
                .checked_sub(started.elapsed())
                .ok_or(ProviderError::ElapsedTimeout)?;
            let response = tokio::time::timeout(
                remaining,
                self.transport.send_stream(self.build_request(input)?),
            )
            .await
            .map_err(|_| ProviderError::ElapsedTimeout)?;
            let response = match response {
                Ok(response) => response,
                Err(error @ TransportError::Connection(_)) => {
                    if started.elapsed().saturating_add(delay) > self.retry.max_elapsed {
                        return Err(ProviderError::Transport(error));
                    }
                    retries.retrying(&format!("connection failed: {error}"));
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                Err(error) => return Err(ProviderError::Transport(error)),
            };
            if is_retryable(response.status) {
                if started.elapsed().saturating_add(delay) > self.retry.max_elapsed {
                    return Err(ProviderError::RetryExhausted {
                        status: response.status,
                    });
                }
                retries.retrying(&format!("provider answered HTTP {}", response.status));
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
            return self.decode_stream_response(response);
        }
    }

    pub fn build_request(&self, input: &ProviderInput) -> Result<WireRequest, ProviderError> {
        validate_provider_endpoint(&self.endpoint)?;
        if input.messages.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "at least one message is required".to_owned(),
            ));
        }
        let model = input.model_override.as_deref().unwrap_or(&self.model);
        let body = match self.style {
            ProviderStyle::Mistral | ProviderStyle::Openai | ProviderStyle::Reasoning => {
                build_chat_request(self.style, model, input)?
            }
            ProviderStyle::OpenaiResponses => build_responses_request(model, input)?,
            ProviderStyle::Anthropic | ProviderStyle::VertexAnthropic => {
                build_anthropic_request(self.style, model, input)?
            }
        };
        let mut headers = BTreeMap::new();
        headers.insert(
            "content-type".to_owned(),
            SecretString::from("application/json".to_owned()),
        );
        match self.style {
            ProviderStyle::Anthropic => {
                headers.insert("x-api-key".to_owned(), self.credential.clone());
                headers.insert(
                    "anthropic-version".to_owned(),
                    SecretString::from("2023-06-01".to_owned()),
                );
            }
            _ => {
                headers.insert(
                    "authorization".to_owned(),
                    SecretString::from(format!("Bearer {}", self.credential.expose_secret())),
                );
            }
        }
        for (name, value) in &input.headers {
            if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
            {
                return Err(ProviderError::InvalidRequest(format!(
                    "reserved authentication header `{name}`"
                )));
            }
            headers.insert(name.to_ascii_lowercase(), SecretString::from(value.clone()));
        }
        Ok(WireRequest {
            endpoint: self.endpoint.clone(),
            headers,
            body,
            max_response_bytes: input.limits.max_response_bytes,
        })
    }

    fn decode_stream_response(
        &self,
        response: WireStreamResponse,
    ) -> Result<ProviderStream, ProviderError> {
        if response.status == 401 || response.status == 403 {
            return Err(ProviderError::Authentication {
                status: response.status,
            });
        }
        if response.status == 413 {
            return Err(ProviderError::ContextOverflow);
        }
        if !(200..300).contains(&response.status) {
            return Err(ProviderError::HttpStatus {
                status: response.status,
            });
        }
        let correlation_id = response
            .headers
            .get("mistral-correlation-id")
            .or_else(|| response.headers.get("x-request-id"))
            .or_else(|| response.headers.get("request-id"))
            .cloned();
        Ok(ProviderStream {
            correlation_id,
            chunks: decode_chunk_stream(self.style, response.chunks),
        })
    }
}

pub type ProviderChunkStream =
    Pin<Box<dyn Stream<Item = Result<ProviderChunk, ProviderError>> + Send>>;

pub struct ProviderStream {
    pub correlation_id: Option<String>,
    pub chunks: ProviderChunkStream,
}

struct DecodeChunkState {
    style: ProviderStyle,
    input: ByteStream,
    buffer: Vec<u8>,
    parsed: StreamParseState,
    queued: VecDeque<ProviderChunk>,
    seen_value: bool,
    finished: bool,
}

fn decode_chunk_stream(style: ProviderStyle, input: ByteStream) -> ProviderChunkStream {
    let state = DecodeChunkState {
        style,
        input,
        buffer: Vec::new(),
        parsed: StreamParseState::default(),
        queued: VecDeque::new(),
        seen_value: false,
        finished: false,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            loop {
                if let Some(chunk) = state.queued.pop_front() {
                    return Some((Ok(chunk), state));
                }
                if state.finished {
                    return None;
                }
                if let Some(newline) = state.buffer.iter().position(|byte| *byte == b'\n') {
                    let line = state.buffer.drain(..=newline).collect::<Vec<_>>();
                    match parse_stream_line(&line, state.style, &mut state.parsed) {
                        Ok(Some(chunks)) => {
                            state.seen_value = true;
                            state.queued.extend(chunks);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            state.finished = true;
                            return Some((Err(error), state));
                        }
                    }
                    continue;
                }
                match state.input.next().await {
                    Some(Ok(bytes)) => state.buffer.extend(bytes),
                    Some(Err(error)) => {
                        state.finished = true;
                        return Some((Err(ProviderError::Transport(error)), state));
                    }
                    None => {
                        state.finished = true;
                        if !state.buffer.is_empty() {
                            match parse_stream_line(&state.buffer, state.style, &mut state.parsed) {
                                Ok(Some(chunks)) => {
                                    state.seen_value = true;
                                    state.queued.extend(chunks);
                                    state.buffer.clear();
                                    state.finished = false;
                                    continue;
                                }
                                Ok(None) => {}
                                Err(error) => return Some((Err(error), state)),
                            }
                        }
                        if !state.seen_value {
                            return Some((
                                Err(ProviderError::MalformedStream(
                                    "no JSON events in provider response".to_owned(),
                                )),
                                state,
                            ));
                        }
                        return None;
                    }
                }
            }
        },
    ))
}

fn parse_stream_line(
    bytes: &[u8],
    style: ProviderStyle,
    state: &mut StreamParseState,
) -> Result<Option<Vec<ProviderChunk>>, ProviderError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::MalformedStream(error.to_string()))?
        .trim();
    if text.is_empty() || text.starts_with(':') || text == "data: [DONE]" {
        return Ok(None);
    }
    let payload = text.strip_prefix("data: ").unwrap_or(text);
    let value = serde_json::from_str::<Value>(payload)
        .map_err(|error| ProviderError::MalformedStream(error.to_string()))?;
    let chunks = match style {
        ProviderStyle::Mistral | ProviderStyle::Openai | ProviderStyle::Reasoning => {
            parse_chat_event(&value, state)?
        }
        ProviderStyle::OpenaiResponses => parse_responses_event(&value, state)?,
        ProviderStyle::Anthropic | ProviderStyle::VertexAnthropic => {
            parse_anthropic_event(&value, state)?
        }
    };
    Ok(Some(chunks))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderChunk {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Refusal {
        message: String,
    },
    Stop {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub text: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_signature: Option<String>,
    #[serde(default)]
    pub reasoning_state: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: Usage,
    #[serde(default)]
    pub refusal: Option<String>,
    pub stop_reason: String,
    #[serde(default)]
    pub correlation_id: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProviderError {
    #[error("unknown provider style `{0}`")]
    UnknownStyle(String),
    #[error("invalid provider request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Transport(TransportError),
    #[error("provider authentication failed with HTTP {status}")]
    Authentication { status: u16 },
    #[error("provider context window is full")]
    ContextOverflow,
    #[error("provider returned HTTP {status}")]
    HttpStatus { status: u16 },
    #[error("provider retry budget exhausted after HTTP {status}")]
    RetryExhausted { status: u16 },
    #[error("provider request exceeded its elapsed-time limit")]
    ElapsedTimeout,
    #[error("malformed provider stream: {0}")]
    MalformedStream(String),
    #[error("provider returned unsupported content block `{0}`")]
    UnsupportedContentBlock(String),
    #[error("provider response omitted final usage")]
    MissingUsage,
    #[error("provider response was refused: {0}")]
    Refusal(String),
}

fn build_chat_request(
    style: ProviderStyle,
    model: &str,
    input: &ProviderInput,
) -> Result<Value, ProviderError> {
    let mut messages = input
        .messages
        .iter()
        .map(|message| {
            if style == ProviderStyle::Reasoning {
                reasoning_message(message)
            } else {
                chat_message(message)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    attach_images(&mut messages, &input.images)?;
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": input.stream,
        "max_tokens": input.limits.max_tokens,
    });
    if input.stream {
        body["stream_options"] =
            if matches!(style, ProviderStyle::Mistral | ProviderStyle::Reasoning) {
                json!({"include_usage": true, "stream_tool_calls": true})
            } else {
                json!({"include_usage": true})
            };
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(choice) = &input.tool_choice {
        body["tool_choice"] = chat_tool_choice(choice);
    }
    if let Some(temperature) = input.limits.temperature_millis {
        body["temperature"] = json!(f64::from(temperature) / 1000.0);
    }
    if style == ProviderStyle::Mistral && input.thinking {
        body["temperature"] = json!(1.0);
    }
    if style == ProviderStyle::Reasoning
        && let Some(effort) = input
            .reasoning_effort
            .as_deref()
            .filter(|effort| !matches!(*effort, "off" | "none"))
            .or(input.thinking.then_some("medium"))
    {
        body["reasoning_effort"] = json!(effort);
    }
    if !input.metadata.is_empty() {
        body["metadata"] = serde_json::to_value(&input.metadata)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
    }
    Ok(body)
}

fn reasoning_message(message: &ModelMessage) -> Result<Value, ProviderError> {
    match message {
        ModelMessage::Assistant {
            content,
            reasoning: Some(reasoning),
            tool_calls,
            ..
        } => {
            let mut value = json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": [{"type": "text", "text": reasoning}],
                    },
                    {"type": "text", "text": content},
                ],
            });
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            Ok(value)
        }
        _ => chat_message(message),
    }
}

fn chat_message(message: &ModelMessage) -> Result<Value, ProviderError> {
    match message {
        ModelMessage::System { content } => Ok(json!({"role": "system", "content": content})),
        ModelMessage::User { content } => Ok(json!({"role": "user", "content": content})),
        ModelMessage::Assistant {
            content,
            reasoning,
            reasoning_signature,
            reasoning_state: _,
            tool_calls,
        } => {
            let mut value = json!({"role": "assistant", "content": content});
            if let Some(reasoning) = reasoning {
                value["reasoning_content"] = json!(reasoning);
            }
            if let Some(signature) = reasoning_signature {
                value["reasoning_signature"] = json!(signature);
            }
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            Ok(value)
        }
        ModelMessage::Tool {
            call_id,
            content,
            is_error,
        } => Ok(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
            "is_error": is_error,
        })),
    }
}

fn attach_images(messages: &mut [Value], images: &[ImageInput]) -> Result<(), ProviderError> {
    if images.is_empty() {
        return Ok(());
    }
    let message = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .ok_or_else(|| ProviderError::InvalidRequest("images require a user message".to_owned()))?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(images.iter().map(|image| {
        json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", image.media_type, image.data)
            }
        })
    }));
    message["content"] = Value::Array(content);
    Ok(())
}

fn build_responses_request(model: &str, input: &ProviderInput) -> Result<Value, ProviderError> {
    let mut messages = Vec::new();
    for message in &input.messages {
        match message {
            ModelMessage::System { content } => {
                messages.push(json!({"role": "system", "content": content}));
            }
            ModelMessage::User { content } => {
                messages.push(json!({"role": "user", "content": content}));
            }
            ModelMessage::Assistant {
                content,
                reasoning_state,
                tool_calls,
                ..
            } => {
                messages.extend(
                    reasoning_state
                        .iter()
                        .map(|state| json!({"type": "reasoning", "encrypted_content": state})),
                );
                messages.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": content}],
                }));
                messages.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    })
                }));
            }
            ModelMessage::Tool {
                call_id, content, ..
            } => messages.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": content,
            })),
        }
    }
    if let Some(last_user) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    {
        if !input.images.is_empty() {
            let text = last_user
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type": "input_text", "text": text}));
            }
            content.extend(input.images.iter().map(|image| {
                json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", image.media_type, image.data),
                })
            }));
            last_user["content"] = Value::Array(content);
        }
    } else if !input.images.is_empty() {
        return Err(ProviderError::InvalidRequest(
            "images require a user message".to_owned(),
        ));
    }
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let effort = match input.reasoning_effort.as_deref() {
        Some("max") => "xhigh",
        Some("off") | Some("none") => "none",
        Some(effort) => effort,
        None if input.thinking => "medium",
        None => "none",
    };
    let mut body = json!({
        "model": model,
        "input": messages,
        "store": false,
        "max_output_tokens": input.limits.max_tokens,
        "reasoning": {"effort": effort},
    });
    if input.stream {
        body["stream"] = json!(true);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(choice) = &input.tool_choice {
        body["tool_choice"] = responses_tool_choice(choice);
    }
    if (model.starts_with("gpt-4") || model.starts_with("gpt-3.5"))
        && let Some(temperature) = input.limits.temperature_millis
    {
        body["temperature"] = json!(f64::from(temperature) / 1000.0);
    }
    Ok(body)
}

fn build_anthropic_request(
    style: ProviderStyle,
    model: &str,
    input: &ProviderInput,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &input.messages {
        match message {
            ModelMessage::System { content } => system.push(content.clone()),
            ModelMessage::User { content } => messages.push(json!({
                "role": "user",
                "content": [{"type": "text", "text": content}],
            })),
            ModelMessage::Assistant {
                content,
                reasoning,
                reasoning_signature,
                reasoning_state: _,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if let (Some(reasoning), Some(signature)) = (reasoning, reasoning_signature) {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": reasoning,
                        "signature": signature,
                    }));
                }
                if !content.is_empty() {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                blocks.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "tool_use",
                        "id": sanitize_tool_id(&call.id),
                        "name": call.name,
                        "input": serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({})),
                    })
                }));
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            ModelMessage::Tool {
                call_id,
                content,
                is_error: _,
            } => messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": sanitize_tool_id(call_id),
                    "content": content,
                }],
            })),
        }
    }
    if let Some(content) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    {
        content.extend(input.images.iter().map(|image| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.media_type,
                    "data": image.data,
                }
            })
        }));
    } else if !input.images.is_empty() {
        return Err(ProviderError::InvalidRequest(
            "images require a user message".to_owned(),
        ));
    }
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "messages": messages,
        "max_tokens": input.limits.max_tokens,
    });
    if style == ProviderStyle::Anthropic {
        body["model"] = json!(model);
    } else {
        body["anthropic_version"] = json!("vertex-2023-10-16");
    }
    if input.stream {
        body["stream"] = json!(true);
    }
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(choice) = &input.tool_choice {
        body["tool_choice"] = anthropic_tool_choice(choice);
    }
    if input.thinking {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": u64::from(input.limits.max_tokens).min(2048),
        });
    }
    Ok(body)
}

pub(crate) fn aggregate_provider_chunks(
    chunks: Vec<ProviderChunk>,
    correlation_id: Option<String>,
) -> Result<AssistantMessage, ProviderError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_signature = None;
    let mut reasoning_state = Vec::new();
    let mut tool_calls = Vec::<ModelToolCall>::new();
    let mut usage: Option<Usage> = None;
    let mut refusal = None;
    let mut stop_reason = None;
    for chunk in chunks {
        match chunk {
            ProviderChunk::Text { text: delta } => text.push_str(&delta),
            ProviderChunk::Reasoning {
                text: delta,
                signature,
            } => {
                reasoning.push_str(&delta);
                if signature.is_some() {
                    reasoning_signature.clone_from(&signature);
                    if let Some(signature) = signature {
                        reasoning_state.push(signature);
                    }
                }
            }
            ProviderChunk::ToolCall {
                id,
                name,
                arguments,
            } => {
                if let Some(existing) = tool_calls.iter_mut().find(|call| call.id == id) {
                    if !name.is_empty() {
                        existing.name = name;
                    }
                    existing.arguments.push_str(&arguments);
                } else {
                    tool_calls.push(ModelToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            ProviderChunk::Usage {
                input_tokens,
                output_tokens,
            } => match &mut usage {
                Some(usage) => {
                    if input_tokens > 0 {
                        usage.input_tokens = input_tokens;
                    }
                    if output_tokens > 0 {
                        usage.output_tokens = output_tokens;
                    }
                }
                None => {
                    usage = Some(Usage {
                        input_tokens,
                        output_tokens,
                    });
                }
            },
            ProviderChunk::Refusal { message } => refusal = Some(message),
            ProviderChunk::Stop { reason } => stop_reason = Some(reason),
        }
    }
    let usage = usage.ok_or(ProviderError::MissingUsage)?;
    if let Some(message) = &refusal {
        return Err(ProviderError::Refusal(message.clone()));
    }
    let stop_reason = stop_reason.ok_or_else(|| {
        ProviderError::MalformedStream("provider response omitted stop state".to_owned())
    })?;
    Ok(AssistantMessage {
        text,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        reasoning_signature,
        reasoning_state,
        tool_calls,
        usage,
        refusal,
        stop_reason,
        correlation_id,
    })
}

#[derive(Debug, Default)]
struct StreamParseState {
    tool_ids_by_index: BTreeMap<u64, String>,
    response_call_ids: BTreeMap<String, String>,
}

fn parse_chat_event(
    value: &Value,
    state: &mut StreamParseState,
) -> Result<Vec<ProviderChunk>, ProviderError> {
    let mut chunks = Vec::new();
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        chunks.push(ProviderChunk::Usage {
            input_tokens: u64_field(usage, "prompt_tokens")?,
            output_tokens: u64_field(usage, "completion_tokens")?,
        });
    }
    let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(chunks);
    };
    let delta = choice
        .get("delta")
        .or_else(|| choice.get("message"))
        .unwrap_or(choice);
    if let Some(content) = delta.get("content").filter(|content| !content.is_null()) {
        match content {
            Value::String(text) => chunks.push(ProviderChunk::Text {
                text: text.to_owned(),
            }),
            Value::Array(blocks) => {
                for block in blocks {
                    let kind = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    match kind {
                        "text" => chunks.push(ProviderChunk::Text {
                            text: string_field(block, "text")?,
                        }),
                        "thinking" => {
                            let text = block
                                .get("thinking")
                                .and_then(Value::as_array)
                                .map(|parts| {
                                    parts
                                        .iter()
                                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                                        .collect::<String>()
                                })
                                .unwrap_or_default();
                            chunks.push(ProviderChunk::Reasoning {
                                text,
                                signature: None,
                            });
                        }
                        _ => {
                            return Err(ProviderError::UnsupportedContentBlock(kind.to_owned()));
                        }
                    }
                }
            }
            _ => {
                return Err(ProviderError::UnsupportedContentBlock(
                    "invalid_chat_content".to_owned(),
                ));
            }
        }
    }
    if let Some(text) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
    {
        chunks.push(ProviderChunk::Reasoning {
            text: text.to_owned(),
            signature: delta
                .get("reasoning_signature")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    if let Some(refusal) = delta.get("refusal").and_then(Value::as_str) {
        chunks.push(ProviderChunk::Refusal {
            message: refusal.to_owned(),
        });
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                state.tool_ids_by_index.insert(index, id.to_owned());
            }
            let id = state
                .tool_ids_by_index
                .get(&index)
                .cloned()
                .unwrap_or_else(|| format!("index-{index}"));
            chunks.push(ProviderChunk::ToolCall {
                id,
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        chunks.push(ProviderChunk::Stop {
            reason: reason.to_owned(),
        });
    }
    Ok(chunks)
}

fn parse_anthropic_event(
    value: &Value,
    state: &mut StreamParseState,
) -> Result<Vec<ProviderChunk>, ProviderError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut chunks = Vec::new();
    match kind {
        "message_start" => {
            if let Some(usage) = value.pointer("/message/usage") {
                chunks.push(ProviderChunk::Usage {
                    input_tokens: u64_field(usage, "input_tokens")?,
                    output_tokens: u64_field(usage, "output_tokens").unwrap_or(0),
                });
            }
        }
        "content_block_start" => {
            match value.pointer("/content_block/type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let id = value
                        .pointer("/content_block/id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state.tool_ids_by_index.insert(index, id.clone());
                    chunks.push(ProviderChunk::ToolCall {
                        id,
                        name: value
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        arguments: String::new(),
                    });
                }
                Some("text" | "thinking") => {}
                Some(other) => {
                    return Err(ProviderError::UnsupportedContentBlock(other.to_owned()));
                }
                None => {
                    return Err(ProviderError::MalformedStream(
                        "content block omitted its type".to_owned(),
                    ));
                }
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => chunks.push(ProviderChunk::Text {
                    text: delta
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                Some("thinking_delta") => chunks.push(ProviderChunk::Reasoning {
                    text: delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    signature: None,
                }),
                Some("signature_delta") => chunks.push(ProviderChunk::Reasoning {
                    text: String::new(),
                    signature: delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }),
                Some("input_json_delta") => chunks.push(ProviderChunk::ToolCall {
                    id: value
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|index| state.tool_ids_by_index.get(&index).cloned())
                        .ok_or_else(|| {
                            ProviderError::MalformedStream(
                                "tool delta preceded tool start".to_owned(),
                            )
                        })?,
                    name: String::new(),
                    arguments: delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                Some(other) => {
                    return Err(ProviderError::UnsupportedContentBlock(other.to_owned()));
                }
                None => {
                    return Err(ProviderError::MalformedStream(
                        "content block delta omitted its type".to_owned(),
                    ));
                }
            }
        }
        "message_delta" => {
            if let Some(usage) = value.get("usage") {
                chunks.push(ProviderChunk::Usage {
                    input_tokens: u64_field(usage, "input_tokens").unwrap_or(0),
                    output_tokens: u64_field(usage, "output_tokens")?,
                });
            }
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                chunks.push(ProviderChunk::Stop {
                    reason: reason.to_owned(),
                });
            }
        }
        "error" => {
            return Err(ProviderError::MalformedStream(
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_owned(),
            ));
        }
        "content_block_stop" | "message_stop" | "ping" => {}
        "" => {
            return Err(ProviderError::MalformedStream(
                "Anthropic event omitted its type".to_owned(),
            ));
        }
        other => {
            return Err(ProviderError::MalformedStream(format!(
                "unsupported Anthropic event `{other}`"
            )));
        }
    }
    Ok(chunks)
}

fn parse_responses_event(
    value: &Value,
    state: &mut StreamParseState,
) -> Result<Vec<ProviderChunk>, ProviderError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let chunk = match kind {
        "response.output_text.delta" => Some(ProviderChunk::Text {
            text: string_field(value, "delta")?,
        }),
        "response.reasoning_summary_text.delta" => Some(ProviderChunk::Reasoning {
            text: string_field(value, "delta")?,
            signature: None,
        }),
        "response.refusal.delta" => Some(ProviderChunk::Refusal {
            message: string_field(value, "delta")?,
        }),
        "response.output_item.added"
            if value.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
        {
            let item_id = value
                .pointer("/item/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let call_id = value
                .pointer("/item/call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            state.response_call_ids.insert(item_id, call_id.clone());
            Some(ProviderChunk::ToolCall {
                id: call_id,
                name: value
                    .pointer("/item/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: String::new(),
            })
        }
        "response.function_call_arguments.delta" => Some(ProviderChunk::ToolCall {
            id: value
                .get("item_id")
                .and_then(Value::as_str)
                .and_then(|item_id| state.response_call_ids.get(item_id))
                .cloned()
                .or_else(|| {
                    value
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .ok_or_else(|| {
                    ProviderError::MalformedStream(
                        "function arguments preceded output item".to_owned(),
                    )
                })?,
            name: String::new(),
            arguments: string_field(value, "delta")?,
        }),
        "response.completed" => {
            let response = value.get("response").unwrap_or(value);
            let usage = response.get("usage").unwrap_or(&Value::Null);
            let mut chunks = response
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                .filter_map(|item| item.get("encrypted_content").and_then(Value::as_str))
                .map(|signature| ProviderChunk::Reasoning {
                    text: String::new(),
                    signature: Some(signature.to_owned()),
                })
                .collect::<Vec<_>>();
            chunks.push(ProviderChunk::Usage {
                input_tokens: u64_field(usage, "input_tokens")?,
                output_tokens: u64_field(usage, "output_tokens")?,
            });
            chunks.push(ProviderChunk::Stop {
                reason: response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_owned(),
            });
            return Ok(chunks);
        }
        "response.failed" => {
            return Err(ProviderError::MalformedStream(
                value
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response failed")
                    .to_owned(),
            ));
        }
        _ => None,
    };
    Ok(chunk.into_iter().collect())
}

fn u64_field(value: &Value, field: &str) -> Result<u64, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| ProviderError::MalformedStream(format!("missing integer `{field}`")))
}

fn string_field(value: &Value, field: &str) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::MalformedStream(format!("missing string `{field}`")))
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn chat_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({
            "type": "function",
            "function": {"name": name},
        }),
    }
}

fn responses_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({"type": "function", "name": name}),
    }
}

fn anthropic_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::Tool { name } => json!({"type": "tool", "name": name}),
    }
}

fn validate_provider_endpoint(endpoint: &str) -> Result<(), ProviderError> {
    let parsed = Url::parse(endpoint)
        .map_err(|error| ProviderError::InvalidRequest(format!("invalid endpoint: {error}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ProviderError::InvalidRequest(
            "provider endpoint must not contain credentials".to_owned(),
        ));
    }
    if !crate::text::is_secure_transport(&parsed) {
        return Err(ProviderError::InvalidRequest(
            "provider endpoint must use HTTPS unless it is loopback".to_owned(),
        ));
    }
    Ok(())
}

#[must_use]
pub fn sanitize_tool_id(id: &str) -> String {
    let mut output = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    if output.is_empty() {
        output.push_str("tool_call");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeTransport {
        responses: Arc<Mutex<VecDeque<Result<WireResponse, TransportError>>>>,
    }

    impl FakeTransport {
        fn with_response(response: WireResponse) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(response)]))),
            }
        }
    }

    impl ProviderTransport for FakeTransport {
        fn send(&self, _request: WireRequest) -> TransportFuture<'_> {
            Box::pin(async move {
                self.responses
                    .lock()
                    .map_err(|_| TransportError::Stream("fake lock poisoned".to_owned()))?
                    .pop_front()
                    .ok_or_else(|| TransportError::Stream("missing fake response".to_owned()))?
            })
        }
    }

    fn input() -> ProviderInput {
        ProviderInput {
            turn_id: None,
            model_override: None,
            messages: vec![
                ModelMessage::System {
                    content: "system".to_owned(),
                },
                ModelMessage::User {
                    content: "hello".to_owned(),
                },
            ],
            stream: true,
            images: Vec::new(),
            tools: vec![ToolDefinition {
                name: "read".to_owned(),
                description: "read a file".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
            tool_choice: None,
            thinking: true,
            reasoning_effort: Some("high".to_owned()),
            headers: BTreeMap::new(),
            limits: RequestLimits::default(),
            metadata: BTreeMap::new(),
        }
    }

    fn backend(style: ProviderStyle, response: WireResponse) -> ProviderBackend<FakeTransport> {
        ProviderBackend::new(
            style,
            "https://provider.invalid",
            "test-model",
            SecretString::from("secret".to_owned()),
            FakeTransport::with_response(response),
        )
    }

    #[test]
    fn every_dialect_has_a_distinct_fixture_shape() {
        let response = WireResponse {
            status: 200,
            headers: BTreeMap::new(),
            chunks: vec![br#"{}"#.to_vec()],
        };
        let chat = backend(ProviderStyle::Openai, response.clone())
            .build_request(&input())
            .expect("chat request");
        assert_eq!(chat.body["messages"][1]["role"], "user");
        assert_eq!(chat.body["tools"][0]["type"], "function");

        let reasoning = backend(ProviderStyle::Reasoning, response.clone())
            .build_request(&input())
            .expect("reasoning request");
        assert_eq!(reasoning.body["reasoning_effort"], "high");
        assert_eq!(reasoning.body["stream_options"]["stream_tool_calls"], true);

        let responses = backend(ProviderStyle::OpenaiResponses, response.clone())
            .build_request(&input())
            .expect("responses request");
        assert_eq!(responses.body["input"][0]["role"], "system");
        assert_eq!(responses.body["input"][1]["role"], "user");
        assert_eq!(responses.body["store"], false);
        assert_eq!(responses.body["tools"][0]["name"], "read");

        let anthropic = backend(ProviderStyle::Anthropic, response)
            .build_request(&input())
            .expect("anthropic request");
        assert_eq!(anthropic.body["system"], "system");
        assert_eq!(anthropic.body["thinking"]["type"], "enabled");
        assert!(anthropic.headers.contains_key("x-api-key"));
        assert!(!anthropic.headers.contains_key("authorization"));
    }

    #[test]
    fn internal_turn_id_is_not_provider_metadata() {
        let mut value = input();
        value.turn_id = Some("turn-private".to_owned());
        value
            .metadata
            .insert("public-key".to_owned(), "public-value".to_owned());
        let serialized = serde_json::to_value(&value).expect("provider input serializes");
        assert!(serialized.get("turnId").is_none());
        assert_eq!(serialized["metadata"]["public-key"], "public-value");

        let request = backend(
            ProviderStyle::Mistral,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: Vec::new(),
            },
        )
        .build_request(&value)
        .expect("Mistral request");
        assert!(request.body["metadata"].get("turn_id").is_none());
        assert_eq!(request.body["metadata"]["public-key"], "public-value");
    }

    #[test]
    fn model_override_changes_routing_without_using_provider_metadata() {
        let mut value = input();
        value.model_override = Some("next-model".to_owned());
        value
            .metadata
            .insert("public-key".to_owned(), "public-value".to_owned());

        let request = backend(
            ProviderStyle::Mistral,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: Vec::new(),
            },
        )
        .build_request(&value)
        .expect("Mistral request");

        assert_eq!(request.body["model"], "next-model");
        assert_eq!(request.body["metadata"]["public-key"], "public-value");
    }

    #[tokio::test]
    async fn chat_stream_aggregates_text_reasoning_tools_usage_and_stop() {
        let response = WireResponse {
            status: 200,
            headers: BTreeMap::from([("x-request-id".to_owned(), "request-1".to_owned())]),
            chunks: vec![
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hello \",\"reasoning_content\":\"think\",\"tool_calls\":[{\"id\":\"call-1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n".to_vec(),
                b"data: {\"choices\":[{\"delta\":{\"content\":\"world\",\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":7}}\n\ndata: [DONE]\n".to_vec(),
            ],
        };
        let message = backend(ProviderStyle::Mistral, response)
            .complete(&input())
            .await
            .expect("stream aggregates");
        assert_eq!(message.text, "hello world");
        assert_eq!(message.reasoning.as_deref(), Some("think"));
        assert_eq!(message.tool_calls[0].arguments, "{\"path\":\"a\"}");
        assert_eq!(message.usage.output_tokens, 7);
        assert_eq!(message.stop_reason, "tool_calls");
        assert_eq!(message.correlation_id.as_deref(), Some("request-1"));
    }

    #[tokio::test]
    async fn malformed_auth_refusal_and_missing_usage_are_typed() {
        let auth = backend(
            ProviderStyle::Openai,
            WireResponse {
                status: 401,
                headers: BTreeMap::new(),
                chunks: Vec::new(),
            },
        )
        .complete(&input())
        .await;
        assert_eq!(auth, Err(ProviderError::Authentication { status: 401 }));

        let missing_usage = backend(
            ProviderStyle::Openai,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: vec![
                    br#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}"#
                        .to_vec(),
                ],
            },
        )
        .complete(&input())
        .await;
        assert_eq!(missing_usage, Err(ProviderError::MissingUsage));

        let malformed = backend(
            ProviderStyle::Openai,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: vec![b"data: not-json\n".to_vec()],
            },
        )
        .complete(&input())
        .await;
        assert!(matches!(malformed, Err(ProviderError::MalformedStream(_))));

        let truncated = backend(
            ProviderStyle::Openai,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: vec![
                    br#"{"choices":[{"delta":{"content":"partial"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#
                        .to_vec(),
                ],
            },
        )
        .complete(&input())
        .await;
        assert!(matches!(truncated, Err(ProviderError::MalformedStream(_))));
    }

    #[tokio::test]
    async fn anthropic_and_responses_streams_preserve_their_dialects() {
        let anthropic = backend(
            ProviderStyle::Anthropic,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: vec![
                    b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"anthropic\"}}\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n"
                        .to_vec(),
                ],
            },
        )
        .complete(&input())
        .await
        .expect("Anthropic stream aggregates");
        assert_eq!(anthropic.text, "anthropic");
        assert_eq!(anthropic.usage.input_tokens, 5);
        assert_eq!(anthropic.usage.output_tokens, 3);
        assert_eq!(anthropic.stop_reason, "end_turn");

        let responses = backend(
            ProviderStyle::OpenaiResponses,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: vec![
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"responses\"}\n\
data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"enc:streamed\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":4}}}\n"
                        .to_vec(),
                ],
            },
        )
        .complete(&input())
        .await
        .expect("Responses stream aggregates");
        assert_eq!(responses.text, "responses");
        assert_eq!(responses.usage.input_tokens, 7);
        assert_eq!(responses.usage.output_tokens, 4);
        assert_eq!(responses.stop_reason, "completed");
        assert_eq!(responses.reasoning_state, ["enc:streamed"]);
    }

    #[tokio::test]
    async fn non_streaming_chat_response_uses_the_message_payload() {
        let response = WireResponse {
            status: 200,
            headers: BTreeMap::new(),
            chunks: vec![
                br#"{"choices":[{"message":{"content":"complete answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#
                    .to_vec(),
            ],
        };
        let mut non_streaming = input();
        non_streaming.stream = false;
        let backend = backend(ProviderStyle::Mistral, response);
        let request = backend
            .build_request(&non_streaming)
            .expect("non-streaming request");
        assert_eq!(request.body["stream"], false);
        assert!(request.body.get("stream_options").is_none());
        let message = backend
            .complete(&non_streaming)
            .await
            .expect("non-streaming response aggregates");
        assert_eq!(message.text, "complete answer");
        assert_eq!(message.stop_reason, "stop");
    }

    #[tokio::test]
    async fn retryable_responses_retry_within_one_elapsed_time_budget() {
        let responses = Arc::new(Mutex::new(VecDeque::from([
            Ok(WireResponse {
                status: 503,
                headers: BTreeMap::new(),
                chunks: Vec::new(),
            }),
            Ok(WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: vec![
                    br#"{"choices":[{"delta":{"content":"retried"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#
                        .to_vec(),
                ],
            }),
        ])));
        let transport = FakeTransport {
            responses: Arc::clone(&responses),
        };
        let backend = ProviderBackend::new(
            ProviderStyle::Mistral,
            "https://provider.invalid",
            "test-model",
            SecretString::from("secret".to_owned()),
            transport,
        )
        .with_retry_policy(RetryPolicy {
            max_elapsed: Duration::from_secs(1),
            initial_delay: Duration::ZERO,
        });
        let message = backend.complete(&input()).await.expect("retry succeeds");
        assert_eq!(message.text, "retried");
        assert!(responses.lock().expect("fake response lock").is_empty());

        let exhausted = ProviderBackend::new(
            ProviderStyle::Mistral,
            "https://provider.invalid",
            "test-model",
            SecretString::from("secret".to_owned()),
            FakeTransport::default(),
        )
        .with_retry_policy(RetryPolicy {
            max_elapsed: Duration::ZERO,
            initial_delay: Duration::ZERO,
        })
        .complete(&input())
        .await;
        assert_eq!(exhausted, Err(ProviderError::ElapsedTimeout));
    }

    #[test]
    fn tool_ids_are_sanitized_without_losing_provider_signatures() {
        assert_eq!(sanitize_tool_id("call/with spaces"), "call_with_spaces");
        assert_eq!(sanitize_tool_id(""), "tool_call");
        let message = ModelMessage::Assistant {
            content: String::new(),
            reasoning: Some("reason".to_owned()),
            reasoning_signature: Some("opaque-signature".to_owned()),
            reasoning_state: vec!["opaque-signature".to_owned()],
            tool_calls: vec![ModelToolCall {
                id: "call/1".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
            }],
        };
        let reasoning_request = backend(
            ProviderStyle::Reasoning,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: Vec::new(),
            },
        )
        .build_request(&ProviderInput {
            messages: vec![message.clone()],
            ..input()
        })
        .expect("reasoning replay");
        assert_eq!(
            reasoning_request.body["messages"][0]["content"][0]["type"],
            "thinking"
        );
        assert_eq!(
            reasoning_request.body["messages"][0]["tool_calls"][0]["id"],
            "call/1"
        );
        let anthropic_request = backend(
            ProviderStyle::Anthropic,
            WireResponse {
                status: 200,
                headers: BTreeMap::new(),
                chunks: Vec::new(),
            },
        )
        .build_request(&ProviderInput {
            messages: vec![message],
            ..input()
        })
        .expect("Anthropic replay");
        assert_eq!(
            anthropic_request.body["messages"][0]["content"][1]["id"],
            "call_1"
        );
    }
}
