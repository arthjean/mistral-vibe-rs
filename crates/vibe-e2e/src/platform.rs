//! The scripted model platform a world points every host at.
//!
//! Chat completions are answered from scripted [`Reply`] queues: a probe queue
//! for one-token requests that offer no tools, marker queues for requests
//! whose messages carry a scenario's marker, and the turn queue for the rest.
//! Every other path is a service route ([`crate::routes`]). Every request lands
//! in one journal in arrival order, and anything a scenario did not script
//! (an exhausted queue, an unknown path, a tool message answering a call no
//! assistant message declared) becomes a failure the trial reports.

use std::collections::{BTreeMap, VecDeque};
use std::io::BufReader;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::http::{self, Chunked, Request};
use crate::routes::{self, RouteReply};
use crate::{DEFAULT_DEADLINE, Failure, Outcome, scaled};

/// One scripted answer to a chat-completions request.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    text: Vec<String>,
    reasoning: Option<String>,
    thinking: Option<String>,
    tool_calls: Vec<(String, String)>,
    usage: Option<(u64, u64)>,
    finish: Finish,
    status: Option<u16>,
    body: Option<String>,
    headers: Vec<(String, String)>,
    raw: Option<Vec<String>>,
    pace: Duration,
    malformed: bool,
    no_done: bool,
    cut_before_last: bool,
    drop_after: Option<usize>,
}

#[derive(Debug, Clone, Default)]
enum Finish {
    #[default]
    Inferred,
    Reason(String),
    Absent,
}

impl Reply {
    /// An answer whose text arrives as one chunk.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: vec![text.into()],
            ..Self::default()
        }
    }

    /// An answer whose text arrives as one chunk per piece.
    pub fn chunks<I, S>(pieces: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            text: pieces.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// An answer that calls one tool with `arguments`.
    pub fn tool_call(name: impl Into<String>, arguments: Value) -> Self {
        Self::default().and_tool_call(name, arguments)
    }

    /// Adds another tool call to the same answer.
    #[must_use]
    pub fn and_tool_call(mut self, name: impl Into<String>, arguments: Value) -> Self {
        self.tool_calls.push((name.into(), arguments.to_string()));
        self
    }

    /// Adds a tool call whose arguments are sent verbatim, valid JSON or not.
    #[must_use]
    pub fn and_raw_tool_call(
        mut self,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        self.tool_calls.push((name.into(), arguments.into()));
        self
    }

    /// Streams generic `reasoning_content` before the text.
    #[must_use]
    pub fn reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Streams a Mistral thinking chunk before the text.
    #[must_use]
    pub fn thinking(mut self, thinking: impl Into<String>) -> Self {
        self.thinking = Some(thinking.into());
        self
    }

    /// The token counts the last chunk reports (default 10 and 5).
    #[must_use]
    pub fn usage(mut self, prompt: u64, completion: u64) -> Self {
        self.usage = Some((prompt, completion));
        self
    }

    /// The finish reason the last chunk carries; by default `tool_calls` when
    /// the answer calls a tool and `stop` otherwise.
    #[must_use]
    pub fn finish(mut self, reason: impl Into<String>) -> Self {
        self.finish = Finish::Reason(reason.into());
        self
    }

    /// Leaves the finish reason out of the last chunk.
    #[must_use]
    pub fn no_finish(mut self) -> Self {
        self.finish = Finish::Absent;
        self
    }

    /// Answers with an HTTP status and an optional body instead of a stream.
    pub fn status(status: u16) -> Self {
        Self {
            status: Some(status),
            ..Self::default()
        }
    }

    /// The body an HTTP status answer carries.
    #[must_use]
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// A response header, such as `Retry-After`.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .push((name.into().to_ascii_lowercase(), value.into()));
        self
    }

    /// A stream of raw server-sent events, each written as given and followed
    /// by a blank line.
    pub fn sse<I, S>(events: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            raw: Some(events.into_iter().map(Into::into).collect()),
            ..Self::default()
        }
    }

    /// Waits `milliseconds` before each event after the first.
    #[must_use]
    pub fn paced(mut self, milliseconds: u64) -> Self {
        self.pace = Duration::from_millis(milliseconds);
        self
    }

    /// Inserts a data line that is not JSON before the last chunk.
    #[must_use]
    pub fn malformed(mut self) -> Self {
        self.malformed = true;
        self
    }

    /// Leaves out the closing `[DONE]` event.
    #[must_use]
    pub fn no_done(mut self) -> Self {
        self.no_done = true;
        self
    }

    /// Ends the stream cleanly before its last event.
    #[must_use]
    pub fn cut_before_last(mut self) -> Self {
        self.cut_before_last = true;
        self
    }

    /// Drops the connection, without ending the body, after `events` events.
    #[must_use]
    pub fn drop_after(mut self, events: usize) -> Self {
        self.drop_after = Some(events);
        self
    }

    fn finish_reason(&self) -> Option<String> {
        match &self.finish {
            Finish::Inferred if self.tool_calls.is_empty() => Some("stop".to_owned()),
            Finish::Inferred => Some("tool_calls".to_owned()),
            Finish::Reason(reason) => Some(reason.clone()),
            Finish::Absent => None,
        }
    }

    fn usage_value(&self) -> Value {
        let (prompt, completion) = self.usage.unwrap_or((10, 5));
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        })
    }

    fn calls(&self, ids: &[String]) -> Vec<Value> {
        self.tool_calls
            .iter()
            .zip(ids)
            .enumerate()
            .map(|(index, ((name, arguments), id))| {
                json!({
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                })
            })
            .collect()
    }

    /// The events of the stream, each a full `data:` block.
    fn events(&self, model: &str, ids: &[String]) -> Vec<String> {
        if let Some(raw) = &self.raw {
            return raw.clone();
        }
        let chunk = |delta: Value, finish: Option<String>, usage: Option<Value>| {
            let mut body = json!({
                "id": "cmpl-e2e",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            });
            if let (Some(usage), Some(object)) = (usage, body.as_object_mut()) {
                object.insert("usage".to_owned(), usage);
            }
            format!("data: {body}")
        };
        let mut events = vec![chunk(
            json!({"role": "assistant", "content": ""}),
            None,
            None,
        )];
        if let Some(thinking) = &self.thinking {
            events.push(chunk(
                json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": thinking}]}]}),
                None,
                None,
            ));
        }
        if let Some(reasoning) = &self.reasoning {
            events.push(chunk(json!({"reasoning_content": reasoning}), None, None));
        }
        for piece in &self.text {
            events.push(chunk(json!({"content": piece}), None, None));
        }
        for call in self.calls(ids) {
            events.push(chunk(json!({"tool_calls": [call]}), None, None));
        }
        if self.malformed {
            events.push("data: {this is not json".to_owned());
        }
        events.push(chunk(
            json!({"content": ""}),
            self.finish_reason(),
            Some(self.usage_value()),
        ));
        if !self.no_done {
            events.push("data: [DONE]".to_owned());
        }
        if self.cut_before_last {
            events.pop();
        }
        events
    }

    fn completion(&self, model: &str, ids: &[String]) -> Value {
        let mut message = json!({"role": "assistant", "content": self.text.concat()});
        let calls = self.calls(ids);
        if let (false, Some(object)) = (calls.is_empty(), message.as_object_mut()) {
            object.insert("tool_calls".to_owned(), Value::Array(calls));
        }
        json!({
            "id": "cmpl-e2e",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": self.finish_reason()}],
            "usage": self.usage_value(),
        })
    }
}

/// One request the platform received.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Position in the journal, from 0.
    pub index: usize,
    pub method: String,
    /// The path without its query string.
    pub path: String,
    /// `turn`, `probe`, `when:<marker>`, a route name, or `unknown`.
    pub queue: String,
    /// Header names lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The body parsed as JSON, or as a string when it is not JSON.
    pub body: Value,
    /// Milliseconds since the platform started.
    pub at_ms: u64,
}

impl Entry {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether this is a chat-completions request.
    pub fn is_chat(&self) -> bool {
        self.path.ends_with("/chat/completions")
    }

    /// The `messages` array of a chat request.
    pub fn messages(&self) -> &[Value] {
        self.body
            .get("messages")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "index": self.index,
            "method": self.method,
            "path": self.path,
            "queue": self.queue,
            "headers": self.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "body": self.body,
            "atMs": self.at_ms,
        })
    }
}

/// Holds one scripted turn response until the scenario releases it.
#[derive(Clone)]
pub struct Gate {
    turn: usize,
    shared: Arc<Shared>,
}

impl Gate {
    pub fn release(&self) {
        self.shared.lock().gates.insert(self.turn, true);
        self.shared.changed.notify_all();
    }
}

#[derive(Default)]
struct State {
    turn: VecDeque<Reply>,
    turns_served: usize,
    probe: VecDeque<Reply>,
    markers: Vec<(String, VecDeque<Reply>)>,
    journal: Vec<Entry>,
    chat: Vec<usize>,
    failures: Vec<String>,
    exact: bool,
    gates: BTreeMap<usize, bool>,
    routes: BTreeMap<String, RouteReply>,
    pages: BTreeMap<String, String>,
    next_call: usize,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    started: Instant,
    stop: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The loopback platform: one listener for the provider and every service.
#[derive(Clone)]
pub struct Platform {
    base: String,
    address: SocketAddr,
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
}

/// Stops the listener when the last handle goes.
struct Owner {
    address: SocketAddr,
    shared: Arc<Shared>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        let _ = TcpStream::connect(self.address);
    }
}

impl Platform {
    pub fn start() -> Outcome<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            started: Instant::now(),
            stop: AtomicBool::new(false),
        });
        let accepting = Arc::clone(&shared);
        thread::Builder::new()
            .name("e2e-platform".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accepting.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let shared = Arc::clone(&accepting);
                    let _ = thread::Builder::new()
                        .name("e2e-platform-connection".to_owned())
                        .spawn(move || serve(stream, &shared));
                }
            })?;
        Ok(Self {
            base: format!("http://{address}"),
            address,
            shared: Arc::clone(&shared),
            _owner: Arc::new(Owner { address, shared }),
        })
    }

    /// `http://127.0.0.1:<port>`, with no trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Queues one turn response.
    pub fn reply(&self, reply: Reply) {
        self.shared.lock().turn.push_back(reply);
    }

    /// Queues turn responses in order.
    pub fn replies(&self, replies: impl IntoIterator<Item = Reply>) {
        self.shared.lock().turn.extend(replies);
    }

    /// Queues an answer for a one-token request that offers no tools.
    pub fn probe_reply(&self, reply: Reply) {
        self.shared.lock().probe.push_back(reply);
    }

    /// Queues an answer for the next request whose serialized messages contain
    /// `marker`, served before the turn queue.
    pub fn when(&self, marker: impl Into<String>, reply: Reply) {
        let marker = marker.into();
        let mut state = self.shared.lock();
        if let Some((_, queue)) = state.markers.iter_mut().find(|(m, _)| *m == marker) {
            queue.push_back(reply);
        } else {
            state.markers.push((marker, VecDeque::from([reply])));
        }
    }

    /// Fails the trial when a scripted turn response is left unserved.
    pub fn exact(&self) {
        self.shared.lock().exact = true;
    }

    /// Holds the `turn`-th turn response (from 1) until [`Gate::release`].
    pub fn gate(&self, turn: usize) -> Gate {
        self.shared.lock().gates.insert(turn, false);
        Gate {
            turn,
            shared: Arc::clone(&self.shared),
        }
    }

    /// Replaces what the service route `name` answers.
    pub fn route(&self, name: &str, reply: RouteReply) {
        self.shared.lock().routes.insert(name.to_owned(), reply);
    }

    /// Serves `html` at `/pages/<name>`, for `web_fetch`.
    pub fn page(&self, name: &str, html: impl Into<String>) {
        self.shared
            .lock()
            .pages
            .insert(name.to_owned(), html.into());
    }

    /// The URL of a page served by [`Self::page`].
    pub fn page_url(&self, name: &str) -> String {
        format!("{}/pages/{name}", self.base)
    }

    /// Every request so far, in arrival order.
    pub fn journal(&self) -> Vec<Entry> {
        self.shared.lock().journal.clone()
    }

    /// The chat-completions requests so far, in arrival order.
    pub fn chat_requests(&self) -> Vec<Entry> {
        let state = self.shared.lock();
        state
            .chat
            .iter()
            .map(|&i| state.journal[i].clone())
            .collect()
    }

    /// Waits until the `n`-th chat-completions request (from 1) is journaled.
    pub fn await_request(&self, n: usize, deadline: Duration) -> Outcome<Entry> {
        let until = Instant::now() + scaled(deadline);
        let mut state = self.shared.lock();
        loop {
            if let Some(&index) = state.chat.get(n.saturating_sub(1)) {
                return Ok(state.journal[index].clone());
            }
            let now = Instant::now();
            if now >= until {
                return Err(Failure(format!(
                    "chat request #{n} never arrived within {:?}; {} arrived",
                    scaled(deadline),
                    state.chat.len()
                )));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, until - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// What went wrong on the platform side, including scripted turn responses
    /// left unserved when [`Self::exact`] was declared.
    pub fn failures(&self) -> Vec<String> {
        let state = self.shared.lock();
        let mut failures = state.failures.clone();
        if state.exact && !state.turn.is_empty() {
            failures.push(format!(
                "expected {} more turn request(s): {} scripted turn response(s) were served and {} \
                 were left",
                state.turn.len(),
                state.turns_served,
                state.turn.len()
            ));
        }
        failures
    }
}

fn serve(stream: TcpStream, shared: &Arc<Shared>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let request = match http::read_request(&mut reader) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(error) => {
            shared
                .lock()
                .failures
                .push(format!("unreadable request: {error}"));
            return;
        }
    };
    let mut stream = stream;
    if request.method == "POST" && request.path().ends_with("/chat/completions") {
        chat(&mut stream, &request, shared);
    } else {
        service(&mut stream, &request, shared);
    }
}

fn parsed_body(body: &[u8]) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
}

fn journal(state: &mut State, shared: &Shared, request: &Request, queue: String) -> usize {
    let index = state.journal.len();
    state.journal.push(Entry {
        index,
        method: request.method.clone(),
        path: request.path().to_owned(),
        queue,
        headers: request.headers.clone(),
        body: parsed_body(&request.body),
        at_ms: u64::try_from(shared.started.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
    index
}

/// The tool-call ids a request's tool messages answer without an earlier
/// assistant message declaring them, with the ids declared so far.
pub(crate) fn orphan_tool_messages(messages: &[Value]) -> Vec<(String, Vec<String>)> {
    let mut declared: Vec<String> = Vec::new();
    let mut orphans = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        declared.push(id.to_owned());
                    }
                }
            }
            Some("tool") => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !declared.iter().any(|d| d == id) {
                    orphans.push((id.to_owned(), declared.clone()));
                }
            }
            _ => {}
        }
    }
    orphans
}

fn is_probe(body: &Value) -> bool {
    body.get("max_tokens").and_then(Value::as_u64) == Some(1)
        && body
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
}

fn last_user_text(body: &Value) -> String {
    let text = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .map(|content| match content {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    text.chars().take(120).collect()
}

fn chat(stream: &mut TcpStream, request: &Request, shared: &Arc<Shared>) {
    let body = parsed_body(&request.body);
    let mut state = shared.lock();
    for (orphan, declared) in orphan_tool_messages(
        body.get("messages")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice),
    ) {
        state.failures.push(format!(
            "a tool message answers tool call {orphan:?}, which no earlier assistant message \
             declared (declared: {declared:?})"
        ));
    }
    let serialized = body
        .get("messages")
        .map(Value::to_string)
        .unwrap_or_default();
    let (queue, reply, turn) = if is_probe(&body) {
        let reply = state.probe.pop_front().unwrap_or_else(|| Reply::text("ok"));
        ("probe".to_owned(), Some(reply), None)
    } else if let Some(position) = state
        .markers
        .iter()
        .position(|(marker, queue)| !queue.is_empty() && serialized.contains(marker.as_str()))
    {
        let (marker, queue) = &mut state.markers[position];
        let marker = marker.clone();
        (format!("when:{marker}"), queue.pop_front(), None)
    } else {
        let reply = state.turn.pop_front();
        if reply.is_some() {
            state.turns_served += 1;
        }
        let turn = state.turns_served;
        ("turn".to_owned(), reply, Some(turn))
    };
    let index = journal(&mut state, shared, request, queue);
    state.chat.push(index);
    shared.changed.notify_all();
    let Some(reply) = reply else {
        let number = state.chat.len();
        state.failures.push(format!(
            "unexpected turn request #{number}: no scripted turn response was left (last user \
             message: {:?})",
            last_user_text(&body)
        ));
        drop(state);
        let _ = http::respond(
            stream,
            500,
            &[("content-type".to_owned(), "text/plain".to_owned())],
            b"e2e platform: no scripted turn response left for this request",
        );
        return;
    };
    let ids: Vec<String> = reply
        .tool_calls
        .iter()
        .map(|_| {
            state.next_call += 1;
            format!("e2e{:06}", state.next_call)
        })
        .collect();
    if let Some(turn) = turn {
        let until = Instant::now() + scaled(DEFAULT_DEADLINE * 4);
        while state.gates.get(&turn) == Some(&false) && !shared.stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= until {
                state.failures.push(format!(
                    "the gate on turn response #{turn} was never released"
                ));
                break;
            }
            state = shared
                .changed
                .wait_timeout(state, until - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
    drop(state);
    let model = body.get("model").and_then(Value::as_str).unwrap_or("e2e");
    let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
    let _ = answer(stream, &reply, model, &ids, streamed);
}

fn answer(
    stream: &mut TcpStream,
    reply: &Reply,
    model: &str,
    ids: &[String],
    streamed: bool,
) -> std::io::Result<()> {
    if let Some(status) = reply.status {
        let body = reply.body.clone().unwrap_or_default();
        let mut headers = reply.headers.clone();
        if !headers.iter().any(|(name, _)| name == "content-type") {
            let kind = if serde_json::from_str::<Value>(&body).is_ok() {
                "application/json"
            } else {
                "text/plain"
            };
            headers.push(("content-type".to_owned(), kind.to_owned()));
        }
        return http::respond(stream, status, &headers, body.as_bytes());
    }
    if !streamed && reply.raw.is_none() {
        let body = reply.completion(model, ids).to_string();
        let mut headers = reply.headers.clone();
        headers.push(("content-type".to_owned(), "application/json".to_owned()));
        return http::respond(stream, 200, &headers, body.as_bytes());
    }
    let mut headers = reply.headers.clone();
    headers.push(("content-type".to_owned(), "text/event-stream".to_owned()));
    headers.push(("cache-control".to_owned(), "no-cache".to_owned()));
    let mut body = Chunked::begin(stream, 200, &headers)?;
    for (position, event) in reply.events(model, ids).iter().enumerate() {
        if reply.drop_after == Some(position) {
            body.drop_connection();
            return Ok(());
        }
        if position > 0 && !reply.pace.is_zero() {
            thread::sleep(reply.pace);
        }
        body.chunk(format!("{event}\n\n").as_bytes())?;
    }
    body.finish()
}

fn service(stream: &mut TcpStream, request: &Request, shared: &Arc<Shared>) {
    let mut state = shared.lock();
    let route = routes::resolve(&request.method, request.path());
    let queue = route
        .as_ref()
        .map_or("unknown", |route| route.name)
        .to_owned();
    journal(&mut state, shared, request, queue);
    shared.changed.notify_all();
    let Some(route) = route else {
        state.failures.push(format!(
            "unexpected request {} {}: no platform route serves it",
            request.method,
            request.path()
        ));
        drop(state);
        let _ = http::respond(
            stream,
            404,
            &[("content-type".to_owned(), "application/json".to_owned())],
            br#"{"detail":"Not Found"}"#,
        );
        return;
    };
    let reply = state
        .routes
        .get(route.name)
        .cloned()
        .unwrap_or_else(|| (route.default)(request.path(), &state.pages));
    drop(state);
    if !reply.delay.is_zero() {
        thread::sleep(reply.delay);
    }
    let _ = http::respond(stream, reply.status, &reply.headers, reply.body.as_bytes());
}

#[cfg(test)]
mod platform_tests;
