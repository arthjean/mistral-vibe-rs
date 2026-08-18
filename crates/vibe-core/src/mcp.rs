use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use url::Url;

use crate::child::{ChildGroup, Rung, TerminationError};
use crate::policy::{ApprovalAgent, PermissionContext, PermissionStore, PolicyGuardedTool};
use crate::remote_tools::{
    ProviderReach, public_tool_name, sanitize_mcp_name, set_all, tool_availability,
};
use crate::text::{canonical_url, is_secure_transport};

use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

mod http;

use http::HttpMcpPeer;
pub use http::HttpMcpPeerFactory;

const MAX_MCP_SERVERS: usize = 256;
const MAX_MCP_TOOLS_PER_SERVER: usize = 256;
const MAX_MCP_DISCOVERY_PAGES: usize = 256;
const MAX_MCP_DISCOVERY_BYTES: usize = 1_048_576;
const MCP_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_CLEANUP_GRACE: Duration = Duration::from_secs(2);
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
pub const DEFAULT_MCP_STARTUP_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_MCP_TOOL_TIMEOUT_MS: u64 = 60_000;
const MAX_MCP_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

mod auth;

pub use auth::{
    DEFAULT_MCP_API_KEY_FORMAT, DEFAULT_MCP_API_KEY_HEADER, DEFAULT_MCP_OAUTH_REDIRECT_PORT,
    MCP_TOKEN_PLACEHOLDER, McpAuthConfig, McpOAuthConfig, McpStaticAuth,
};

pub type McpFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, McpError>> + Send + 'a>>;

/// One message of a `sampling/createMessage` request, already reduced to what a
/// completion needs.
///
/// Reference `_map_sampling_messages` keeps the two roles it knows and treats
/// anything else as an assistant turn, and `_extract_text_content` joins the
/// text blocks and drops the rest, so the handler this port hands the request to
/// never sees a shape the engine cannot carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingMessage {
    pub role: SamplingRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingRole {
    System,
    User,
    Assistant,
}

/// What a server asks the client to complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingRequest {
    pub messages: Vec<SamplingMessage>,
    pub max_tokens: Option<u32>,
    /// The temperature in thousandths, matching [`crate::provider::RequestLimits`],
    /// so a fractional value survives a type that carries no floats.
    pub temperature_millis: Option<u16>,
}

/// What the client answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingResponse {
    pub text: String,
    pub model: String,
}

/// Serves the completions MCP servers ask this client for.
///
/// Reference `MCPSamplingHandler` reaches the active backend and the active
/// model through two getters rather than holding either, because both move
/// between turns. The same shape holds here: the implementation lives in the
/// layer that owns a provider, and `vibe-core` only declares what it must
/// answer.
pub trait SamplingHandler: Send + Sync {
    fn complete<'a>(&'a self, request: SamplingRequest) -> McpFuture<'a, SamplingResponse>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum McpTransportConfig {
    /// `transport = "http"`. The exchange is the same streamable HTTP one
    /// [`Self::StreamableHttp`] drives, as it is upstream; the variant exists so
    /// an entry round-trips under the transport name it was written with.
    Http {
        url: Url,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    StreamableHttp {
        url: Url,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        arguments: Vec<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        working_directory: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub alias: String,
    pub transport: McpTransportConfig,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub disabled_tools: BTreeSet<String>,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default = "default_tool_timeout_ms")]
    pub tool_timeout_ms: u64,
    /// How the HTTP transports authenticate. Declared per entry and resolved at
    /// connect time, so no resolved token is ever held in a persisted value.
    #[serde(default)]
    pub auth: McpAuthConfig,
    /// A usage hint appended to every tool this server publishes.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Whether the server may ask the client for model completions. This port
    /// advertises no sampling capability, so an inbound request is refused
    /// either way; the flag names which refusal the server gets.
    #[serde(default = "default_sampling_enabled")]
    pub sampling_enabled: bool,
}

const fn default_startup_timeout_ms() -> u64 {
    DEFAULT_MCP_STARTUP_TIMEOUT_MS
}

const fn default_tool_timeout_ms() -> u64 {
    DEFAULT_MCP_TOOL_TIMEOUT_MS
}

const fn default_sampling_enabled() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: Value,
}

/// A connected MCP server.
///
/// Implementations return decoded values: the byte budgets are enforced against
/// the wire payload inside the transport, which is the only place where the
/// encoded size is known.
pub trait McpPeer: Send + Sync {
    fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<RemoteTool>>;
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        max_response_bytes: usize,
        output: ToolOutputSink,
    ) -> McpFuture<'a, ToolExecutionOutput>;
    fn close<'a>(&'a self) -> McpFuture<'a, ()>;
}

pub trait McpPeerFactory: Send + Sync {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StdioMcpPeerFactory;

impl McpPeerFactory for StdioMcpPeerFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        Box::pin(async move {
            let peer = StdioMcpPeer::connect(config, None).await?;
            Ok(Arc::new(peer) as Arc<dyn McpPeer>)
        })
    }
}

#[derive(Clone, Default)]
pub struct DefaultMcpPeerFactory {
    sampling: Option<Arc<dyn SamplingHandler>>,
}

impl DefaultMcpPeerFactory {
    /// The same factory serving the completions its servers ask for.
    ///
    /// A handler is what turns `sampling_enabled` from a declaration into a
    /// capability: without one, a server that asks is refused whatever its
    /// entry says, because there is nothing behind the advertisement.
    #[must_use]
    pub fn with_sampling(handler: Arc<dyn SamplingHandler>) -> Self {
        Self {
            sampling: Some(handler),
        }
    }
}

impl McpPeerFactory for DefaultMcpPeerFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        match config.transport {
            McpTransportConfig::Stdio { .. } => {
                let sampling = self.sampling.clone();
                Box::pin(async move {
                    let peer = StdioMcpPeer::connect(config, sampling).await?;
                    Ok(Arc::new(peer) as Arc<dyn McpPeer>)
                })
            }
            McpTransportConfig::Http { .. } | McpTransportConfig::StreamableHttp { .. } => {
                let sampling = self.sampling.clone();
                Box::pin(async move {
                    let peer = HttpMcpPeer::connect(config, sampling).await?;
                    Ok(Arc::new(peer) as Arc<dyn McpPeer>)
                })
            }
        }
    }
}

struct StdioMcpPeer {
    state: Mutex<StdioMcpState>,
    /// Present only when the entry enables sampling and the host installed a
    /// handler, which is exactly when the capability was advertised.
    sampling: Option<Arc<dyn SamplingHandler>>,
}

struct StdioMcpState {
    child: ChildGroup,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: JoinHandle<()>,
    next_request_id: u64,
    closed: bool,
}

impl StdioMcpPeer {
    async fn connect(
        config: &McpServerConfig,
        sampling: Option<Arc<dyn SamplingHandler>>,
    ) -> Result<Self, McpError> {
        validate_config(config)?;
        let McpTransportConfig::Stdio {
            command,
            arguments,
            environment,
            working_directory,
        } = &config.transport
        else {
            return Err(McpError::Transport(
                "the built-in MCP peer supports stdio transports only".to_owned(),
            ));
        };
        let mut command_builder = Command::new(command);
        command_builder.args(arguments).envs(environment);
        if let Some(working_directory) = working_directory {
            command_builder.current_dir(working_directory);
        }
        let (child, pipes) = ChildGroup::spawn(&mut command_builder)
            .map_err(|error| McpError::Transport(format!("cannot launch `{command}`: {error}")))?;
        let stdin = pipes
            .stdin
            .ok_or_else(|| McpError::Transport("stdio stdin is missing".to_owned()))?;
        let stdout = pipes
            .stdout
            .ok_or_else(|| McpError::Transport("stdio stdout is missing".to_owned()))?;
        let stderr = pipes
            .stderr
            .ok_or_else(|| McpError::Transport("stdio stderr is missing".to_owned()))?;
        let stderr = tokio::spawn(drain_stderr(stderr));
        let peer = Self {
            state: Mutex::new(StdioMcpState {
                child,
                stdin: Some(stdin),
                stdout: BufReader::new(stdout),
                stderr,
                next_request_id: 1,
                closed: false,
            }),
            // Reference `mcp/tools.py` hands the session a sampling callback
            // only when the entry enables it, so an entry that disables it has
            // nothing to advertise and nothing to serve.
            sampling: sampling.filter(|_| config.sampling_enabled),
        };
        if let Err(error) = peer.initialize().await {
            let _ = peer.close_transport().await;
            return Err(error);
        }
        Ok(peer)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": client_capabilities(self.sampling.is_some()),
                    "clientInfo": {
                        "name": "mistral-vibe-rs",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                MAX_MCP_DISCOVERY_BYTES,
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
        mut params: Value,
        max_response_bytes: usize,
        progress: Option<&ToolOutputSink>,
    ) -> Result<Value, McpError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(McpError::Transport("stdio peer is closed".to_owned()));
        }
        let request_id = state.next_request_id;
        state.next_request_id = state.next_request_id.saturating_add(1);
        if progress.is_some() {
            let params = params
                .as_object_mut()
                .ok_or_else(|| McpError::Transport(format!("{method} params must be an object")))?;
            params.insert("_meta".to_owned(), json!({"progressToken": request_id}));
        }
        write_message(
            &mut state,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }),
        )
        .await?;
        loop {
            let message = read_message(&mut state, max_response_bytes).await?;
            validate_jsonrpc_message(&message)?;
            if message.get("id").and_then(Value::as_u64) == Some(request_id) {
                return match (message.get("result"), message.get("error")) {
                    (Some(result), None) => Ok(result.clone()),
                    (None, Some(error)) => Err(McpError::Transport(format!(
                        "{method} failed: {}",
                        bounded_rpc_error(error)
                    ))),
                    _ => Err(McpError::Transport(format!(
                        "{method} response must contain exactly one of result or error"
                    ))),
                };
            }
            if message.get("method").and_then(Value::as_str) == Some("notifications/progress") {
                if message
                    .pointer("/params/progressToken")
                    .and_then(Value::as_u64)
                    == Some(request_id)
                    && let Some(chunk) = message.pointer("/params/message").and_then(Value::as_str)
                    && let Some(progress) = progress
                {
                    progress
                        .emit(chunk)
                        .map_err(|error| McpError::Tool(error.to_string()))?;
                }
            } else if message.get("method").is_some() && message.get("id").is_some() {
                self.serve_inbound(&mut state, &message).await?;
            } else if message.get("method").is_none() {
                return Err(McpError::Transport(format!(
                    "{method} received an unexpected response ID"
                )));
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(McpError::Transport("stdio peer is closed".to_owned()));
        }
        write_message(
            &mut state,
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
        )
        .await
    }

    /// Answers a request the server made of this client.
    ///
    /// Only sampling is served, and only where the capability was advertised;
    /// everything else keeps the method-not-found answer the protocol expects
    /// from a client that declared nothing.
    async fn serve_inbound(
        &self,
        state: &mut StdioMcpState,
        request: &Value,
    ) -> Result<(), McpError> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(handler) = self
            .sampling
            .as_ref()
            .filter(|_| method == "sampling/createMessage")
        else {
            return reply_method_not_found(state, request).await;
        };
        let answer = sampling_answer(handler, request).await;
        write_message(state, &answer).await
    }

    async fn list_tools(&self, max_response_bytes: usize) -> Result<Vec<RemoteTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None::<String>;
        let mut seen = BTreeSet::new();
        let mut encoded_bytes = 0_usize;
        let mut pages = 0_usize;
        loop {
            if pages == MAX_MCP_DISCOVERY_PAGES {
                return Err(McpError::Tool(format!(
                    "MCP discovery exceeded {MAX_MCP_DISCOVERY_PAGES} pages"
                )));
            }
            pages = pages.saturating_add(1);
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
                break;
            };
            if !seen.insert(next.clone()) {
                return Err(McpError::Tool(
                    "MCP discovery repeated a pagination cursor".to_owned(),
                ));
            }
        }
        Ok(tools)
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

    async fn close_transport(&self) -> Result<(), McpError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        // Closing stdin is the protocol's shutdown signal, so a well-behaved
        // server exits before any signal is needed.
        state.stdin.take();
        state
            .child
            .shut_down(MCP_CLEANUP_GRACE, Rung::Wait)
            .await
            .map_err(McpError::from)?;
        state
            .child
            .reap_group(MCP_CLEANUP_GRACE, false)
            .await
            .map_err(McpError::from)?;
        if tokio::time::timeout(MCP_CLEANUP_GRACE, &mut state.stderr)
            .await
            .is_err()
        {
            state.stderr.abort();
            let _ = (&mut state.stderr).await;
        }
        Ok(())
    }
}

impl McpPeer for StdioMcpPeer {
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
        Box::pin(self.close_transport())
    }
}

impl Drop for StdioMcpPeer {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.try_lock() {
            state.stdin.take();
            let _ = state.child.signal(true);
            state.stderr.abort();
        }
    }
}

/// Decodes a `tools/call` result into the crate's tool output contract.
///
/// Both transports speak the same result schema, so the validation rules live
/// here rather than once per transport.
fn decode_tool_result(result: Value) -> Result<ToolExecutionOutput, McpError> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Tool("tools/call omitted content".to_owned()))?;
    if content.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("text")
            && !block.get("text").is_some_and(Value::is_string)
    }) {
        return Err(McpError::Tool(
            "tools/call returned malformed text content".to_owned(),
        ));
    }
    if result
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
    {
        return Err(McpError::Tool(
            "tools/call structuredContent must be an object".to_owned(),
        ));
    }
    if result
        .get("isError")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(McpError::Tool(
            "tools/call isError must be a boolean".to_owned(),
        ));
    }
    let mut model_text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if model_text.is_empty() {
        model_text = match result.get("structuredContent") {
            Some(structured) => serde_json::to_string(structured),
            None => serde_json::to_string(content),
        }
        .map_err(|error| McpError::Tool(error.to_string()))?;
    }
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(McpError::Tool(if model_text.is_empty() {
            "remote MCP tool reported an error".to_owned()
        } else {
            crate::integrations::redact(&model_text)
        }));
    }
    Ok(ToolExecutionOutput::new(model_text)
        .displayed_as(json!({"kind": "mcp", "isError": false}))
        .typed(
            result
                .get("structuredContent")
                .cloned()
                .unwrap_or_else(|| json!({"content": content})),
        ))
}

async fn write_message(state: &mut StdioMcpState, message: &Value) -> Result<(), McpError> {
    let input = state
        .stdin
        .as_mut()
        .ok_or_else(|| McpError::Transport("stdio stdin is closed".to_owned()))?;
    let mut encoded =
        serde_json::to_vec(message).map_err(|error| McpError::Transport(error.to_string()))?;
    encoded.push(b'\n');
    input
        .write_all(&encoded)
        .await
        .map_err(|error| McpError::Transport(format!("cannot write stdio request: {error}")))?;
    input
        .flush()
        .await
        .map_err(|error| McpError::Transport(format!("cannot flush stdio request: {error}")))
}

async fn read_message(
    state: &mut StdioMcpState,
    max_response_bytes: usize,
) -> Result<Value, McpError> {
    let mut encoded = Vec::new();
    let limit = u64::try_from(max_response_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let read = (&mut state.stdout)
        .take(limit)
        .read_until(b'\n', &mut encoded)
        .await
        .map_err(|error| McpError::Transport(format!("cannot read stdio response: {error}")))?;
    if read == 0 {
        return Err(McpError::Transport(
            "stdio server closed its output".to_owned(),
        ));
    }
    if encoded.len() > max_response_bytes {
        return Err(McpError::Transport(
            "stdio response exceeded its byte budget".to_owned(),
        ));
    }
    if !encoded.ends_with(b"\n") {
        return Err(McpError::Transport(
            "stdio response was not newline-delimited".to_owned(),
        ));
    }
    serde_json::from_slice(&encoded)
        .map_err(|error| McpError::Transport(format!("invalid stdio JSON-RPC message: {error}")))
}

/// What the client declares it can answer.
///
/// Reference `mcp/tools.py` passes a sampling callback into the session only for
/// an entry that enables it, and the SDK derives the advertised capability from
/// that callback, so the declaration and the handler are one decision here too.
fn client_capabilities(sampling: bool) -> Value {
    if sampling {
        json!({"sampling": {}})
    } else {
        json!({})
    }
}

/// The JSON-RPC answer to one `sampling/createMessage` request.
///
/// Both transports send the same envelope, so the mapping and the failure shape
/// live once rather than once per transport.
pub(crate) async fn sampling_answer(handler: &Arc<dyn SamplingHandler>, request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let parameters = request.get("params").cloned().unwrap_or(Value::Null);
    match tokio::time::timeout(
        MCP_OPERATION_TIMEOUT,
        handler.complete(sampling_request(&parameters)),
    )
    .await
    {
        Ok(Ok(response)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "role": "assistant",
                "content": {"type": "text", "text": response.text},
                "model": response.model,
                "stopReason": "endTurn",
            },
        }),
        // A failure is answered as an error rather than as a partial completion,
        // so the server never reads half an answer as a whole one. The message
        // is the handler's, which reports the failure it saw rather than the
        // request it made, so no credential travels with it.
        Ok(Err(error)) => sampling_error(&id, &error.to_string()),
        Err(_) => sampling_error(
            &id,
            &format!(
                "the model did not answer within {}s",
                MCP_OPERATION_TIMEOUT.as_secs()
            ),
        ),
    }
}

/// The refusal a client that declared no sampling capability owes a server that
/// asked anyway.
pub(crate) fn method_not_found(request: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "error": {
            "code": -32601,
            "message": "Method not found",
        },
    })
}

/// The request the handler is asked to complete.
///
/// Reference `MCPSamplingHandler.__call__` prepends the system prompt as a
/// message rather than passing it beside them, maps an unknown role onto
/// assistant, and joins the text blocks of a content list while skipping the
/// rest. A malformed request therefore reduces to a poorer request rather than
/// to a failure, which is what keeps a server's own message shapes from failing
/// the call.
fn sampling_request(parameters: &Value) -> SamplingRequest {
    let mut messages = Vec::new();
    if let Some(system) = parameters
        .get("systemPrompt")
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.is_empty())
    {
        messages.push(SamplingMessage {
            role: SamplingRole::System,
            content: system.to_owned(),
        });
    }
    if let Some(inbound) = parameters.get("messages").and_then(Value::as_array) {
        messages.extend(inbound.iter().map(|message| SamplingMessage {
            role: match message.get("role").and_then(Value::as_str) {
                Some("user") => SamplingRole::User,
                _ => SamplingRole::Assistant,
            },
            content: sampling_text(message.get("content")),
        }));
    }
    SamplingRequest {
        messages,
        max_tokens: parameters
            .get("maxTokens")
            .and_then(Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok()),
        temperature_millis: parameters
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|temperature| (temperature * 1_000.0).round())
            .filter(|millis| *millis >= 0.0)
            .and_then(|millis| u16::try_from(millis as u64).ok()),
    }
}

/// The text of one content block, or of the text blocks of a list of them.
fn sampling_text(content: Option<&Value>) -> String {
    let block_text = |block: &Value| {
        (block.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            })
            .map(str::to_owned)
    };
    match content {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(block_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Some(block) => block_text(block).unwrap_or_default(),
        None => String::new(),
    }
}

/// The structured error a failed completion answers with, reference
/// `ErrorData(code=-1, ...)`.
fn sampling_error(id: &Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -1,
            "message": format!("sampling failed: {message}"),
        },
    })
}

async fn reply_method_not_found(
    state: &mut StdioMcpState,
    request: &Value,
) -> Result<(), McpError> {
    write_message(state, &method_not_found(request)).await
}

fn bounded_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown JSON-RPC error");
    crate::integrations::redact(crate::text::truncate_utf8(message, 512))
}

fn validate_jsonrpc_message(message: &Value) -> Result<(), McpError> {
    let object = message
        .as_object()
        .ok_or_else(|| McpError::Transport("JSON-RPC message must be an object".to_owned()))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::Transport(
            "JSON-RPC message omitted version 2.0".to_owned(),
        ));
    }
    if !object.contains_key("method") && !object.contains_key("id") {
        return Err(McpError::Transport(
            "JSON-RPC message is neither a request nor a response".to_owned(),
        ));
    }
    if let Some(error) = object.get("error")
        && (!error.is_object()
            || !error.get("code").is_some_and(Value::is_i64)
            || !error.get("message").is_some_and(Value::is_string))
    {
        return Err(McpError::Transport(
            "JSON-RPC error response is malformed".to_owned(),
        ));
    }
    Ok(())
}

async fn drain_stderr(mut stderr: ChildStderr) {
    let mut sink = tokio::io::sink();
    let _ = tokio::io::copy(&mut stderr, &mut sink).await;
}

mod registry;
pub use registry::{McpRegistry, McpServerStatus, McpServerView};

#[derive(Debug, Error)]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    InvalidConfig(String),
    #[error("MCP server is disabled")]
    Disabled,
    #[error("MCP authorization is required")]
    AuthRequired,
    #[error("unknown MCP server `{0}`")]
    UnknownServer(String),
    #[error("MCP server must be reconnected before it can be enabled or refreshed")]
    ReconnectRequired,
    #[error("MCP transport failed: {0}")]
    Transport(String),
    #[error("MCP tool contract failed: {0}")]
    Tool(String),
    #[error("MCP registry capacity of {0} servers was reached")]
    RegistryFull(usize),
}

impl From<TerminationError> for McpError {
    fn from(error: TerminationError) -> Self {
        Self::Transport(format!("stdio server shutdown failed: {error}"))
    }
}

pub fn validate_config(config: &McpServerConfig) -> Result<(), McpError> {
    if sanitize_mcp_name(&config.alias) != config.alias || config.alias.is_empty() {
        return Err(McpError::InvalidConfig(format!(
            "invalid alias `{}`",
            config.alias
        )));
    }
    if !(1..=MAX_MCP_TIMEOUT_MS).contains(&config.startup_timeout_ms)
        || !(1..=MAX_MCP_TIMEOUT_MS).contains(&config.tool_timeout_ms)
    {
        return Err(McpError::InvalidConfig(format!(
            "timeouts must be between 1 and {MAX_MCP_TIMEOUT_MS} milliseconds"
        )));
    }
    match &config.transport {
        McpTransportConfig::Http { url, headers }
        | McpTransportConfig::StreamableHttp { url, headers } => {
            require_secure_url(url, "server")?;
            validate_headers(headers)?;
        }
        McpTransportConfig::Stdio {
            command,
            environment,
            ..
        } => {
            if command.is_empty() {
                return Err(McpError::InvalidConfig("stdio command is empty".to_owned()));
            }
            if environment
                .keys()
                .any(|key| key.is_empty() || key.contains(['=', '\0']))
            {
                return Err(McpError::InvalidConfig(
                    "stdio environment contains an invalid name".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_remote_tool(tool: &RemoteTool, transport: &McpTransportConfig) -> Result<(), McpError> {
    if tool.name.is_empty() || tool.name.len() > 128 {
        return Err(McpError::Tool("invalid remote tool name".to_owned()));
    }
    if !tool.input_schema.is_object() {
        return Err(McpError::Tool("input schema must be an object".to_owned()));
    }
    if matches!(
        transport,
        McpTransportConfig::Http { .. } | McpTransportConfig::StreamableHttp { .. }
    ) {
        let mut names = BTreeSet::new();
        validate_header_annotations(&tool.input_schema, &mut names)?;
    }
    Ok(())
}

fn validate_header_annotations(
    schema: &Value,
    names: &mut BTreeSet<String>,
) -> Result<(), McpError> {
    if let Some(header) = schema.get("x-mcp-header") {
        let name = header
            .as_str()
            .ok_or_else(|| McpError::Tool("x-mcp-header must be a string".to_owned()))?;
        let primitive = schema
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "string" | "integer" | "boolean"));
        if !primitive || !is_header_name(name) || !names.insert(name.to_ascii_lowercase()) {
            return Err(McpError::Tool(
                "invalid or duplicate x-mcp-header annotation".to_owned(),
            ));
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for property in properties.values() {
            validate_header_annotations(property, names)?;
        }
    }
    Ok(())
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), McpError> {
    if headers
        .iter()
        .any(|(name, value)| !is_header_name(name) || value.contains(['\r', '\n']))
    {
        return Err(McpError::InvalidConfig(
            "invalid static HTTP header".to_owned(),
        ));
    }
    Ok(())
}

fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn require_secure_url(url: &Url, field: &str) -> Result<(), McpError> {
    if url.fragment().is_some() {
        return Err(McpError::InvalidConfig(format!(
            "{field} URL must not contain a fragment"
        )));
    }
    if is_secure_transport(url) {
        Ok(())
    } else {
        Err(McpError::InvalidConfig(format!(
            "{field} URL must use HTTPS or loopback HTTP"
        )))
    }
}

fn transport_url(transport: &McpTransportConfig) -> Option<&Url> {
    match transport {
        McpTransportConfig::Http { url, .. } | McpTransportConfig::StreamableHttp { url, .. } => {
            Some(url)
        }
        McpTransportConfig::Stdio { .. } => None,
    }
}

fn transport_name(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Http { .. } => "http",
        McpTransportConfig::StreamableHttp { .. } => "streamable-http",
        McpTransportConfig::Stdio { .. } => "stdio",
    }
}

fn canonical_diagnostic(alias: &str, error: &McpError) -> String {
    let message = error.to_string();
    let redacted = crate::integrations::redact(&message);
    format!("MCP `{alias}`: {redacted}")
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::policy::{ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode};
    use crate::tools::ToolExecutionOutput;
    use serde_json::json;
    use tokio::sync::Notify;

    struct AlwaysApprove;

    impl ApprovalAgent for AlwaysApprove {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
        }
    }

    struct FakePeer {
        tools: Vec<RemoteTool>,
        fail_calls: bool,
        oversized: bool,
        closed: AtomicBool,
    }

    struct SerialRefreshPeer {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }

    struct FailingClosePeer;

    impl McpPeer for FailingClosePeer {
        fn discover<'a>(&'a self, _max_response_bytes: usize) -> McpFuture<'a, Vec<RemoteTool>> {
            Box::pin(async { Ok(vec![remote_tool()]) })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            _arguments: Value,
            _max_response_bytes: usize,
            _output: ToolOutputSink,
        ) -> McpFuture<'a, ToolExecutionOutput> {
            Box::pin(async { Err(McpError::Transport("not used".to_owned())) })
        }

        fn close<'a>(&'a self) -> McpFuture<'a, ()> {
            Box::pin(async { Err(McpError::Transport("close failed".to_owned())) })
        }
    }

    impl McpPeer for SerialRefreshPeer {
        fn discover<'a>(&'a self, _max_response_bytes: usize) -> McpFuture<'a, Vec<RemoteTool>> {
            Box::pin(async move {
                let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
                self.maximum.fetch_max(active, Ordering::AcqRel);
                tokio::time::sleep(Duration::from_millis(10)).await;
                self.active.fetch_sub(1, Ordering::AcqRel);
                Ok(vec![remote_tool()])
            })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            _arguments: Value,
            _max_response_bytes: usize,
            _output: ToolOutputSink,
        ) -> McpFuture<'a, ToolExecutionOutput> {
            Box::pin(async { Err(McpError::Transport("not used".to_owned())) })
        }

        fn close<'a>(&'a self) -> McpFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    impl McpPeer for FakePeer {
        fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<RemoteTool>> {
            Box::pin(async move {
                // A real transport rejects the payload before decoding it.
                let encoded = serde_json::to_vec(&self.tools)
                    .map_err(|error| McpError::Transport(error.to_string()))?;
                if encoded.len() > max_response_bytes {
                    return Err(McpError::Tool(
                        "MCP discovery response exceeded its byte budget".to_owned(),
                    ));
                }
                Ok(self.tools.clone())
            })
        }

        fn call<'a>(
            &'a self,
            name: &'a str,
            arguments: Value,
            max_response_bytes: usize,
            _output: ToolOutputSink,
        ) -> McpFuture<'a, ToolExecutionOutput> {
            Box::pin(async move {
                if self.fail_calls {
                    return Err(McpError::Transport("peer crashed".to_owned()));
                }
                let model_text = if self.oversized {
                    " ".repeat(max_response_bytes.saturating_add(1))
                } else {
                    format!("{name} completed")
                };
                Ok(ToolExecutionOutput::new(model_text)
                    .typed(json!({"tool": name, "arguments": arguments})))
            })
        }

        fn close<'a>(&'a self) -> McpFuture<'a, ()> {
            Box::pin(async move {
                self.closed.store(true, Ordering::Release);
                Ok(())
            })
        }
    }

    struct FakeFactory {
        peers: BTreeMap<String, Arc<dyn McpPeer>>,
    }

    struct HangingFactory;

    impl McpPeerFactory for HangingFactory {
        fn connect<'a>(&'a self, _config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
            Box::pin(async { std::future::pending().await })
        }
    }

    struct HangingPeer {
        entered: Notify,
        closed: AtomicBool,
    }

    impl McpPeer for HangingPeer {
        fn discover<'a>(&'a self, _max_response_bytes: usize) -> McpFuture<'a, Vec<RemoteTool>> {
            Box::pin(async move { Ok(vec![remote_tool()]) })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            _arguments: Value,
            _max_response_bytes: usize,
            _output: ToolOutputSink,
        ) -> McpFuture<'a, ToolExecutionOutput> {
            Box::pin(async move {
                self.entered.notify_one();
                std::future::pending().await
            })
        }

        fn close<'a>(&'a self) -> McpFuture<'a, ()> {
            Box::pin(async move {
                self.closed.store(true, Ordering::Release);
                Ok(())
            })
        }
    }

    impl McpPeerFactory for FakeFactory {
        fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
            Box::pin(async move {
                self.peers
                    .get(&config.alias)
                    .cloned()
                    .ok_or_else(|| McpError::Transport("connection failed".to_owned()))
            })
        }
    }

    fn config(alias: &str) -> McpServerConfig {
        McpServerConfig {
            alias: alias.to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: Url::parse(&format!("https://{alias}.example/mcp")).expect("url"),
                headers: BTreeMap::new(),
            },
            enabled: true,
            disabled_tools: BTreeSet::new(),
            startup_timeout_ms: DEFAULT_MCP_STARTUP_TIMEOUT_MS,
            tool_timeout_ms: DEFAULT_MCP_TOOL_TIMEOUT_MS,
            auth: Default::default(),
            prompt: None,
            sampling_enabled: true,
        }
    }

    fn remote_tool() -> RemoteTool {
        RemoteTool {
            name: "search".to_owned(),
            description: "Search remote data".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            annotations: json!({"readOnlyHint": true}),
        }
    }

    #[test]
    fn malformed_jsonrpc_envelopes_fail_closed() {
        assert!(validate_jsonrpc_message(&json!({"id": 1, "result": {}})).is_err());
        assert!(
            validate_jsonrpc_message(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {"code": "bad", "message": 7}
            }))
            .is_err()
        );
        assert!(
            validate_jsonrpc_message(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {}
            }))
            .is_ok()
        );
    }

    #[tokio::test]
    async fn partial_failure_keeps_healthy_server_and_policy_guards_tool() {
        let good: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let factory = Arc::new(FakeFactory {
            peers: BTreeMap::from([("good".to_owned(), good)]),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        // An MCP tool raises no granular requirement, so what grants it is the
        // session permission an "allow always" sets.
        policy.set_tool_permission("good_search", PermissionMode::Always);
        let diagnostics = registry
            .discover_all(
                vec![config("good"), config("failed")],
                factory,
                &tools,
                policy,
                Arc::new(AlwaysApprove),
            )
            .await;
        assert_eq!(diagnostics.len(), 1);
        let views = registry.read().await;
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].status, McpServerStatus::Failed);
        assert_eq!(views[1].status, McpServerStatus::Healthy);
        let output = tools
            .invoke(
                "good_search",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"query": "rust"}),
                },
            )
            .await
            .expect("invoke");
        assert_eq!(output.typed_result["tool"], "search");

        let disabled = registry.toggle("good", false).await.expect("disable");
        assert_eq!(disabled.status, McpServerStatus::Disabled);
        assert!(matches!(
            tools
                .invoke(
                    "good_search",
                    ToolInvocation {
                        call_id: "call-2".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
        let enabled = registry.toggle("good", true).await.expect("reconnect");
        assert_eq!(enabled.status, McpServerStatus::Healthy);
    }

    /// The usage hint an entry declares rides on every tool the server
    /// publishes, which is the only place the model ever reads it.
    #[tokio::test]
    async fn a_declared_prompt_reaches_the_description_of_every_published_tool() {
        let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let mut configured = config("docs");
        configured.prompt = Some("Search the handbook first".to_owned());
        let tools = ToolRegistry::default();

        McpRegistry::default()
            .discover_all(
                vec![configured],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("docs".to_owned(), peer)]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let published = tools
            .list()
            .expect("the registry lists what it registered")
            .into_iter()
            .find(|spec| spec.name == "docs_search")
            .expect("the server published its tool");
        assert_eq!(
            published.description,
            "Search remote data\nHint: Search the handbook first"
        );
    }

    #[tokio::test]
    async fn rediscovery_retires_aliases_missing_from_the_new_configuration() {
        let first_a = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let first_b = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        registry
            .discover_all(
                vec![config("a"), config("b")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([
                        ("a".to_owned(), first_a.clone() as Arc<dyn McpPeer>),
                        ("b".to_owned(), first_b.clone() as Arc<dyn McpPeer>),
                    ]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let replacement_a: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        registry
            .discover_all(
                vec![config("a")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("a".to_owned(), replacement_a)]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        assert!(first_a.closed.load(Ordering::Acquire));
        assert!(first_b.closed.load(Ordering::Acquire));
        assert_eq!(
            registry
                .read()
                .await
                .into_iter()
                .map(|view| view.alias)
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert!(matches!(
            tools
                .invoke(
                    "b_search",
                    ToolInvocation {
                        call_id: "retired-call".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn disabled_tool_preferences_survive_disabled_startup_and_reconnect() {
        let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let factory = Arc::new(FakeFactory {
            peers: BTreeMap::from([("disabled".to_owned(), peer)]),
        });
        let mut disabled = config("disabled");
        disabled.enabled = false;
        disabled.disabled_tools = BTreeSet::from(["search".to_owned()]);
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        registry
            .discover_all(
                vec![disabled],
                factory,
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let initial = registry.read().await.remove(0);
        assert_eq!(
            initial.disabled_tools,
            BTreeSet::from(["search".to_owned()])
        );
        let enabled = registry.toggle("disabled", true).await.expect("reconnect");
        assert_eq!(
            enabled.disabled_tools,
            BTreeSet::from(["disabled_search".to_owned()])
        );
        assert!(matches!(
            tools
                .invoke(
                    "disabled_search",
                    ToolInvocation {
                        call_id: "call-disabled".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }

    /// Reference `_apply_per_source_filtering` keys the denylist by source, so
    /// two servers exposing the same remote name are filtered independently:
    /// disabling `search` on one leaves the other's published name in place.
    #[tokio::test]
    async fn a_per_source_denylist_only_withholds_that_server_tools() {
        let peers = ["muted", "loud"]
            .map(|alias| {
                let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
                    tools: vec![remote_tool()],
                    fail_calls: false,
                    oversized: false,
                    closed: AtomicBool::new(false),
                });
                (alias.to_owned(), peer)
            })
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let mut muted = config("muted");
        muted.disabled_tools = BTreeSet::from(["search".to_owned()]);
        let tools = ToolRegistry::default();
        McpRegistry::default()
            .discover_all(
                vec![muted, config("loud")],
                Arc::new(FakeFactory { peers }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let published = tools
            .available(
                &crate::matching::NameFilter::default(),
                &crate::matching::NameFilter::default(),
            )
            .expect("available")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(published, ["loud_search".to_owned()]);
        assert!(matches!(
            tools
                .invoke(
                    "muted_search",
                    ToolInvocation {
                        call_id: "muted".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn per_tool_toggle_updates_visible_state_and_registry_availability() {
        let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        registry
            .discover_all(
                vec![config("tools")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("tools".to_owned(), peer)]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let disabled = registry
            .toggle_tool("tools", "tools_search", false)
            .await
            .expect("tool disables");
        assert!(disabled.disabled_tools.contains("tools_search"));
        assert!(matches!(
            tools
                .invoke(
                    "tools_search",
                    ToolInvocation {
                        call_id: "disabled".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));

        let refreshed = registry.refresh("tools").await.expect("server refreshes");
        assert!(refreshed.disabled_tools.contains("tools_search"));
        assert!(matches!(
            tools
                .invoke(
                    "tools_search",
                    ToolInvocation {
                        call_id: "still-disabled".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));

        let enabled = registry
            .toggle_tool("tools", "tools_search", true)
            .await
            .expect("tool re-enables");
        assert!(enabled.disabled_tools.is_empty());
        tools
            .invoke(
                "tools_search",
                ToolInvocation {
                    call_id: "enabled".to_owned(),
                    arguments: json!({"query": "rust"}),
                },
            )
            .await
            .expect("enabled tool invokes");

        let cleared = registry.clear_auth("tools").await.expect("auth clears");
        assert_eq!(cleared.status, McpServerStatus::AuthRequired);
        assert!(matches!(
            tools
                .invoke(
                    "tools_search",
                    ToolInvocation {
                        call_id: "logged-out".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_refreshes_are_serialized() {
        let peer = Arc::new(SerialRefreshPeer {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        });
        let registry = McpRegistry::default();
        registry
            .discover_all(
                vec![config("serialized")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([(
                        "serialized".to_owned(),
                        peer.clone() as Arc<dyn McpPeer>,
                    )]),
                }),
                &ToolRegistry::default(),
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let (left, right) = tokio::join!(
            registry.refresh("serialized"),
            registry.refresh("serialized")
        );

        left.expect("first refresh");
        right.expect("second refresh");
        assert_eq!(peer.maximum.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn cleanup_failure_never_rolls_back_disable_or_logout_state() {
        let peer: Arc<dyn McpPeer> = Arc::new(FailingClosePeer);
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        registry
            .discover_all(
                vec![config("cleanup")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("cleanup".to_owned(), peer)]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        let disabled = registry
            .toggle("cleanup", false)
            .await
            .expect("disable commits despite close failure");
        assert!(!disabled.enabled);
        assert_eq!(disabled.status, McpServerStatus::Disabled);
        assert!(
            disabled
                .diagnostic
                .as_deref()
                .is_some_and(|message| message.contains("close failed"))
        );

        registry
            .toggle("cleanup", true)
            .await
            .expect("server reconnects");
        let logged_out = registry
            .clear_auth("cleanup")
            .await
            .expect("logout commits despite close failure");
        assert_eq!(logged_out.status, McpServerStatus::AuthRequired);
        assert!(
            logged_out
                .diagnostic
                .as_deref()
                .is_some_and(|message| message.contains("close failed"))
        );
    }

    #[tokio::test]
    async fn replacing_server_alias_closes_the_previous_owned_peer() {
        let first = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let second = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        registry
            .discover_all(
                vec![config("good")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("good".to_owned(), first.clone() as Arc<dyn McpPeer>)]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;
        registry
            .discover_all(
                vec![config("good")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([(
                        "good".to_owned(),
                        second.clone() as Arc<dyn McpPeer>,
                    )]),
                }),
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;
        assert!(first.closed.load(Ordering::Acquire));
        assert!(!second.closed.load(Ordering::Acquire));
        assert_eq!(registry.read().await[0].status, McpServerStatus::Healthy);
    }

    #[tokio::test]
    async fn failed_discovery_closes_the_connected_peer() {
        let peer = Arc::new(FakePeer {
            tools: vec![RemoteTool {
                description: "x".repeat(MAX_MCP_DISCOVERY_BYTES),
                ..remote_tool()
            }],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let diagnostics = McpRegistry::default()
            .discover_all(
                vec![config("oversized")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([(
                        "oversized".to_owned(),
                        peer.clone() as Arc<dyn McpPeer>,
                    )]),
                }),
                &ToolRegistry::default(),
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;

        assert_eq!(diagnostics.len(), 1);
        assert!(peer.closed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn disabling_server_cancels_hung_call_without_waiting_for_peer() {
        let peer = Arc::new(HangingPeer {
            entered: Notify::new(),
            closed: AtomicBool::new(false),
        });
        let factory = Arc::new(FakeFactory {
            peers: BTreeMap::from([("good".to_owned(), peer.clone() as Arc<dyn McpPeer>)]),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        // An MCP tool raises no granular requirement, so what grants it is the
        // session permission an "allow always" sets.
        policy.set_tool_permission("good_search", PermissionMode::Always);
        assert!(
            registry
                .discover_all(
                    vec![config("good")],
                    factory,
                    &tools,
                    policy,
                    Arc::new(AlwaysApprove),
                )
                .await
                .is_empty()
        );
        let invocation = {
            let tools = tools.clone();
            tokio::spawn(async move {
                tools
                    .invoke(
                        "good_search",
                        ToolInvocation {
                            call_id: "hung".to_owned(),
                            arguments: json!({"query": "rust"}),
                        },
                    )
                    .await
            })
        };
        peer.entered.notified().await;

        let disabled =
            tokio::time::timeout(Duration::from_millis(250), registry.toggle("good", false))
                .await
                .expect("toggle did not wait for the call")
                .expect("disable");

        assert_eq!(disabled.status, McpServerStatus::Disabled);
        assert!(peer.closed.load(Ordering::Acquire));
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(250), invocation)
                .await
                .expect("hung call was cancelled")
                .expect("invoke task"),
            Err(ToolError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn timed_out_call_retires_and_closes_the_owned_peer() {
        let peer = Arc::new(HangingPeer {
            entered: Notify::new(),
            closed: AtomicBool::new(false),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        // An MCP tool raises no granular requirement, so what grants it is the
        // session permission an "allow always" sets.
        policy.set_tool_permission("good_search", PermissionMode::Always);
        let mut server = config("good");
        server.tool_timeout_ms = 10;
        registry
            .discover_all(
                vec![server],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("good".to_owned(), peer.clone() as Arc<dyn McpPeer>)]),
                }),
                &tools,
                policy,
                Arc::new(AlwaysApprove),
            )
            .await;
        assert!(matches!(
            tools
                .invoke(
                    "good_search",
                    ToolInvocation {
                        call_id: "timeout".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::Execution(message)) if message.contains("timed out")
        ));
        assert!(peer.closed.load(Ordering::Acquire));
        assert_eq!(registry.read().await[0].status, McpServerStatus::Failed);
    }

    #[tokio::test]
    async fn cancelled_call_retires_and_closes_the_owned_peer() {
        let peer = Arc::new(HangingPeer {
            entered: Notify::new(),
            closed: AtomicBool::new(false),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        // An MCP tool raises no granular requirement, so what grants it is the
        // session permission an "allow always" sets.
        policy.set_tool_permission("good_search", PermissionMode::Always);
        registry
            .discover_all(
                vec![config("good")],
                Arc::new(FakeFactory {
                    peers: BTreeMap::from([("good".to_owned(), peer.clone() as Arc<dyn McpPeer>)]),
                }),
                &tools,
                policy,
                Arc::new(AlwaysApprove),
            )
            .await;
        let invocation = tokio::spawn({
            let tools = tools.clone();
            async move {
                tools
                    .invoke(
                        "good_search",
                        ToolInvocation {
                            call_id: "cancelled".to_owned(),
                            arguments: json!({"query": "rust"}),
                        },
                    )
                    .await
            }
        });
        peer.entered.notified().await;
        invocation.abort();
        let _ = invocation.await;
        tokio::time::timeout(Duration::from_millis(250), async {
            while !peer.closed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled peer cleanup");
        assert_eq!(registry.read().await[0].status, McpServerStatus::Failed);
    }

    #[tokio::test]
    async fn malformed_remote_schema_isolated_without_granting_tool() {
        let malformed = RemoteTool {
            input_schema: Value::Null,
            ..remote_tool()
        };
        let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![malformed],
            fail_calls: false,
            oversized: false,
            closed: AtomicBool::new(false),
        });
        let factory = Arc::new(FakeFactory {
            peers: BTreeMap::from([("good".to_owned(), peer)]),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        let diagnostics = registry
            .discover_all(
                vec![config("good")],
                factory,
                &tools,
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;
        assert_eq!(diagnostics.len(), 1);
        assert!(tools.list().expect("list").is_empty());
    }

    #[tokio::test]
    async fn peer_response_budget_is_enforced_before_deserialization() {
        let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
            tools: vec![remote_tool()],
            fail_calls: false,
            oversized: true,
            closed: AtomicBool::new(false),
        });
        let factory = Arc::new(FakeFactory {
            peers: BTreeMap::from([("good".to_owned(), peer)]),
        });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::new(128);
        assert!(
            registry
                .discover_all(
                    vec![config("good")],
                    factory,
                    &tools,
                    PermissionStore::default(),
                    Arc::new(AlwaysApprove),
                )
                .await
                .is_empty()
        );

        assert!(matches!(
            tools
                .invoke(
                    "good_search",
                    ToolInvocation {
                        call_id: "oversized".to_owned(),
                        arguments: json!({"query": "rust"}),
                    },
                )
                .await,
            Err(ToolError::OutputTooLarge {
                actual: 129,
                limit: 128
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn hung_peer_connection_is_bounded_by_the_operation_deadline() {
        let registry = McpRegistry::default();
        let diagnostics = registry
            .discover_all(
                vec![config("hung")],
                Arc::new(HangingFactory),
                &ToolRegistry::default(),
                PermissionStore::default(),
                Arc::new(AlwaysApprove),
            )
            .await;
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("operation timed out"));
        assert!(matches!(
            registry.read().await.as_slice(),
            [McpServerView {
                status: McpServerStatus::Failed,
                ..
            }]
        ));
    }

    // ----------------------------------------------------------------------
    // Sampling
    // ----------------------------------------------------------------------

    /// An MCP server that asks the client to complete a message in the middle
    /// of a tool call, which is the shape reference `MCPSamplingHandler` serves.
    ///
    /// The script answers on its own standard output, so the case drives the
    /// real stdio transport rather than a fake peer: the inbound request has to
    /// survive the same framing, the same request loop and the same lock the
    /// outbound call holds.
    #[cfg(unix)]
    const SAMPLING_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"id":9001'*)
      case "$line" in
        *'"error"'*) answer=$(printf '%s' "$line" | sed -e 's/.*"message":"//' -e 's/".*//') ;;
        *) answer=$(printf '%s' "$line" | sed -e 's/.*"text":"//' -e 's/".*//') ;;
      esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$pending" "$answer"
      ;;
    *'"method":"initialize"'*)
      case "$line" in
        *'"capabilities":{"sampling":{}}'*) advertised=yes ;;
        *) advertised=no ;;
      esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"probe","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"advertised","description":"reports the advertised capability","inputSchema":{"type":"object"}},{"name":"ask","description":"asks the client","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"name":"advertised"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$advertised"
      ;;
    *'"method":"tools/call"'*)
      pending="$id"
      printf '{"jsonrpc":"2.0","id":9001,"method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"ping"}},{"role":"oracle","content":[{"type":"image","data":"x"},{"type":"text","text":"pong"}]}],"systemPrompt":"be brief","maxTokens":16,"temperature":0.25}}\n'
      ;;
  esac
done
"#;

    #[cfg(unix)]
    struct RecordingSampler {
        seen: Arc<Mutex<Vec<SamplingRequest>>>,
        answer: Result<SamplingResponse, String>,
    }

    #[cfg(unix)]
    impl SamplingHandler for RecordingSampler {
        fn complete<'a>(&'a self, request: SamplingRequest) -> McpFuture<'a, SamplingResponse> {
            Box::pin(async move {
                self.seen.lock().await.push(request);
                self.answer.clone().map_err(McpError::Tool)
            })
        }
    }

    #[cfg(unix)]
    fn sampling_server(directory: &std::path::Path, sampling_enabled: bool) -> McpServerConfig {
        let script = directory.join("sampling-server.sh");
        std::fs::write(&script, SAMPLING_SERVER).expect("the script is written");
        McpServerConfig {
            alias: "probe".to_owned(),
            transport: McpTransportConfig::Stdio {
                command: "/bin/sh".to_owned(),
                arguments: vec![script.to_string_lossy().into_owned()],
                environment: BTreeMap::new(),
                working_directory: None,
            },
            enabled: true,
            auth: McpAuthConfig::default(),
            startup_timeout_ms: DEFAULT_MCP_STARTUP_TIMEOUT_MS,
            tool_timeout_ms: DEFAULT_MCP_TOOL_TIMEOUT_MS,
            sampling_enabled,
            disabled_tools: BTreeSet::new(),
            prompt: None,
        }
    }

    /// A server that asks for a completion receives one, and the request it
    /// made reaches the handler in the shape the reference maps it into.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_requests_a_completion_receives_one() {
        let directory = tempfile::tempdir().expect("tempdir");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let factory = DefaultMcpPeerFactory::with_sampling(Arc::new(RecordingSampler {
            seen: seen.clone(),
            answer: Ok(SamplingResponse {
                text: "sampled".to_owned(),
                model: "probe-model".to_owned(),
            }),
        }));
        let config = sampling_server(directory.path(), true);
        let peer = factory.connect(&config).await.expect("the peer connects");

        let advertised = peer
            .call(
                "advertised",
                json!({}),
                MAX_MCP_DISCOVERY_BYTES,
                ToolOutputSink::discard(MAX_MCP_DISCOVERY_BYTES),
            )
            .await
            .expect("the capability probe answers");
        assert_eq!(advertised.model_text.trim(), "yes", "{advertised:?}");

        let sampled = peer
            .call(
                "ask",
                json!({}),
                MAX_MCP_DISCOVERY_BYTES,
                ToolOutputSink::discard(MAX_MCP_DISCOVERY_BYTES),
            )
            .await
            .expect("the tool call answers");
        assert_eq!(sampled.model_text.trim(), "sampled", "{sampled:?}");

        let requests = seen.lock().await;
        let request = requests.first().expect("the handler was asked");
        assert_eq!(
            request.messages,
            vec![
                SamplingMessage {
                    role: SamplingRole::System,
                    content: "be brief".to_owned(),
                },
                SamplingMessage {
                    role: SamplingRole::User,
                    content: "ping".to_owned(),
                },
                // An unknown role maps onto assistant and the blocks that are
                // not text are skipped rather than failing the request.
                SamplingMessage {
                    role: SamplingRole::Assistant,
                    content: "pong".to_owned(),
                },
            ]
        );
        assert_eq!(request.max_tokens, Some(16));
        assert_eq!(request.temperature_millis, Some(250));
        let _ = peer.close().await;
    }

    /// A backend failure is answered as a structured error rather than as a
    /// partial completion.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_completion_answers_with_a_structured_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let factory = DefaultMcpPeerFactory::with_sampling(Arc::new(RecordingSampler {
            seen: Arc::new(Mutex::new(Vec::new())),
            answer: Err("the backend refused".to_owned()),
        }));
        let config = sampling_server(directory.path(), true);
        let peer = factory.connect(&config).await.expect("the peer connects");
        let answered = peer
            .call(
                "ask",
                json!({}),
                MAX_MCP_DISCOVERY_BYTES,
                ToolOutputSink::discard(MAX_MCP_DISCOVERY_BYTES),
            )
            .await
            .expect("the tool call answers");
        assert!(
            answered.model_text.contains("sampling failed"),
            "{answered:?}"
        );
        assert!(
            answered.model_text.contains("the backend refused"),
            "{answered:?}"
        );
        let _ = peer.close().await;
    }

    /// An entry that disables sampling advertises nothing and refuses an
    /// inbound request with the capability-absent error.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_disabled_entry_refuses_an_inbound_sampling_request() {
        let directory = tempfile::tempdir().expect("tempdir");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let factory = DefaultMcpPeerFactory::with_sampling(Arc::new(RecordingSampler {
            seen: seen.clone(),
            answer: Ok(SamplingResponse {
                text: "sampled".to_owned(),
                model: "probe-model".to_owned(),
            }),
        }));
        let config = sampling_server(directory.path(), false);
        let peer = factory.connect(&config).await.expect("the peer connects");

        let advertised = peer
            .call(
                "advertised",
                json!({}),
                MAX_MCP_DISCOVERY_BYTES,
                ToolOutputSink::discard(MAX_MCP_DISCOVERY_BYTES),
            )
            .await
            .expect("the capability probe answers");
        assert_eq!(advertised.model_text.trim(), "no", "{advertised:?}");

        let refused = peer
            .call(
                "ask",
                json!({}),
                MAX_MCP_DISCOVERY_BYTES,
                ToolOutputSink::discard(MAX_MCP_DISCOVERY_BYTES),
            )
            .await
            .expect("the tool call answers");
        assert!(
            refused.model_text.contains("Method not found"),
            "{refused:?}"
        );
        assert!(
            seen.lock().await.is_empty(),
            "a disabled entry still reached the handler"
        );
        let _ = peer.close().await;
    }
}
