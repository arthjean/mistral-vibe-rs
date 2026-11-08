//! The LLM backend corpus's shared replay machinery and its backoff family.
//!
//! `scripts/parity/llm_backends.py` drives what stays of the reference's
//! model layer at v3.0.0 against a scripted HTTP stand-in on a fake clock.
//! This module holds that stand-in, the fake clock and the comparison the
//! utility-model replays in `availability_parity_tests` share, and replays
//! the backoff arithmetic. The in-process backends the corpus once also
//! measured left the reference with v3.0.0; the agent-loop and
//! unified-harness oracles record the requests a model call sends.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::retry::{Clock, SleepFuture, SleepKind, next_delay};
use super::{TokenFuture, VertexAccess};
use crate::parity::replay::Capture;
use crate::pycompat::{self, Dumps, truthy};

/// `json.dumps(value, ensure_ascii=False)`, how the stand-in serves a body.
const UNICODE: Dumps = Dumps {
    ensure_ascii: false,
    ..Dumps::DEFAULT
};

pub(super) const CORPUS: &str = include_str!("../../tests/llm-backends/corpus.json");
const CAPTURE_SCRIPT: &str = "scripts/parity/llm_backends.py";
/// Headers a transport adds on its own, which the stand-in does not record.
const TRANSPORT_HEADERS: &[&str] = &["host", "content-length", "accept-encoding", "connection"];

// --------------------------------------------------------------------------
// The scripted stand-in
// --------------------------------------------------------------------------

pub(super) struct StandIn {
    pub(super) base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl StandIn {
    pub(super) async fn start(responses: &[Value]) -> Self {
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

    pub(super) fn requests(&self) -> Vec<Value> {
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
        if response.get("drop").is_some_and(truthy) {
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
                    response.get("crlf").is_some_and(truthy),
                ),
                "text/event-stream",
            )
        } else if let Some(value) = response.get("json") {
            (
                pycompat::dumps(value, UNICODE).into_bytes(),
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
        let truncate = response.get("truncate").is_some_and(truthy);
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
            other => pycompat::dumps(other, UNICODE),
        };
        out.push_str(&format!("data: {payload}{newline}{newline}"));
    }
    out.into_bytes()
}

pub(super) fn closed_port() -> String {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a probe port binds");
    let port = probe.local_addr().expect("the probe has an address").port();
    drop(probe);
    format!("http://127.0.0.1:{port}")
}

// --------------------------------------------------------------------------
// The fake clock, the retry recorder and the Vertex stand-in
// --------------------------------------------------------------------------

pub(super) struct FakeClock {
    state: Mutex<(f64, Vec<Value>)>,
}

impl FakeClock {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new((1_000.0, Vec::new())),
        }
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

pub(super) struct FakeVertex(pub(super) String);

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

pub(super) fn toml_table(value: &Value) -> toml::Table {
    let value = toml::Value::try_from(value).expect("a scenario entry converts to TOML");
    value
        .as_table()
        .cloned()
        .expect("a scenario entry is a table")
}

pub(super) fn substitute(value: &Value, from: &[(&str, &str)]) -> Value {
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
pub(super) fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
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

pub(super) fn resolve(value: &Value, pointer: &str) -> String {
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

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    // `--corpus` names the committed corpus, which `--check` compares anyway,
    // so the regeneration hint a failure prints is the command that writes it.
    Capture {
        script: CAPTURE_SCRIPT,
        family: &["--corpus"],
        arguments: &[],
    }
    .probe("llm backends");
}
