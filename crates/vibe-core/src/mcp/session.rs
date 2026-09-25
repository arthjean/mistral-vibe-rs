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
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use url::Url;

use super::{RemoteTool, SamplingHandler, sampling_answer};
use crate::child::{ChildGroup, Rung};

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

/// Reference `create_vibe_mcp_http_client` names the client unless the entry
/// declares its own user agent. The product token is the reference's; the
/// version is this build's, as `telemetry_user_agent` reports it.
const USER_AGENT_PRODUCT: &str = "MistralAI-VibeCLI";

/// SDK `PROCESS_TERMINATION_TIMEOUT`: how long a stdio server has to exit once
/// its input closes before it is terminated.
const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);
/// SDK `create_mcp_http_client` and reference `_MCP_DEFAULT_TIMEOUT`: the
/// connect and write budget of every HTTP exchange.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Reference `_MCP_DEFAULT_SSE_READ_TIMEOUT`: how long one read of an answer
/// may wait, which bounds the silence of a streamed answer.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// SDK `DEFAULT_INHERITED_ENV_VARS`: the only variables a stdio server inherits
/// from this process, beside the ones its entry declares.
#[cfg(not(windows))]
const INHERITED_ENVIRONMENT: &[&str] = &["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];
#[cfg(windows)]
const INHERITED_ENVIRONMENT: &[&str] = &[
    "APPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "USERNAME",
    "USERPROFILE",
];

/// How long the handshake waits for the standing `GET` to be answered.
const LISTENING_STREAM_WAIT: Duration = Duration::from_secs(1);

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
                super::render::python_float(timeout.as_secs_f64())
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

/// One message a transport received, with the text it arrived as.
///
/// The text is kept because a structured result is rendered with its keys in
/// the order the server sent them, which a parsed [`Value`] no longer knows.
enum Inbound {
    Message { value: Value, raw: String },
    Failed(SessionError),
    Closed,
}

/// A `tools/call` answer that passed the SDK's validation.
pub(crate) struct CallAnswer {
    /// The `CallToolResult` object.
    pub(crate) result: Value,
    /// The JSON-RPC message it arrived in, as text.
    pub(crate) raw: String,
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
        let (result, _) = self
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
        let (result, _) = self.request("tools/list", None, self.timeout).await?;
        let tools = parse_tools(&result)?;
        for tool in &tools {
            self.output_schemas
                .insert(tool.name.clone(), tool.output_schema.clone());
        }
        Ok(tools)
    }

    /// `tools/call`, validated the way SDK `ClientSession.call_tool` does.
    pub(crate) async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Option<Duration>,
    ) -> Result<CallAnswer, SessionError> {
        let (result, raw) = self
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
        Ok(CallAnswer { result, raw })
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
    ) -> Result<(Value, String), SessionError> {
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
            let (value, raw) = match next {
                None | Some(Inbound::Closed) => {
                    self.dead = true;
                    return Err(SessionError::Closed);
                }
                Some(Inbound::Failed(error)) => return Err(error),
                Some(Inbound::Message { value, raw }) => (value, raw),
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
            return Ok((value.get("result").cloned().unwrap_or(Value::Null), raw));
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

struct StdioChannel {
    child: ChildGroup,
    stdin: Option<ChildStdin>,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

impl StdioChannel {
    fn spawn(
        program: &str,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        working_directory: Option<&Path>,
        sender: mpsc::UnboundedSender<Inbound>,
    ) -> Result<Self, SessionError> {
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env_clear()
            .envs(inherited_environment())
            .envs(environment);
        if let Some(working_directory) = working_directory {
            command.current_dir(working_directory);
        }
        let (child, pipes) = ChildGroup::spawn(&mut command).map_err(|error| {
            SessionError::Transport(format!("cannot launch `{program}`: {error}"))
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (pipes.stdin, pipes.stdout, pipes.stderr)
        else {
            return Err(SessionError::Transport(
                "the stdio server started without its pipes".to_owned(),
            ));
        };
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // SDK `stdio_client` hands a line that is not JSON-RPC to the
                // session as an exception, which the client session ignores.
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if sender.send(Inbound::Message { value, raw: line }).is_err() {
                    return;
                }
            }
            let _ = sender.send(Inbound::Closed);
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            reader,
            stderr: tokio::spawn(drain(stderr)),
        })
    }

    async fn write(&mut self, message: &Value) -> Result<(), SessionError> {
        let stdin = self.stdin.as_mut().ok_or(SessionError::Closed)?;
        let mut line = serde_json::to_vec(message)
            .map_err(|error| SessionError::Transport(error.to_string()))?;
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|_| SessionError::Closed)?;
        stdin.flush().await.map_err(|_| SessionError::Closed)
    }

    async fn close(mut self) {
        // Closing input is the protocol's shutdown signal; SDK `stdio_client`
        // then waits for the exit and terminates the tree only after its
        // grace period.
        self.stdin.take();
        let _ = self
            .child
            .shut_down(PROCESS_TERMINATION_TIMEOUT, Rung::Wait)
            .await;
        let _ = self
            .child
            .reap_group(PROCESS_TERMINATION_TIMEOUT, false)
            .await;
        self.reader.abort();
        if tokio::time::timeout(PROCESS_TERMINATION_TIMEOUT, &mut self.stderr)
            .await
            .is_err()
        {
            self.stderr.abort();
        }
    }
}

impl Drop for StdioChannel {
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr.abort();
    }
}

fn inherited_environment() -> Vec<(&'static str, String)> {
    INHERITED_ENVIRONMENT
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.starts_with("()"))
                .map(|value| (*name, value))
        })
        .collect()
}

async fn drain(mut stderr: ChildStderr) {
    let mut sink = tokio::io::sink();
    let _ = tokio::io::copy(&mut stderr, &mut sink).await;
}

/// What every exchange of one HTTP session shares.
struct HttpShared {
    session_id: StdMutex<Option<HeaderValue>>,
    protocol_version: StdMutex<Option<HeaderValue>>,
}

struct HttpChannel {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    shared: Arc<HttpShared>,
    sender: mpsc::UnboundedSender<Inbound>,
    tasks: Vec<JoinHandle<()>>,
}

impl HttpChannel {
    fn new(
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

    fn negotiated(&mut self, version: &str) {
        if let (Ok(value), Ok(mut slot)) = (
            HeaderValue::from_str(version),
            self.shared.protocol_version.lock(),
        ) {
            *slot = Some(value);
        }
    }

    /// SDK `StreamableHTTPTransport._prepare_headers`: the base headers, the
    /// session id once the server assigned one, and the negotiated revision
    /// once the handshake settled it.
    fn prepared(shared: &HttpShared, base: &HeaderMap) -> HeaderMap {
        let mut headers = base.clone();
        if let Ok(session) = shared.session_id.lock()
            && let Some(session) = session.as_ref()
        {
            headers.insert("mcp-session-id", session.clone());
        }
        if let Ok(version) = shared.protocol_version.lock()
            && let Some(version) = version.as_ref()
        {
            headers.insert("mcp-protocol-version", version.clone());
        }
        headers
    }

    /// Posts a request and pumps whatever answers it into the session's
    /// queue, from a task of its own so that a request the server makes
    /// mid-stream is answered while the stream stays open.
    fn post_request(&mut self, message: &Value, initialize: bool) {
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
            if initialize
                && let Some(session) = response.headers().get("mcp-session-id")
                && let Ok(mut slot) = shared.session_id.lock()
            {
                *slot = Some(session.clone());
            }
            if status == reqwest::StatusCode::ACCEPTED {
                return;
            }
            pump(response, &sender).await;
        }));
    }

    /// Posts a notification or an answer, which the server acknowledges
    /// without a body.
    async fn post_notification(&mut self, message: &Value) -> Result<(), SessionError> {
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
        let has_session = self
            .shared
            .session_id
            .lock()
            .is_ok_and(|session| session.is_some());
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

    async fn close(self, terminate: bool) {
        for task in &self.tasks {
            task.abort();
        }
        let has_session = self
            .shared
            .session_id
            .lock()
            .is_ok_and(|session| session.is_some());
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
                let _ = sender.send(Inbound::Message { value, raw: body });
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
                && sender.send(Inbound::Message { value, raw: data }).is_err()
            {
                return;
            }
        }
    }
}

/// Splits the next complete event off an event-stream buffer.
fn take_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
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
fn event_data(event: &[u8]) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_stream_yields_its_message_data_and_skips_other_events() {
        let mut buffer =
            b"event: message\r\ndata: {\"id\":0}\r\n\r\n: keepalive\n\nevent: ping\ndata: x\n\n"
                .to_vec();
        let first = take_event(&mut buffer).expect("first event");
        assert_eq!(event_data(&first).as_deref(), Some("{\"id\":0}"));
        let comment = take_event(&mut buffer).expect("comment");
        assert_eq!(event_data(&comment), None);
        let ping = take_event(&mut buffer).expect("ping");
        assert_eq!(event_data(&ping), None);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_tool_list_with_one_malformed_tool_fails_whole() {
        let error = parse_tools(&json!({"tools": [
            {"name": "echo", "inputSchema": {"type": "object"}},
            {"name": "schemaless"},
        ]}))
        .expect_err("the list is refused");
        assert!(matches!(error, SessionError::Protocol(_)));
        let tools = parse_tools(&json!({"tools": [
            {"name": "echo", "description": null, "inputSchema": {"type": "object"}},
        ]}))
        .expect("a null description is absent");
        assert_eq!(tools[0].description, None);
    }
}
