//! One MCP client session over either transport.
//!
//! Reference `mcp/tools.py` drives the Python MCP SDK's `ClientSession`, and
//! what that session sends is what a server sees from the reference: request
//! ids counting from zero, the `2025-11-25` handshake with the SDK's default
//! client identity, an `initialized` notification and a `tools/list` without
//! params, and after a successful `tools/call` one `tools/list` per session to
//! learn the output schemas it validates structured results against
//! (`ClientSession._validate_tool_result`, MCP SDK 1.28). The same session
//! answers the requests a server makes of it: sampling when the entry enables
//! it, and the SDK's default refusals otherwise.
//!
//! A session is used by one operation at a time: a stdio server's pooled
//! connection serializes its calls as reference `_StdioConnection` does, and an
//! HTTP session lives for exactly one discovery or one call. That is what lets
//! a request read its answer straight off the inbound queue, discarding the
//! stale answer a timed-out request left behind the way the SDK does.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use url::Url;

use super::{RemoteTool, SamplingHandler, sampling_answer};

mod http;
mod stdio;

#[cfg(test)]
mod session_tests;

use http::HttpChannel;
use stdio::StdioChannel;

/// The protocol revision the handshake asks for, SDK `LATEST_PROTOCOL_VERSION`.
pub(crate) const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// Every revision the SDK accepts back, `SUPPORTED_PROTOCOL_VERSIONS`.
pub(crate) const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    LATEST_PROTOCOL_VERSION,
];

/// SDK `DEFAULT_CLIENT_INFO`, which reference `ClientSession` construction
/// never overrides.
const CLIENT_NAME: &str = "mcp";
const CLIENT_VERSION: &str = "0.1.0";

/// JSON-RPC codes the SDK answers a server's request with.
const INVALID_REQUEST: i64 = -32_600;
const INVALID_PARAMS: i64 = -32_602;

/// Why a session operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionError {
    /// The server answered the request with a JSON-RPC error.
    Rpc { code: i64, message: String },
    /// No answer arrived within the request's timeout.
    Timeout(Duration),
    /// The HTTP endpoint answered 401, which is an authorization rejection.
    Unauthorized,
    /// The HTTP endpoint answered with another failing status.
    Status(u16),
    /// The server went away: a stdio process ended or its pipes closed.
    Closed,
    /// The exchange itself failed before the server could answer.
    Transport(String),
    /// The server answered with something the protocol does not allow.
    Protocol(String),
}

impl SessionError {
    /// Whether the session still ends with its termination request.
    ///
    /// The SDK sends the HTTP `DELETE` from the `finally` of its transport
    /// context, which a failure inside the session reaches but a transport
    /// failure that cancels the task group does not: a JSON-RPC error, a
    /// rejected result or a timeout is followed by the `DELETE`, a failing
    /// HTTP status or a broken connection is not.
    const fn terminates_session(&self) -> bool {
        matches!(
            self,
            Self::Rpc { .. } | Self::Protocol(_) | Self::Timeout(_)
        )
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { code, message } => {
                write!(
                    formatter,
                    "the server answered with error {code}: {message}"
                )
            }
            Self::Timeout(timeout) => write!(
                formatter,
                "no answer arrived within {} seconds",
                crate::pycompat::repr_float(timeout.as_secs_f64())
            ),
            Self::Unauthorized => {
                formatter.write_str("the server refused the credentials (HTTP 401)")
            }
            Self::Status(status) => write!(formatter, "the server answered HTTP {status}"),
            Self::Closed => formatter.write_str("the server closed the connection"),
            Self::Transport(message) | Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

/// Where a session connects.
pub(crate) enum Endpoint<'a> {
    Stdio {
        program: &'a str,
        arguments: &'a [String],
        environment: &'a BTreeMap<String, String>,
        working_directory: Option<&'a Path>,
    },
    Http {
        url: &'a Url,
        headers: &'a BTreeMap<String, String>,
    },
}

/// One message a transport received.
enum Inbound {
    Message(Value),
    Failed(SessionError),
    Closed,
}

pub(crate) struct Session {
    channel: Channel,
    inbound: mpsc::UnboundedReceiver<Inbound>,
    next_id: u64,
    /// SDK `read_timeout_seconds`: the default every request waits, which
    /// reference `mcp/tools.py` sets to the entry's startup timeout.
    timeout: Option<Duration>,
    sampling: Option<Arc<dyn SamplingHandler>>,
    /// SDK `_tool_output_schemas`, filled by every `tools/list`.
    output_schemas: BTreeMap<String, Option<Value>>,
    dead: bool,
}

impl Session {
    /// Connects and completes the handshake.
    ///
    /// `sampling` is what makes the client advertise the capability, as the
    /// SDK derives it from the callback it was given.
    pub(crate) async fn open(
        endpoint: Endpoint<'_>,
        timeout: Option<Duration>,
        sampling: Option<Arc<dyn SamplingHandler>>,
    ) -> Result<Self, SessionError> {
        let (sender, inbound) = mpsc::unbounded_channel();
        let channel = match endpoint {
            Endpoint::Stdio {
                program,
                arguments,
                environment,
                working_directory,
            } => Channel::Stdio(StdioChannel::spawn(
                program,
                arguments,
                environment,
                working_directory,
                sender,
            )?),
            Endpoint::Http { url, headers } => {
                Channel::Http(HttpChannel::new(url, headers, sender)?)
            }
        };
        let mut session = Self {
            channel,
            inbound,
            next_id: 0,
            timeout,
            sampling,
            output_schemas: BTreeMap::new(),
            dead: false,
        };
        match session.initialize().await {
            Ok(()) => Ok(session),
            Err(error) => {
                let terminate = error.terminates_session();
                session.close(terminate).await;
                Err(error)
            }
        }
    }

    /// Whether the transport went away, so the next call needs a new session.
    pub(crate) const fn is_dead(&self) -> bool {
        self.dead
    }

    async fn initialize(&mut self) -> Result<(), SessionError> {
        let capabilities = if self.sampling.is_some() {
            json!({"sampling": {}})
        } else {
            json!({})
        };
        let result = self
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": capabilities,
                    "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
                })),
                self.timeout,
            )
            .await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("the initialize answer names no protocol version"))?;
        // The transport adopts whatever revision the answer names before the
        // session checks it, so the termination of a refused handshake still
        // carries it, as SDK `_maybe_extract_protocol_version_from_message`.
        self.channel.negotiated(version);
        if !result.get("capabilities").is_some_and(Value::is_object)
            || !result
                .pointer("/serverInfo/name")
                .is_some_and(Value::is_string)
            || !result
                .pointer("/serverInfo/version")
                .is_some_and(Value::is_string)
        {
            return Err(protocol(
                "the initialize answer lacks the server capabilities or identity",
            ));
        }
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return Err(protocol(format!(
                "the server speaks protocol revision {version}, which this client does not"
            )));
        }
        self.channel
            .notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await
    }

    /// `tools/list` without a cursor: the first page is the whole discovery,
    /// as reference `list_tools_stdio` and `list_tools_http` read it.
    pub(crate) async fn list_tools(&mut self) -> Result<Vec<RemoteTool>, SessionError> {
        let result = self.request("tools/list", None, self.timeout).await?;
        let tools = parse_tools(&result)?;
        for tool in &tools {
            self.output_schemas
                .insert(tool.name.clone(), tool.output_schema.clone());
        }
        Ok(tools)
    }

    /// One page of `tools/list`, from `cursor` or from the start, with the
    /// cursor of the next page when the server announced one.
    pub(crate) async fn list_tools_page(
        &mut self,
        cursor: Option<&str>,
    ) -> Result<(Vec<RemoteTool>, Option<String>), SessionError> {
        let params = cursor.map(|cursor| json!({"cursor": cursor}));
        let result = self.request("tools/list", params, self.timeout).await?;
        let tools = parse_tools(&result)?;
        let next = match result.get("nextCursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) => Some(cursor.clone()),
            Some(_) => return Err(protocol("the tools/list answer carries an invalid cursor")),
        };
        Ok((tools, next))
    }

    /// `tools/call`, validated the way SDK `ClientSession.call_tool` does.
    pub(crate) async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, SessionError> {
        let result = self
            .request(
                "tools/call",
                Some(json!({"name": name, "arguments": arguments})),
                timeout,
            )
            .await?;
        validate_call_result(&result)?;
        if result.get("isError").and_then(Value::as_bool) != Some(true) {
            if !self.output_schemas.contains_key(name) {
                self.list_tools().await?;
            }
            if let Some(Some(schema)) = self.output_schemas.get(name) {
                let structured = result
                    .get("structuredContent")
                    .filter(|value| !value.is_null())
                    .ok_or_else(|| {
                        protocol(format!(
                            "tool {name} declares an output schema but returned no structured content"
                        ))
                    })?;
                crate::tools::validate_arguments(structured, schema).map_err(|violations| {
                    protocol(format!(
                        "tool {name} returned structured content its output schema rejects: {}",
                        violations
                            .first()
                            .map_or_else(String::new, |violation| violation.message.clone())
                    ))
                })?;
            }
        }
        Ok(result)
    }

    /// Ends the session: a stdio server gets its input closed and time to
    /// exit, an HTTP session its `DELETE` when `terminate` says the SDK would
    /// still have sent one.
    pub(crate) async fn close(self, terminate: bool) {
        self.channel.close(terminate).await;
    }

    /// Whether this failure still lets the HTTP session end with its `DELETE`.
    pub(crate) const fn terminates(error: &SessionError) -> bool {
        error.terminates_session()
    }

    async fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, SessionError> {
        if self.dead {
            return Err(SessionError::Closed);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut message = Map::new();
        message.insert("jsonrpc".to_owned(), json!("2.0"));
        message.insert("id".to_owned(), json!(id));
        message.insert("method".to_owned(), json!(method));
        if let Some(params) = params {
            message.insert("params".to_owned(), params);
        }
        if let Err(error) = self
            .channel
            .request(&Value::Object(message), method == "initialize")
            .await
        {
            if error == SessionError::Closed {
                self.dead = true;
            }
            return Err(error);
        }
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let next = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, self.inbound.recv())
                    .await
                    .map_err(|_| SessionError::Timeout(timeout.unwrap_or_default()))?,
                None => self.inbound.recv().await,
            };
            let value = match next {
                None | Some(Inbound::Closed) => {
                    self.dead = true;
                    return Err(SessionError::Closed);
                }
                Some(Inbound::Failed(error)) => return Err(error),
                Some(Inbound::Message(value)) => value,
            };
            if let Some(inbound_method) = value.get("method").and_then(Value::as_str) {
                if value.get("id").is_some() {
                    let answer = self.answer(inbound_method, &value).await;
                    if let Err(error) = self.channel.notify(&answer).await {
                        if error == SessionError::Closed {
                            self.dead = true;
                        }
                        return Err(error);
                    }
                }
                continue;
            }
            // An answer to another id is the late answer to a request that
            // already timed out, which the SDK drops.
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(SessionError::Rpc {
                    code: error
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// The answer this client owes a request the server made of it, SDK
    /// `ClientSession._received_request` with reference `mcp/tools.py`'s
    /// callbacks: sampling when the entry enables it, and otherwise the
    /// refusals the SDK's default callbacks give.
    async fn answer(&self, method: &str, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let refuse = |code: i64, message: &str, data: Option<&str>| {
            let mut error = json!({"code": code, "message": message});
            if let Some(data) = data {
                error["data"] = json!(data);
            }
            json!({"jsonrpc": "2.0", "id": id, "error": error})
        };
        match method {
            "sampling/createMessage" => match &self.sampling {
                Some(handler) => sampling_answer(handler, request).await,
                None => refuse(INVALID_REQUEST, "Sampling not supported", None),
            },
            "roots/list" => refuse(INVALID_REQUEST, "List roots not supported", None),
            "elicitation/create" => refuse(INVALID_REQUEST, "Elicitation not supported", None),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            _ => refuse(INVALID_PARAMS, "Invalid request parameters", Some("")),
        }
    }
}

fn protocol(message: impl Into<String>) -> SessionError {
    SessionError::Protocol(message.into())
}

/// SDK `ListToolsResult` validation followed by reference `RemoteTool`'s: one
/// malformed tool fails the whole list.
fn parse_tools(result: &Value) -> Result<Vec<RemoteTool>, SessionError> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol("the tools/list answer carries no tool list"))?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let invalid = |field: &str| {
                protocol(format!(
                    "tool {index} of the tools/list answer has an invalid {field}"
                ))
            };
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| invalid("name"))?;
            let input_schema = tool
                .get("inputSchema")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| invalid("inputSchema"))?;
            let description = match tool.get("description") {
                None | Some(Value::Null) => None,
                Some(Value::String(description)) => Some(description.clone()),
                Some(_) => return Err(invalid("description")),
            };
            let output_schema = match tool.get("outputSchema") {
                None | Some(Value::Null) => None,
                Some(schema @ Value::Object(_)) => Some(schema.clone()),
                Some(_) => return Err(invalid("outputSchema")),
            };
            Ok(RemoteTool {
                name: name.to_owned(),
                description,
                input_schema: input_schema.clone(),
                output_schema,
                annotations: tool.get("annotations").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// SDK `CallToolResult` validation: a content list of known blocks, an object
/// or nothing as structured content, and a boolean error flag.
fn validate_call_result(result: &Value) -> Result<(), SessionError> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol("the tools/call answer carries no content list"))?;
    for block in content {
        let string = |field: &str| block.get(field).is_some_and(Value::is_string);
        let valid = match block.get("type").and_then(Value::as_str) {
            Some("text") => string("text"),
            Some("image" | "audio") => string("data") && string("mimeType"),
            Some("resource_link") => string("uri") && string("name"),
            Some("resource") => block.get("resource").is_some_and(Value::is_object),
            _ => false,
        };
        if !valid {
            return Err(protocol(
                "the tools/call answer carries a malformed content block",
            ));
        }
    }
    if result
        .get("structuredContent")
        .is_some_and(|structured| !structured.is_null() && !structured.is_object())
    {
        return Err(protocol(
            "the tools/call answer carries structured content that is not an object",
        ));
    }
    if result
        .get("isError")
        .is_some_and(|flag| !flag.is_null() && !flag.is_boolean())
    {
        return Err(protocol(
            "the tools/call answer carries a non-boolean error flag",
        ));
    }
    Ok(())
}

enum Channel {
    Stdio(StdioChannel),
    Http(HttpChannel),
}

impl Channel {
    async fn request(&mut self, message: &Value, initialize: bool) -> Result<(), SessionError> {
        match self {
            Self::Stdio(channel) => channel.write(message).await,
            Self::Http(channel) => {
                channel.post_request(message, initialize);
                Ok(())
            }
        }
    }

    async fn notify(&mut self, message: &Value) -> Result<(), SessionError> {
        match self {
            Self::Stdio(channel) => channel.write(message).await,
            Self::Http(channel) => channel.post_notification(message).await,
        }
    }

    fn negotiated(&mut self, version: &str) {
        if let Self::Http(channel) = self {
            channel.negotiated(version);
        }
    }

    async fn close(self, terminate: bool) {
        match self {
            Self::Stdio(channel) => channel.close().await,
            Self::Http(channel) => channel.close(terminate).await,
        }
    }
}
