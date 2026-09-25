//! Replays the LLM backend corpus against this port's backends.
//!
//! `scripts/parity/llm_backends.py` drove the reference's backends and agent
//! loop call paths against a scripted HTTP stand-in on a fake clock, and
//! recorded, per call, every request that reached the wire, what the loop
//! published and appended, the usage it counted, each retry it announced and
//! each wait it asked for, and how a failure was classified. This module
//! stands up the same stand-in, serves each scenario's scripted answers in
//! order, runs the same calls through [`super::Backend`] and
//! [`super::call`], and compares every observation field by field.
//!
//! Two fields hold the reference's own sentences, which `NOTICE` forbids
//! shipping: the public message of a failure, and the provider message a
//! failure reports when the provider gave none. The corpus holds them as a
//! length plus a SHA-256, and this port's sentence must never hash to it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::call::{self, Answered, Failed, StreamingCall};
use super::error::{BackendErrorSource, CallFailure, KeySource, WrappedCause};
use super::python_json::Ordered;
use super::retry::{Clock, RetryObserver, RetryReason, SleepFuture, SleepKind, next_delay};
use super::types::{Chunk, Image, Message, Role, Tool, ToolCall, ToolChoice};
use super::utility;
use super::{Backend, BackendContext, MapCredentials, ModelRequest, TokenFuture, VertexAccess};
use crate::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::provider::config::{ApiSettings, ModelConfig, ModelRouting, ProviderConfig};

const CORPUS: &str = include_str!("../../tests/llm-backends/corpus.json");
const SCORECARD: &str = include_str!("../../../../docs/parity.md");
const CAPTURE_SCRIPT: &str = "scripts/parity/llm_backends.py";
const SESSION_ID: &str = "session-oracle";
/// Headers a transport adds on its own, which the stand-in does not record.
const TRANSPORT_HEADERS: &[&str] = &["host", "content-length", "accept-encoding", "connection"];
/// The scenarios the corpus may not fall below.
const SCENARIO_FLOOR: usize = 350;
/// How many scenarios replay side by side.
const JOBS: usize = 16;

/// A difference this port keeps on purpose, and the scorecard row that
/// answers for it.
struct Divergence {
    /// The scenario name prefix the entry covers.
    scenario: &'static str,
    /// Every difference whose JSON pointer ends with this suffix is covered.
    suffix: &'static str,
    row: &'static str,
    reason: &'static str,
}

const LEDGER: &[Divergence] = &[];

// --------------------------------------------------------------------------
// The scripted stand-in
// --------------------------------------------------------------------------

struct StandIn {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl StandIn {
    async fn start(responses: &[Value]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the stand-in binds a loopback port");
        let port = listener
            .local_addr()
            .expect("the stand-in has an address")
            .port();
        let queue = Arc::new(Mutex::new(
            responses.iter().cloned().collect::<VecDeque<_>>(),
        ));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let requests = Arc::clone(&requests);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(serve(socket, Arc::clone(&queue), Arc::clone(&requests)));
                }
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fill(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> bool {
    let mut piece = [0_u8; 16_384];
    match socket.read(&mut piece).await {
        Ok(0) | Err(_) => false,
        Ok(read) => {
            buffer.extend_from_slice(&piece[..read]);
            true
        }
    }
}

async fn serve(
    mut socket: TcpStream,
    queue: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
) {
    let mut buffer = Vec::new();
    loop {
        let head_end = loop {
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            if !fill(&mut socket, &mut buffer).await {
                return;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        buffer.drain(..head_end + 4);
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split(' ');
        let method = request_line.next().unwrap_or_default().to_owned();
        let path = request_line.next().unwrap_or_default().to_owned();
        let mut headers = Map::new();
        let mut length = 0;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            if name == "content-length" {
                length = value.parse().unwrap_or(0);
            }
            if !TRANSPORT_HEADERS.contains(&name.as_str()) {
                headers.insert(name, Value::String(value));
            }
        }
        while buffer.len() < length {
            if !fill(&mut socket, &mut buffer).await {
                return;
            }
        }
        let raw: Vec<u8> = buffer.drain(..length).collect();
        let body = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&raw)
                .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&raw)}))
        };
        let response = {
            let mut recorded = requests.lock().expect("requests lock");
            recorded
                .push(json!({"method": method, "path": path, "headers": headers, "body": body}));
            queue
                .lock()
                .expect("queue lock")
                .pop_front()
                .unwrap_or_else(|| json!({"status": 599, "json": {"error": "unscripted request"}}))
        };
        if let Some(delay) = response
            .get("delay")
            .and_then(Value::as_f64)
            .filter(|delay| *delay > 0.0)
        {
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        }
        if truthy(response.get("drop")) {
            let _ = socket.shutdown().await;
            return;
        }
        let status = response
            .get("status")
            .and_then(Value::as_u64)
            .unwrap_or(200);
        let mut extra: Vec<(String, String)> = response
            .get("headers")
            .and_then(Value::as_object)
            .map(|headers| {
                headers
                    .iter()
                    .map(|(name, value)| {
                        (name.clone(), value.as_str().unwrap_or_default().to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (payload, default_type) = if let Some(events) = response.get("sse") {
            (
                sse_body(
                    events.as_array().map_or(&[][..], Vec::as_slice),
                    truthy(response.get("crlf")),
                ),
                "text/event-stream",
            )
        } else if let Some(value) = response.get("json") {
            (
                Ordered::from(value).dumps(false).into_bytes(),
                "application/json",
            )
        } else {
            (
                response
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec(),
                "text/plain",
            )
        };
        let content_type = match extra.iter().position(|(name, _)| name == "content-type") {
            Some(index) => extra.remove(index).1,
            None => default_type.to_owned(),
        };
        let phrase = u16::try_from(status)
            .ok()
            .and_then(|status| reqwest::StatusCode::from_u16(status).ok())
            .and_then(|status| status.canonical_reason())
            .unwrap_or_default();
        let truncate = truthy(response.get("truncate"));
        let declared = payload.len() + if truncate { 64 } else { 0 };
        let mut head = format!("HTTP/1.1 {status} {phrase}\r\ncontent-type: {content_type}\r\n");
        for (name, value) in &extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("content-length: {declared}\r\n\r\n"));
        let mut message = head.into_bytes();
        message.extend_from_slice(&payload);
        if socket.write_all(&message).await.is_err() {
            return;
        }
        let _ = socket.flush().await;
        if truncate {
            let _ = socket.shutdown().await;
            return;
        }
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// Server-sent events as the oracle's stand-in writes them.
fn sse_body(events: &[Value], crlf: bool) -> Vec<u8> {
    let newline = if crlf { "\r\n" } else { "\n" };
    let mut out = String::new();
    for event in events {
        if let Some(raw) = event.get("raw") {
            out.push_str(raw.as_str().unwrap_or_default());
            continue;
        }
        if let Some(comment) = event.get("comment") {
            out.push_str(&format!(
                ": {}{newline}",
                comment.as_str().unwrap_or_default()
            ));
        }
        if let Some(name) = event.get("event") {
            out.push_str(&format!(
                "event: {}{newline}",
                name.as_str().unwrap_or_default()
            ));
        }
        if let Some(id) = event.get("id") {
            out.push_str(&format!("id: {}{newline}", id.as_str().unwrap_or_default()));
        }
        let data = &event["data"];
        let payload = match data {
            Value::String(text) => text.clone(),
            other => Ordered::from(other).dumps(false),
        };
        out.push_str(&format!("data: {payload}{newline}{newline}"));
    }
    out.into_bytes()
}

fn closed_port() -> String {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a probe port binds");
    let port = probe.local_addr().expect("the probe has an address").port();
    drop(probe);
    format!("http://127.0.0.1:{port}")
}

// --------------------------------------------------------------------------
// The fake clock, the retry recorder and the Vertex stand-in
// --------------------------------------------------------------------------

struct FakeClock {
    state: Mutex<(f64, Vec<Value>)>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            state: Mutex::new((1_000.0, Vec::new())),
        }
    }

    fn sleeps(&self) -> Vec<Value> {
        self.state.lock().expect("clock lock").1.clone()
    }
}

impl Clock for FakeClock {
    fn monotonic(&self) -> f64 {
        self.state.lock().expect("clock lock").0
    }

    /// The oracle leaves the wall clock alone, so a `Retry-After` date is
    /// measured against the real one on both sides.
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, seconds: f64, kind: SleepKind) -> SleepFuture {
        let mut state = self.state.lock().expect("clock lock");
        let kind = match kind {
            SleepKind::Retry => "retry",
            SleepKind::Pacer => "pacer",
        };
        state
            .1
            .push(json!({"kind": kind, "seconds": (seconds * 1e6).round() / 1e6}));
        state.0 += seconds.max(0.0);
        Box::pin(std::future::ready(()))
    }

    fn jitter(&self) -> f64 {
        0.5
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Value>>);

impl Recorder {
    fn reasons(&self) -> Vec<Value> {
        self.0.lock().expect("recorder lock").clone()
    }
}

impl RetryObserver for Recorder {
    fn retrying(&self, reason: &RetryReason) {
        self.0
            .lock()
            .expect("recorder lock")
            .push(json!({"category": reason.category.as_str(), "detail": reason.detail}));
    }
}

struct FakeVertex(String);

impl VertexAccess for FakeVertex {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async { Ok("vertex-token".to_owned()) })
    }

    fn base_url(&self, _region: &str) -> String {
        self.0.clone()
    }
}

// --------------------------------------------------------------------------
// Reading a scenario
// --------------------------------------------------------------------------

fn toml_table(value: &Value) -> toml::Table {
    let value = toml::Value::try_from(value).expect("a scenario entry converts to TOML");
    value
        .as_table()
        .cloned()
        .expect("a scenario entry is a table")
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn message(value: &Value) -> Message {
    let role = Role::parse(value["role"].as_str().expect("a message has a role"))
        .expect("a message role is known");
    let mut message = Message::new(role);
    message.content = text(value, "content");
    message.reasoning_content = text(value, "reasoning_content");
    message.name = text(value, "name");
    message.tool_call_id = text(value, "tool_call_id");
    message.images = value
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .map(|image| Image {
                    mime_type: text(image, "mime_type").unwrap_or_default(),
                    data: image["source"]["data"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    message.reasoning_payloads = value
        .get("reasoning_payloads")
        .and_then(Value::as_array)
        .map(|payloads| {
            payloads
                .iter()
                .filter_map(|payload| payload.as_object().cloned())
                .collect()
        });
    message.tool_calls = value
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|entry| ToolCall {
                    id: text(entry, "id"),
                    index: entry.get("index").and_then(Value::as_u64),
                    name: entry
                        .get("function")
                        .and_then(|function| text(function, "name")),
                    arguments: entry
                        .get("function")
                        .and_then(|function| text(function, "arguments")),
                })
                .collect()
        });
    message
}

fn tool(value: &Value) -> Tool {
    let function = &value["function"];
    Tool {
        name: text(function, "name").unwrap_or_default(),
        description: text(function, "description").unwrap_or_default(),
        parameters: function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({})),
    }
}

fn tool_choice(value: &Value) -> Option<ToolChoice> {
    match value {
        Value::Null => None,
        Value::String(keyword) => Some(match keyword.as_str() {
            "none" => ToolChoice::None,
            "any" => ToolChoice::Any,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        }),
        Value::Object(_) => Some(ToolChoice::Tool(Tool {
            name: value["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            description: String::new(),
            parameters: json!({}),
        })),
        _ => None,
    }
}

fn seconds(value: &Value, key: &str) -> Duration {
    Duration::from_secs_f64(value[key].as_f64().expect("an API budget is a number"))
}

// --------------------------------------------------------------------------
// Recording what a call did
// --------------------------------------------------------------------------

fn digest(text: &str) -> Value {
    json!({
        "length": text.chars().count(),
        "sha256": Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    })
}

fn message_record(message: &Message) -> Value {
    let mut record = Map::new();
    record.insert("role".to_owned(), json!(message.role.as_str()));
    if let Some(content) = message
        .content
        .as_ref()
        .filter(|content| !content.is_empty())
    {
        record.insert("content".to_owned(), json!(content));
    }
    if let Some(reasoning) = message
        .reasoning_content
        .as_ref()
        .filter(|reasoning| !reasoning.is_empty())
    {
        record.insert("reasoningContent".to_owned(), json!(reasoning));
    }
    if let Some(payloads) = message
        .reasoning_payloads
        .as_ref()
        .filter(|payloads| !payloads.is_empty())
    {
        record.insert("reasoningPayloads".to_owned(), json!(payloads));
    }
    if let Some(calls) = message
        .tool_calls
        .as_ref()
        .filter(|calls| !calls.is_empty())
    {
        let calls: Vec<Value> = calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "index": call.index,
                    "name": call.name,
                    "arguments": call.arguments,
                })
            })
            .collect();
        record.insert("toolCalls".to_owned(), Value::Array(calls));
    }
    if let Some(id) = message.tool_call_id.as_ref().filter(|id| !id.is_empty()) {
        record.insert("toolCallId".to_owned(), json!(id));
    }
    Value::Object(record)
}

fn usage_record(usage: super::types::Usage) -> Value {
    json!({
        "prompt": usage.prompt_tokens,
        "completion": usage.completion_tokens,
        "cached": usage.cached_tokens,
    })
}

fn chunk_record(chunk: &Chunk) -> Value {
    let mut record = Map::new();
    record.insert("message".to_owned(), message_record(&chunk.message));
    if let Some(usage) = chunk.usage {
        record.insert("usage".to_owned(), usage_record(usage));
    }
    if let Some(stop) = &chunk.stop {
        let mut fields = Map::new();
        for (name, value) in [
            ("reason", &stop.reason),
            ("category", &stop.category),
            ("explanation", &stop.explanation),
        ] {
            if let Some(value) = value {
                fields.insert(name.to_owned(), json!(value));
            }
        }
        record.insert("stop".to_owned(), Value::Object(fields));
    }
    if let Some(id) = chunk.correlation_id.as_ref().filter(|id| !id.is_empty()) {
        record.insert("correlationId".to_owned(), json!(id));
    }
    Value::Object(record)
}

/// The reference's exception class for a failure and for its cause.
fn classes(failure: &CallFailure) -> (&'static str, Option<&'static str>) {
    match failure {
        CallFailure::RateLimit { .. } => ("RateLimitError", Some("BackendError")),
        CallFailure::ContextTooLong { .. } => ("ContextTooLongError", Some("BackendError")),
        CallFailure::ResponseTooLong { .. } => ("ResponseTooLongError", Some("BackendError")),
        CallFailure::Refusal { .. } => ("RefusalError", None),
        CallFailure::IncompleteStream { .. } => ("IncompleteStreamError", None),
        CallFailure::InvalidModel(error) => (
            "BackendError",
            Some(match &error.source {
                BackendErrorSource::Status => "HTTPStatusError",
                BackendErrorSource::Client => "SDKError",
                BackendErrorSource::Stream => "OpenAIResponsesStreamError",
                BackendErrorSource::Request(kind) => kind.name(),
            }),
        ),
        CallFailure::Wrapped { cause, .. } => (
            "RuntimeError",
            Some(match cause {
                WrappedCause::Backend(_) => "BackendError",
                WrappedCause::Local(local) => local.kind.reference_class(),
                WrappedCause::MissingUsage { .. } => "AgentLoopLLMResponseError",
            }),
        ),
    }
}

/// A string the scenario supplied, verbatim; one this port wrote, digested.
fn scenario_text(value: Option<&str>, served: &str) -> Value {
    match value {
        None => Value::Null,
        Some(value) => {
            let escaped = serde_json::to_string(value).unwrap_or_default();
            let escaped = &escaped[1..escaped.len().saturating_sub(1)];
            if served.contains(value) || served.contains(escaped) {
                json!(value)
            } else {
                digest(value)
            }
        }
    }
}

fn error_record(failure: &CallFailure, served: &str, normalize: &[(&str, &str)]) -> Value {
    let (kind, cause) = classes(failure);
    let message = normalize
        .iter()
        .fold(failure.to_string(), |text, (needle, with)| {
            text.replace(needle, with)
        });
    let mut record = json!({
        "type": kind,
        "cause": cause,
        "code": failure.code().as_str(),
        "details": failure.details().map_or(Value::Null, Value::Object),
        "message": digest(&message),
    });
    if let Some(error) = failure.backend_error() {
        record["backend"] = json!({
            "status": error.status,
            "parsedError": scenario_text(error.parsed_error.as_deref(), served),
            "contextTooLong": error.is_context_too_long(),
            "responseTooLong": error.is_response_too_long(),
            "invalidModel": error.is_invalid_model(),
            "requestId": error.request_id(),
            "keyOrigin": error.api_key_origin.as_ref().map(|origin| json!({
                "source": match origin.source {
                    KeySource::Environment => "environment",
                    KeySource::Keyring => "keyring",
                },
                "variable": origin.variable,
            })),
        });
    }
    record
}

struct Scenario<'a> {
    backend: &'a Backend,
    provider: &'a ProviderConfig,
    model: &'a ModelConfig,
    stand_in: &'a StandIn,
    clock: &'a FakeClock,
    recorder: &'a Recorder,
    served: &'a str,
    closed: &'a str,
}

async fn run_call(scenario: &Scenario<'_>, entry: &Value) -> Value {
    let history: Vec<Message> = entry["messages"]
        .as_array()
        .expect("a call has messages")
        .iter()
        .map(message)
        .collect();
    let tools: Vec<Tool> = entry["tools"]
        .as_array()
        .map(|tools| tools.iter().map(tool).collect())
        .unwrap_or_default();
    let choice = tool_choice(&entry["toolChoice"]);
    let metadata = entry["metadata"].as_object().cloned();
    let headers = call::extra_headers(scenario.provider, SESSION_ID);
    let messages = call::messages_for_backend(history, scenario.model.supports_images);
    let request = ModelRequest {
        model: scenario.model,
        messages: &messages,
        temperature: scenario.model.temperature,
        tools: (!tools.is_empty()).then_some(tools.as_slice()),
        max_tokens: entry["maxTokens"].as_u64(),
        tool_choice: choice.as_ref(),
        extra_headers: &headers,
        metadata: metadata.as_ref(),
    };
    let before_requests = scenario.stand_in.requests().len();
    let before_sleeps = scenario.clock.sleeps().len();
    let before_retries = scenario.recorder.reasons().len();

    let mut pieces: Vec<Chunk> = Vec::new();
    let outcome: Result<Answered, Box<Failed>> = if entry["streaming"].as_bool() == Some(true) {
        let mut state = StreamingCall::new(scenario.provider, &scenario.model.name);
        match scenario
            .backend
            .complete_streaming(&request, scenario.recorder)
            .await
        {
            Err(failure) => Err(Box::new(state.fail(failure))),
            Ok(mut stream) => loop {
                match stream.next().await {
                    Some(Ok(chunk)) => match state.push(chunk) {
                        Ok(piece) => pieces.push(piece),
                        Err(failed) => break Err(failed),
                    },
                    Some(Err(failure)) => break Err(Box::new(state.fail(failure))),
                    None => break state.finish(),
                }
            },
        }
    } else {
        let result = scenario.backend.complete(&request, scenario.recorder).await;
        let outcome = call::finish_complete(result, scenario.provider, &scenario.model.name);
        if let Ok(answered) = &outcome {
            pieces.push(answered.chunk.clone());
        }
        outcome
    };

    let mut record = Map::new();
    let (appended, stats) = match &outcome {
        Ok(answered) => (
            vec![message_record(answered.message())],
            vec![usage_record(answered.usage)],
        ),
        Err(failed) => {
            let normalize = [
                (scenario.stand_in.base.as_str(), "$BASE"),
                (scenario.closed, "$CLOSED"),
                (env!("CARGO_PKG_VERSION"), "<version>"),
            ];
            record.insert(
                "error".to_owned(),
                error_record(&failed.failure, scenario.served, &normalize),
            );
            (
                failed.appended.iter().map(message_record).collect(),
                failed.usage.map(usage_record).into_iter().collect(),
            )
        }
    };
    if let Some(first) = pieces.first() {
        let total = pieces[1..]
            .iter()
            .try_fold(first.clone(), |total, piece| total.merge(piece.clone()))
            .expect("published pieces merge");
        record.insert("result".to_owned(), chunk_record(&total));
        let deltas = |read: fn(&Message) -> Option<&String>| -> Vec<Value> {
            pieces
                .iter()
                .filter_map(|piece| read(&piece.message).filter(|text| !text.is_empty()))
                .map(|text| json!(text))
                .collect()
        };
        record.insert(
            "deltas".to_owned(),
            json!({
                "text": deltas(|message| message.content.as_ref()),
                "reasoning": deltas(|message| message.reasoning_content.as_ref()),
            }),
        );
    }
    record.insert("appended".to_owned(), Value::Array(appended));
    record.insert("stats".to_owned(), Value::Array(stats));
    record.insert(
        "requests".to_owned(),
        Value::Array(scenario.stand_in.requests()[before_requests..].to_vec()),
    );
    record.insert(
        "sleeps".to_owned(),
        Value::Array(scenario.clock.sleeps()[before_sleeps..].to_vec()),
    );
    record.insert(
        "retries".to_owned(),
        Value::Array(scenario.recorder.reasons()[before_retries..].to_vec()),
    );
    Value::Object(record)
}

fn substitute(value: &Value, from: &[(&str, &str)]) -> Value {
    match value {
        Value::String(text) => {
            Value::String(from.iter().fold(text.clone(), |text, (needle, with)| {
                text.replace(needle, with)
            }))
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| substitute(item, from)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), substitute(item, from)))
                .collect(),
        ),
        other => other.clone(),
    }
}

async fn run_scenario(entry: Value) -> (String, Value, Value) {
    let name = entry["name"]
        .as_str()
        .expect("a scenario is named")
        .to_owned();
    let stand_in =
        StandIn::start(entry["responses"].as_array().map_or(&[][..], Vec::as_slice)).await;
    let closed = closed_port();
    let served = Ordered::from(&entry["responses"]).dumps(false);
    let provider_entry = substitute(
        &entry["provider"],
        &[
            ("$BASE", stand_in.base.as_str()),
            ("$CLOSED", closed.as_str()),
        ],
    );
    let provider = ProviderConfig::from_table(&toml_table(&provider_entry))
        .expect("a scenario provider reads");
    let model = ModelConfig::from_table(&toml_table(&entry["model"]), None)
        .expect("a scenario model reads");
    let credentials: BTreeMap<String, String> = entry["env"]
        .as_object()
        .map(|env| {
            env.iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let api = &entry["api"];
    let clock = Arc::new(FakeClock::new());
    let context = BackendContext {
        api: ApiSettings {
            timeout: seconds(api, "timeout"),
            retry_max_elapsed_time: seconds(api, "retryMaxElapsedTime"),
            connect_timeout: seconds(api, "connectTimeout"),
            write_timeout: seconds(api, "writeTimeout"),
            pool_timeout: seconds(api, "poolTimeout"),
        },
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        credentials: Arc::new(MapCredentials(credentials)),
        vertex: Arc::new(FakeVertex(stand_in.base.clone())),
    };
    let recorder = Recorder::default();
    let mut calls = Vec::new();
    match Backend::new(provider.clone(), context) {
        Ok(backend) => {
            let scenario = Scenario {
                backend: &backend,
                provider: &provider,
                model: &model,
                stand_in: &stand_in,
                clock: &clock,
                recorder: &recorder,
                served: &served,
                closed: &closed,
            };
            for call in entry["calls"].as_array().expect("a scenario has calls") {
                calls.push(run_call(&scenario, call).await);
            }
        }
        Err(failure) => calls.push(json!({"construction": failure.kind.reference_class()})),
    }
    let recorded: usize = calls
        .iter()
        .filter_map(|call| call["retries"].as_array())
        .map(Vec::len)
        .sum();
    let observed = json!({
        "calls": calls,
        "pendingRetries": recorder.reasons()[recorded..].to_vec(),
    });
    let observed = substitute(
        &observed,
        &[
            (closed.as_str(), "$CLOSED"),
            (stand_in.base.as_str(), "$BASE"),
            (env!("CARGO_PKG_VERSION"), "<version>"),
        ],
    );
    (name, entry["observed"].clone(), observed)
}

// --------------------------------------------------------------------------
// Comparing
// --------------------------------------------------------------------------

fn is_digest(value: &Value) -> bool {
    value.as_object().is_some_and(|fields| {
        fields.len() == 2 && fields.contains_key("length") && fields.contains_key("sha256")
    })
}

/// Every JSON pointer at which `port` departs from `reference`. A digest in
/// the corpus is the reference's own prose: this port's value there must be a
/// digest too, and must never equal it.
fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    if is_digest(reference) {
        if !is_digest(port) {
            found.push(pointer.to_owned());
        } else if reference == port {
            found.push(format!("{pointer} (copies the reference's prose)"));
        }
        return;
    }
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let child = format!("{pointer}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        (Value::Number(left), Value::Number(right)) if pointer.ends_with("/seconds") => {
            let (left, right) = (
                left.as_f64().unwrap_or(f64::NAN),
                right.as_f64().unwrap_or(0.0),
            );
            if (left - right).abs() > 1e-6 {
                found.push(pointer.to_owned());
            }
        }
        (Value::Number(left), Value::Number(right)) => {
            if left.as_f64() != right.as_f64() {
                found.push(pointer.to_owned());
            }
        }
        _ if reference != port => found.push(pointer.to_owned()),
        _ => {}
    }
}

fn resolve(value: &Value, pointer: &str) -> String {
    let pointer = pointer.split(' ').next().unwrap_or_default();
    value.pointer(pointer).map_or_else(
        || "<absent>".to_owned(),
        |value| {
            let text = value.to_string();
            text.chars().take(300).collect()
        },
    )
}

#[test]
fn the_backends_answer_every_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let scenarios = corpus["scenarios"]
        .as_array()
        .expect("the corpus holds scenarios")
        .clone();
    assert!(
        scenarios.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        scenarios.len()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime starts");
    // A scenario that times a read out within a second runs alone, after the
    // others, so contention on a loaded machine cannot trip its deadline.
    let (tight, relaxed): (Vec<Value>, Vec<Value>) = scenarios.into_iter().partition(|entry| {
        entry["api"]["timeout"]
            .as_f64()
            .is_some_and(|timeout| timeout < 1.0)
    });
    let results: Vec<(String, Value, Value)> = runtime.block_on(async {
        let mut results: Vec<_> = futures_util::stream::iter(relaxed.into_iter().map(run_scenario))
            .buffer_unordered(JOBS)
            .collect()
            .await;
        for entry in tight {
            results.push(run_scenario(entry).await);
        }
        results
    });

    let mut unexplained = Vec::new();
    let mut reproduced = vec![0_usize; LEDGER.len()];
    let mut conformant = 0;
    let mut families: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (name, reference, port) in &results {
        let mut found = Vec::new();
        differences(reference, port, "", &mut found);
        let family = name.split('/').next().unwrap_or_default().to_owned();
        let tally = families.entry(family).or_default();
        tally.1 += 1;
        if found.is_empty() {
            conformant += 1;
            tally.0 += 1;
        }
        for pointer in found {
            match LEDGER.iter().position(|entry| {
                name.starts_with(entry.scenario) && pointer.ends_with(entry.suffix)
            }) {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!(
                    "{name} {pointer}\n    reference: {}\n    port:      {}",
                    resolve(reference, &pointer),
                    resolve(port, &pointer)
                )),
            }
        }
    }
    let stale: Vec<String> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| format!("{} {}", entry.scenario, entry.suffix))
        .collect();
    println!(
        "llm backend parity: {conformant}/{} scenarios conformant ({families:?}), {} ledgered \
         differences across {} entries",
        results.len(),
        reproduced.iter().sum::<usize>(),
        LEDGER.len()
    );
    unexplained.sort();
    assert!(
        unexplained.is_empty(),
        "{} departures from the corpus no ledger entry names:\n{}",
        unexplained.len(),
        unexplained.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these ledger entries no longer reproduce and should be removed: {stale:?}"
    );
}

#[test]
fn the_generic_backoff_matches_every_recorded_delay() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    for case in corpus["retryDelays"]
        .as_array()
        .expect("the corpus holds delays")
    {
        let number = |key: &str| case[key].as_f64().expect("a delay field is a number");
        let attempt =
            u32::try_from(case["attempt"].as_u64().expect("an attempt")).expect("an attempt fits");
        let seconds = next_delay(
            None,
            attempt,
            number("delay"),
            number("factor"),
            number("cap"),
        );
        assert!(
            (seconds - number("seconds")).abs() < 1e-9,
            "{case}: this port waits {seconds}"
        );
    }
}

#[test]
fn the_utility_model_is_selected_as_the_corpus_records() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    for case in corpus["utilitySelections"]
        .as_array()
        .expect("the corpus holds selections")
    {
        let active = &case["active"];
        let mut model = ModelConfig::new(
            active["name"].as_str().expect("an active model"),
            active["provider"].as_str().expect("an active provider"),
        );
        model.alias = active["alias"].as_str().expect("an alias").to_owned();
        let providers = case["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|entry| {
                let mut provider = ProviderConfig::new(
                    entry["name"].as_str().expect("a provider name"),
                    entry["apiBase"].as_str().expect("an API base"),
                );
                provider.api_key_env_var =
                    entry["keyVariable"].as_str().unwrap_or_default().to_owned();
                if entry["backend"].as_str() == Some("mistral") {
                    provider.backend = crate::provider::config::BackendKind::Mistral;
                }
                provider
            })
            .collect();
        let routing = ModelRouting {
            providers,
            models: vec![model],
            active_alias: active["alias"].as_str().map(str::to_owned),
            allowed_models: case["allowedModels"]
                .as_array()
                .expect("an allowlist")
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            api: ApiSettings::default(),
        };
        let credentials = MapCredentials(
            case["env"]
                .as_array()
                .expect("an environment")
                .iter()
                .filter_map(Value::as_str)
                .map(|name| (name.to_owned(), "oracle-key".to_owned()))
                .collect(),
        );
        let selected = utility::select(&routing, &credentials).expect("a selection");
        let port = json!({
            "model": selected.model.name,
            "alias": selected.model.alias,
            "provider": selected.provider.name,
            "fast": selected.is_fast(),
            "temperature": selected.model.temperature,
        });
        for key in ["model", "alias", "provider", "fast", "temperature"] {
            assert_eq!(case[key], port[key], "{}: {key}", case["name"]);
        }
    }
}

#[test]
fn vertex_requests_go_to_the_recorded_regional_endpoints() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    for case in corpus["vertexEndpoints"]
        .as_array()
        .expect("the corpus holds endpoints")
    {
        let region = case["region"].as_str().expect("a region");
        let streaming = case["streaming"].as_bool().expect("a mode");
        let url = super::vertex::base_url(region)
            + &super::vertex::endpoint(region, "proj", "claude-x@1", streaming);
        assert_eq!(case["url"].as_str(), Some(url.as_str()));
    }
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let rows: BTreeSet<&str> = SCORECARD
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|line| line.split_once(" |"))
        .map(|(number, _)| number.trim())
        .filter(|number| number.parse::<u32>().is_ok())
        .collect();
    for entry in LEDGER {
        assert!(
            rows.contains(entry.row),
            "{} {} names row {}, which docs/parity.md does not carry",
            entry.scenario,
            entry.suffix,
            entry.row
        );
        assert!(
            entry.suffix.starts_with('/') && !entry.reason.is_empty(),
            "{} {} needs a pointer suffix and a reason",
            entry.scenario,
            entry.suffix
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "llm backends") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::process::Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .arg("--check")
        .arg("--reference")
        .arg(&root)
        .current_dir(&repository)
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the capture script");
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `{CAPTURE_SCRIPT} --corpus`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
