use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

use super::{
    MAX_MCP_DISCOVERY_PAGES, MAX_MCP_TOOLS_PER_SERVER, MCP_PROTOCOL_VERSION, McpError, McpFuture,
    McpPeer, McpPeerFactory, McpServerConfig, McpTransportConfig, RemoteTool, decode_tool_result,
    validate_config,
};
use crate::tools::{ToolExecutionOutput, ToolOutputSink};

const SESSION_HEADER: &str = "mcp-session-id";
const SESSION_TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Default)]
pub struct HttpMcpPeerFactory;

impl McpPeerFactory for HttpMcpPeerFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        Box::pin(async move {
            let peer = HttpMcpPeer::connect(config).await?;
            Ok(Arc::new(peer) as Arc<dyn McpPeer>)
        })
    }
}

struct HttpMcpPeer {
    client: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
    state: Mutex<HttpState>,
}

struct HttpState {
    next_request_id: u64,
    session_id: Option<HeaderValue>,
    closed: bool,
}

impl HttpMcpPeer {
    async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        validate_config(config)?;
        let (endpoint, configured_headers) = match &config.transport {
            McpTransportConfig::StreamableHttp { url, headers } => (url.clone(), headers),
            McpTransportConfig::Stdio { .. } => {
                return Err(McpError::Transport(
                    "the HTTP MCP peer requires an HTTP transport".to_owned(),
                ));
            }
        };
        let mut headers = HeaderMap::new();
        for (name, value) in configured_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpError::InvalidConfig("invalid HTTP header name".to_owned()))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| McpError::InvalidConfig("invalid HTTP header value".to_owned()))?;
            headers.insert(name, value);
        }
        let peer = Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| McpError::Transport(error.to_string()))?,
            endpoint,
            headers,
            state: Mutex::new(HttpState {
                next_request_id: 1,
                session_id: None,
                closed: false,
            }),
        };
        peer.initialize().await?;
        Ok(peer)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "mistral-vibe-rs",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                super::MAX_MCP_DISCOVERY_BYTES,
                None,
            )
            .await?;
        let version = response
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                McpError::Transport("initialize response omitted protocolVersion".to_owned())
            })?;
        if version != MCP_PROTOCOL_VERSION {
            return Err(McpError::Transport(format!(
                "unsupported negotiated MCP protocol version `{version}`"
            )));
        }
        if !response
            .pointer("/capabilities/tools")
            .is_some_and(Value::is_object)
        {
            return Err(McpError::Transport(
                "MCP server did not declare the tools capability".to_owned(),
            ));
        }
        self.notify("notifications/initialized", json!({})).await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        max_response_bytes: usize,
        output: Option<&ToolOutputSink>,
    ) -> Result<Value, McpError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(McpError::Transport("HTTP peer is closed".to_owned()));
        }
        let request_id = state.next_request_id;
        state.next_request_id = state.next_request_id.saturating_add(1);
        let messages = self
            .post(
                &mut state,
                json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                }),
                max_response_bytes,
                Some(request_id),
                output,
            )
            .await?;
        for message in messages {
            validate_message(&message)?;
            if message.get("id").and_then(Value::as_u64) == Some(request_id) {
                return match (message.get("result"), message.get("error")) {
                    (Some(result), None) => Ok(result.clone()),
                    (None, Some(error)) => Err(McpError::Transport(format!(
                        "{method} failed: {}",
                        bounded_error(error)
                    ))),
                    _ => Err(McpError::Transport(format!(
                        "{method} response must contain exactly one of result or error"
                    ))),
                };
            }
            if message.get("method").and_then(Value::as_str) == Some("notifications/progress")
                && let Some(chunk) = message.pointer("/params/message").and_then(Value::as_str)
                && let Some(output) = output
            {
                output
                    .emit(chunk)
                    .map_err(|error| McpError::Tool(error.to_string()))?;
            }
        }
        Err(McpError::Transport(format!(
            "{method} response omitted request ID {request_id}"
        )))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(McpError::Transport("HTTP peer is closed".to_owned()));
        }
        self.post(
            &mut state,
            json!({"jsonrpc": "2.0", "method": method, "params": params}),
            super::MAX_MCP_DISCOVERY_BYTES,
            None,
            None,
        )
        .await?;
        Ok(())
    }

    async fn post(
        &self,
        state: &mut HttpState,
        body: Value,
        max_response_bytes: usize,
        expected_response_id: Option<u64>,
        output: Option<&ToolOutputSink>,
    ) -> Result<Vec<Value>, McpError> {
        let response_required = expected_response_id.is_some();
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .json(&body);
        if let Some(session_id) = &state.session_id {
            request = request.header(SESSION_HEADER, session_id.clone());
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::AuthRequired);
        }
        if !response.status().is_success() {
            return Err(McpError::Transport(format!(
                "HTTP MCP endpoint returned {}",
                response.status()
            )));
        }
        if let Some(session_id) = response.headers().get(SESSION_HEADER) {
            state.session_id = Some(session_id.clone());
        }
        if response.status() == reqwest::StatusCode::ACCEPTED && !response_required {
            return Ok(Vec::new());
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_response_bytes as u64)
        {
            return Err(McpError::Transport(
                "HTTP MCP response exceeded its byte budget".to_owned(),
            ));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            if let Some(request_id) = expected_response_id {
                return read_event_stream(&mut response, max_response_bytes, request_id, output)
                    .await;
            }
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?
        {
            if bytes.len().saturating_add(chunk.len()) > max_response_bytes {
                return Err(McpError::Transport(
                    "HTTP MCP response exceeded its byte budget".to_owned(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() && !response_required {
            return Ok(Vec::new());
        }
        let message = serde_json::from_slice(&bytes)
            .map_err(|error| McpError::Transport(format!("invalid JSON response: {error}")))?;
        Ok(vec![message])
    }

    async fn list_tools(&self, max_response_bytes: usize) -> Result<Vec<RemoteTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None::<String>;
        let mut seen = BTreeSet::new();
        let mut encoded_bytes = 0_usize;
        for _ in 0..MAX_MCP_DISCOVERY_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let response = self
                .request("tools/list", params, max_response_bytes, None)
                .await?;
            let page = response
                .get("tools")
                .cloned()
                .ok_or_else(|| McpError::Tool("tools/list omitted tools".to_owned()))?;
            encoded_bytes = encoded_bytes.saturating_add(
                serde_json::to_vec(&page)
                    .map_err(|error| McpError::Tool(error.to_string()))?
                    .len(),
            );
            if encoded_bytes > max_response_bytes {
                return Err(McpError::Tool(
                    "MCP discovery response exceeded its byte budget".to_owned(),
                ));
            }
            let mut page = serde_json::from_value::<Vec<RemoteTool>>(page)
                .map_err(|error| McpError::Tool(format!("invalid tools/list response: {error}")))?;
            tools.append(&mut page);
            if tools.len() > MAX_MCP_TOOLS_PER_SERVER {
                return Err(McpError::Tool(format!(
                    "MCP discovery returned more than {MAX_MCP_TOOLS_PER_SERVER} tools"
                )));
            }
            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(next) = cursor.as_ref() else {
                return Ok(tools);
            };
            if !seen.insert(next.clone()) {
                return Err(McpError::Tool(
                    "MCP discovery repeated a pagination cursor".to_owned(),
                ));
            }
        }
        Err(McpError::Tool(format!(
            "MCP discovery exceeded {MAX_MCP_DISCOVERY_PAGES} pages"
        )))
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        max_response_bytes: usize,
        output: &ToolOutputSink,
    ) -> Result<ToolExecutionOutput, McpError> {
        let result = self
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                max_response_bytes,
                Some(output),
            )
            .await?;
        decode_tool_result(result)
    }
}

impl McpPeer for HttpMcpPeer {
    fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<RemoteTool>> {
        Box::pin(self.list_tools(max_response_bytes))
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        max_response_bytes: usize,
        output: ToolOutputSink,
    ) -> McpFuture<'a, ToolExecutionOutput> {
        Box::pin(async move {
            self.call_tool(name, arguments, max_response_bytes, &output)
                .await
        })
    }

    fn close<'a>(&'a self) -> McpFuture<'a, ()> {
        Box::pin(async move {
            let session_id = {
                let mut state = self.state.lock().await;
                if state.closed {
                    return Ok(());
                }
                state.closed = true;
                state.session_id.take()
            };
            let Some(session_id) = session_id else {
                return Ok(());
            };
            let response = tokio::time::timeout(
                SESSION_TERMINATION_TIMEOUT,
                self.client
                    .delete(self.endpoint.clone())
                    .headers(self.headers.clone())
                    .header(SESSION_HEADER, session_id)
                    .send(),
            )
            .await
            .map_err(|_| McpError::Transport("HTTP MCP session termination timed out".to_owned()))?
            .map_err(|error| McpError::Transport(error.to_string()))?;
            if response.status().is_success()
                || matches!(
                    response.status(),
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
                )
            {
                Ok(())
            } else {
                Err(McpError::Transport(format!(
                    "HTTP MCP session termination returned {}",
                    response.status()
                )))
            }
        })
    }
}

fn validate_message(message: &Value) -> Result<(), McpError> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::Transport(
            "HTTP MCP response has an invalid JSON-RPC version".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn parse_event_stream(bytes: &[u8]) -> Result<Vec<Value>, McpError> {
    let mut buffer = bytes.to_vec();
    let mut messages = Vec::new();
    while let Some(event) = take_event(&mut buffer) {
        messages.extend(parse_event(&event)?);
    }
    if !buffer.is_empty() {
        messages.extend(parse_event(&buffer)?);
    }
    if messages.is_empty() {
        return Err(McpError::Transport(
            "MCP event stream contained no JSON-RPC message".to_owned(),
        ));
    }
    Ok(messages)
}

async fn read_event_stream(
    response: &mut reqwest::Response,
    max_response_bytes: usize,
    request_id: u64,
    output: Option<&ToolOutputSink>,
) -> Result<Vec<Value>, McpError> {
    let mut buffer = Vec::new();
    let mut messages = Vec::new();
    let mut received = 0_usize;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| McpError::Transport(error.to_string()))?
    {
        received = received.saturating_add(chunk.len());
        if received > max_response_bytes {
            return Err(McpError::Transport(
                "HTTP MCP response exceeded its byte budget".to_owned(),
            ));
        }
        buffer.extend_from_slice(&chunk);
        while let Some(event) = take_event(&mut buffer) {
            if record_event_messages(parse_event(&event)?, request_id, output, &mut messages)? {
                return Ok(messages);
            }
        }
    }
    if !buffer.is_empty() {
        record_event_messages(parse_event(&buffer)?, request_id, output, &mut messages)?;
    }
    Ok(messages)
}

fn record_event_messages(
    parsed: Vec<Value>,
    request_id: u64,
    output: Option<&ToolOutputSink>,
    messages: &mut Vec<Value>,
) -> Result<bool, McpError> {
    let mut complete = false;
    for message in parsed {
        validate_message(&message)?;
        if message.get("method").and_then(Value::as_str) == Some("notifications/progress")
            && let Some(chunk) = message.pointer("/params/message").and_then(Value::as_str)
            && let Some(output) = output
        {
            output
                .emit(chunk)
                .map_err(|error| McpError::Tool(error.to_string()))?;
            continue;
        }
        complete |= message.get("id").and_then(Value::as_u64) == Some(request_id);
        messages.push(message);
    }
    Ok(complete)
}

fn take_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let (position, separator_len) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => (lf, 2),
        (Some(_), Some(crlf)) => (crlf, 4),
        (Some(lf), None) => (lf, 2),
        (None, Some(crlf)) => (crlf, 4),
        (None, None) => return None,
    };
    let event = buffer.drain(..position).collect();
    buffer.drain(..separator_len);
    Some(event)
}

fn parse_event(event: &[u8]) -> Result<Vec<Value>, McpError> {
    let text = std::str::from_utf8(event)
        .map_err(|_| McpError::Transport("MCP event stream is not UTF-8".to_owned()))?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&data)
        .map(|message| vec![message])
        .map_err(|error| McpError::Transport(format!("invalid MCP event data: {error}")))
}

fn bounded_error(error: &Value) -> String {
    let encoded = serde_json::to_string(error).unwrap_or_else(|_| "unknown error".to_owned());
    crate::integrations::redact(crate::text::truncate_utf8(&encoded, 4_096))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn parses_sse_data_without_accepting_empty_streams() {
        let messages = parse_event_stream(
            b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\n",
        )
        .expect("valid event stream");
        assert_eq!(messages[0]["id"], 1);
        assert!(parse_event_stream(b": keepalive\n\n").is_err());
    }

    #[tokio::test]
    async fn streamable_http_returns_before_sse_eof_and_terminates_the_owned_session() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let endpoint = Url::parse(&format!(
            "http://{}/mcp",
            listener.local_addr().expect("listener address")
        ))
        .expect("fixture URL");
        let deleted = Arc::new(AtomicBool::new(false));
        let observed_delete = deleted.clone();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let (stream, _) = listener.accept().expect("fixture accepts");
                let observed_delete = observed_delete.clone();
                std::thread::spawn(move || serve_request(stream, &observed_delete));
            }
        });
        let config = McpServerConfig {
            alias: "fixture".to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: endpoint,
                headers: Default::default(),
            },
            enabled: true,
            disabled_tools: Default::default(),
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
        };

        let peer =
            tokio::time::timeout(Duration::from_secs(1), HttpMcpPeerFactory.connect(&config))
                .await
                .expect("initialize must not wait for SSE EOF")
                .expect("peer connects");
        let tools = tokio::time::timeout(Duration::from_secs(1), peer.discover(64 * 1024))
            .await
            .expect("discovery must not wait for SSE EOF")
            .expect("tools discover");
        assert_eq!(tools.len(), 1);
        peer.close().await.expect("session terminates");
        assert!(deleted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn streamable_http_forwards_progress_before_the_tool_result() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let endpoint = Url::parse(&format!(
            "http://{}/mcp",
            listener.local_addr().expect("listener address")
        ))
        .expect("fixture URL");
        let (release_sender, release_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut release_receiver = Some(release_receiver);
            for index in 0..3 {
                let (stream, _) = listener.accept().expect("fixture accepts");
                if index < 2 {
                    std::thread::spawn(move || serve_request(stream, &AtomicBool::new(false)));
                } else {
                    serve_gated_tool_call(
                        stream,
                        release_receiver.take().expect("single gated response"),
                    );
                }
            }
        });
        let config = McpServerConfig {
            alias: "progress".to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: endpoint,
                headers: Default::default(),
            },
            enabled: true,
            disabled_tools: Default::default(),
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
        };
        let peer = HttpMcpPeerFactory
            .connect(&config)
            .await
            .expect("peer connects");
        let (progress_sender, mut progress_receiver) = tokio::sync::mpsc::unbounded_channel();
        let sink = ToolOutputSink::test_streaming(
            Arc::new(move |chunk| {
                progress_sender
                    .send(chunk)
                    .map_err(|_| "progress receiver closed".to_owned())
            }),
            64 * 1024,
        );
        let call =
            tokio::spawn(async move { peer.call("search", json!({}), 64 * 1024, sink).await });

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), progress_receiver.recv())
                .await
                .expect("progress arrives before timeout"),
            Some("working".to_owned())
        );
        assert!(!call.is_finished(), "tool result must still be pending");
        release_sender.send(()).expect("release result");
        call.await
            .expect("tool task joins")
            .expect("tool result completes");
    }

    fn serve_gated_tool_call(mut stream: TcpStream, release: mpsc::Receiver<()>) {
        let request = read_request(&mut stream);
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let message: Value = serde_json::from_str(body).expect("JSON-RPC request");
        let id = message
            .get("id")
            .and_then(Value::as_u64)
            .expect("request ID");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nMcp-Session-Id: session-test\r\nConnection: keep-alive\r\n\r\n",
            )
            .expect("SSE headers");
        write_chunk(
            &mut stream,
            b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"message\":\"working\"}}\r\n\r\n",
        );
        stream.flush().expect("progress flushes");
        release.recv().expect("result release");
        let response = format!(
            "event: message\r\ndata: {}\r\n\r\n",
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": "done"}],
                    "structuredContent": {"ok": true},
                }
            })
        );
        write_chunk(&mut stream, response.as_bytes());
        stream.flush().expect("result flushes");
    }

    fn serve_request(mut stream: TcpStream, deleted: &AtomicBool) {
        let request = read_request(&mut stream);
        if request.starts_with("DELETE ") {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("mcp-session-id: session-test")
            );
            deleted.store(true, Ordering::SeqCst);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("delete response");
            return;
        }
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let message: Value = serde_json::from_str(body).expect("JSON-RPC request");
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if method == "notifications/initialized" {
            stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nMcp-Session-Id: session-test\r\nConnection: close\r\n\r\n",
                )
                .expect("notification response");
            return;
        }
        let id = message
            .get("id")
            .and_then(Value::as_u64)
            .expect("request ID");
        let result = if method == "initialize" {
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
            })
        } else {
            json!({
                "tools": [{
                    "name": "search",
                    "description": "fixture",
                    "inputSchema": {"type": "object"},
                }]
            })
        };
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nMcp-Session-Id: session-test\r\nConnection: keep-alive\r\n\r\n",
            )
            .expect("SSE headers");
        write_chunk(
            &mut stream,
            b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"message\":\"working\"}}\r\n\r\n",
        );
        let response = format!(
            "event: message\r\ndata: {}\r\n\r\n",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        );
        write_chunk(&mut stream, response.as_bytes());
        stream.flush().expect("SSE flushes");
        std::thread::sleep(Duration::from_secs(2));
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut expected = None;
        loop {
            let read = stream.read(&mut chunk).expect("request reads");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or_default();
                expected = Some(header_end + content_length);
            }
            if expected.is_some_and(|expected| bytes.len() >= expected) {
                break;
            }
        }
        String::from_utf8(bytes).expect("request UTF-8")
    }

    fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) {
        write!(stream, "{:X}\r\n", bytes.len()).expect("chunk size");
        stream.write_all(bytes).expect("chunk body");
        stream.write_all(b"\r\n").expect("chunk delimiter");
    }
}
