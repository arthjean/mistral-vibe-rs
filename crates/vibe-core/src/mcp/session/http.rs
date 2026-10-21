//! The streamable HTTP transport: every message a `POST`, every answer a JSON
//! body or an event stream, and a standing `GET` for what the server sends
//! outside an answer.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;

use super::{Inbound, SessionError, protocol};
use crate::sync::lock;

/// Reference `create_vibe_mcp_http_client` names the client unless the entry
/// declares its own user agent. The product token is the reference's; the
/// version is this build's, as `telemetry_user_agent` reports it.
const USER_AGENT_PRODUCT: &str = "MistralAI-VibeCLI";

/// SDK `create_mcp_http_client` and reference `_MCP_DEFAULT_TIMEOUT`: the
/// connect and write budget of every HTTP exchange.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Reference `_MCP_DEFAULT_SSE_READ_TIMEOUT`: how long one read of an answer
/// may wait, which bounds the silence of a streamed answer.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the handshake waits for the standing `GET` to be answered.
const LISTENING_STREAM_WAIT: Duration = Duration::from_secs(1);

/// What every exchange of one HTTP session shares.
struct HttpShared {
    session_id: StdMutex<Option<HeaderValue>>,
    protocol_version: StdMutex<Option<HeaderValue>>,
}

pub(super) struct HttpChannel {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    shared: Arc<HttpShared>,
    sender: mpsc::UnboundedSender<Inbound>,
    tasks: Vec<JoinHandle<()>>,
}

impl HttpChannel {
    pub(super) fn new(
        url: &Url,
        declared: &BTreeMap<String, String>,
        sender: mpsc::UnboundedSender<Inbound>,
    ) -> Result<Self, SessionError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if !declared
            .keys()
            .any(|name| name.eq_ignore_ascii_case("user-agent"))
        {
            let agent = format!("{USER_AGENT_PRODUCT}/{}", crate::telemetry::version());
            headers.insert(
                USER_AGENT,
                HeaderValue::from_str(&agent)
                    .map_err(|error| SessionError::Transport(error.to_string()))?,
            );
        }
        for (name, value) in declared {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                SessionError::Transport(format!("invalid HTTP header name `{name}`"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                SessionError::Transport(format!("invalid value for HTTP header `{name}`"))
            })?;
            headers.insert(name, value);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT)
            .read_timeout(HTTP_READ_TIMEOUT)
            .build()
            .map_err(|error| SessionError::Transport(error.to_string()))?;
        Ok(Self {
            client,
            url: url.clone(),
            headers,
            shared: Arc::new(HttpShared {
                session_id: StdMutex::new(None),
                protocol_version: StdMutex::new(None),
            }),
            sender,
            tasks: Vec::new(),
        })
    }

    pub(super) fn negotiated(&mut self, version: &str) {
        if let Ok(value) = HeaderValue::from_str(version) {
            *lock(&self.shared.protocol_version) = Some(value);
        }
    }

    /// SDK `StreamableHTTPTransport._prepare_headers`: the base headers, the
    /// session id once the server assigned one, and the negotiated revision
    /// once the handshake settled it.
    fn prepared(shared: &HttpShared, base: &HeaderMap) -> HeaderMap {
        let mut headers = base.clone();
        if let Some(session) = lock(&shared.session_id).as_ref() {
            headers.insert("mcp-session-id", session.clone());
        }
        if let Some(version) = lock(&shared.protocol_version).as_ref() {
            headers.insert("mcp-protocol-version", version.clone());
        }
        headers
    }

    /// Posts a request and pumps whatever answers it into the session's
    /// queue, from a task of its own so that a request the server makes
    /// mid-stream is answered while the stream stays open.
    pub(super) fn post_request(&mut self, message: &Value, initialize: bool) {
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = Self::prepared(&self.shared, &self.headers);
        let shared = self.shared.clone();
        let sender = self.sender.clone();
        let body = message.to_string();
        self.tasks.push(tokio::spawn(async move {
            let response = match client.post(url).headers(headers).body(body).send().await {
                Ok(response) => response,
                Err(error) => {
                    let _ = sender.send(Inbound::Failed(SessionError::Transport(
                        error.without_url().to_string(),
                    )));
                    return;
                }
            };
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let _ = sender.send(Inbound::Failed(SessionError::Unauthorized));
                return;
            }
            if !status.is_success() {
                let _ = sender.send(Inbound::Failed(SessionError::Status(status.as_u16())));
                return;
            }
            if initialize && let Some(session) = response.headers().get("mcp-session-id") {
                *lock(&shared.session_id) = Some(session.clone());
            }
            if status == reqwest::StatusCode::ACCEPTED {
                return;
            }
            pump(response, &sender).await;
        }));
    }

    /// Posts a notification or an answer, which the server acknowledges
    /// without a body.
    pub(super) async fn post_notification(&mut self, message: &Value) -> Result<(), SessionError> {
        let initialized =
            message.get("method").and_then(Value::as_str) == Some("notifications/initialized");
        let response = self
            .client
            .post(self.url.clone())
            .headers(Self::prepared(&self.shared, &self.headers))
            .timeout(HTTP_TIMEOUT)
            .body(message.to_string())
            .send()
            .await
            .map_err(|error| SessionError::Transport(error.without_url().to_string()))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SessionError::Unauthorized);
        }
        if initialized {
            self.open_listening_stream().await;
        }
        Ok(())
    }

    /// SDK `handle_get_stream`: once a session the server named is
    /// initialized, a standing `GET` carries whatever the server sends outside a request's own answer.
    ///
    /// The SDK starts it before the session's next request reaches the
    /// transport, so the server meets the `GET` first. The wait for its answer
    /// to begin is what keeps that order here, bounded so a server that holds
    /// the stream's headers back delays nothing.
    async fn open_listening_stream(&mut self) {
        // The SDK opens it only on a session the server named: without an
        // `Mcp-Session-Id` it returns before sending anything.
        let has_session = lock(&self.shared.session_id).is_some();
        if !has_session {
            return;
        }
        let client = self.client.clone();
        let url = self.url.clone();
        let mut headers = Self::prepared(&self.shared, &self.headers);
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert("cache-control", HeaderValue::from_static("no-store"));
        let sender = self.sender.clone();
        let (opened, answered) = tokio::sync::oneshot::channel();
        self.tasks.push(tokio::spawn(async move {
            let response = client.get(url).headers(headers).send().await;
            let _ = opened.send(());
            let Ok(response) = response else {
                return;
            };
            if response.status().is_success() {
                pump(response, &sender).await;
            }
        }));
        let _ = tokio::time::timeout(LISTENING_STREAM_WAIT, answered).await;
    }

    pub(super) async fn close(self, terminate: bool) {
        for task in &self.tasks {
            task.abort();
        }
        let has_session = lock(&self.shared.session_id).is_some();
        if terminate && has_session {
            let _ = self
                .client
                .delete(self.url.clone())
                .headers(Self::prepared(&self.shared, &self.headers))
                .timeout(HTTP_TIMEOUT)
                .send()
                .await;
        }
    }
}

impl Drop for HttpChannel {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Forwards the JSON-RPC messages of one HTTP answer, a JSON body or an event
/// stream, into the session's queue.
async fn pump(mut response: reqwest::Response, sender: &mpsc::UnboundedSender<Inbound>) {
    let event_stream = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    if !event_stream {
        let Ok(body) = response.text().await else {
            let _ = sender.send(Inbound::Failed(SessionError::Transport(
                "the HTTP answer could not be read".to_owned(),
            )));
            return;
        };
        match serde_json::from_str::<Value>(&body) {
            Ok(value) => {
                let _ = sender.send(Inbound::Message(value));
            }
            Err(error) => {
                let _ = sender.send(Inbound::Failed(protocol(format!(
                    "the HTTP answer is not JSON-RPC: {error}"
                ))));
            }
        }
        return;
    }
    let mut buffer = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        buffer.extend_from_slice(&chunk);
        while let Some(event) = take_event(&mut buffer) {
            if let Some(data) = event_data(&event)
                && let Ok(value) = serde_json::from_str::<Value>(&data)
                && sender.send(Inbound::Message(value)).is_err()
            {
                return;
            }
        }
    }
}

/// Splits the next complete event off an event-stream buffer.
pub(super) fn take_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let (position, separator) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => (lf, 2),
        (Some(_), Some(crlf)) | (None, Some(crlf)) => (crlf, 4),
        (Some(lf), None) => (lf, 2),
        (None, None) => return None,
    };
    let event = buffer.drain(..position).collect();
    buffer.drain(..separator);
    Some(event)
}

/// The data of a `message` event, which is the only kind carrying JSON-RPC.
pub(super) fn event_data(event: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event).ok()?;
    let mut kind = "message";
    let mut data = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            kind = value.trim();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (kind == "message" && !data.is_empty()).then(|| data.join("\n"))
}
