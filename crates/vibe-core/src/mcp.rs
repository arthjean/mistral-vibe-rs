use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use url::Url;

use crate::policy::{ApprovalAgent, PermissionContext, PermissionStore, PolicyGuardedTool};
use crate::remote_tools::{
    ProviderReach, public_tool_name, sanitize_mcp_name, set_all, tool_availability,
};
use crate::text::canonical_url;

use crate::tools::{
    ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler, ToolInvocation, ToolOutputSink,
    ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

mod auth;
pub mod authorization;
pub mod descriptor_cache;
mod render;
mod session;

use render::McpToolResult;
use session::{Endpoint, Session, SessionError};

const MAX_MCP_SERVERS: usize = 256;
const MCP_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MCP_STARTUP_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_MCP_TOOL_TIMEOUT_MS: u64 = 60_000;
const MAX_MCP_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

pub use auth::{
    DEFAULT_MCP_API_KEY_FORMAT, DEFAULT_MCP_API_KEY_HEADER, DEFAULT_MCP_OAUTH_REDIRECT_PORT,
    MCP_TOKEN_PLACEHOLDER, McpAuthConfig, McpOAuthConfig, McpStaticAuth,
};
pub use authorization::{
    McpAuthenticationError, McpAuthenticationService, McpAuthorization, McpAuthorizationKind,
    McpAuthorizationReason, McpAuthorizationRef, McpAuthorizationRequired,
    McpAuthorizationSnapshot, McpCatalogOwner, McpCredentialRemoval, McpEnvironment,
    McpServerRemoveError, authorization_kind,
};
pub use descriptor_cache::McpDescriptorCache;

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

/// `command` as an entry wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpDeclaredCommand {
    Text(String),
    Argv(Vec<String>),
}

/// The parts of an entry the reference fingerprints as they were written,
/// which the decoded transport no longer carries: the shape of `command`, the
/// `args` apart from it, the `cwd` before it was resolved and the `url` before
/// it was parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpDeclared {
    #[serde(default)]
    pub command: Option<McpDeclaredCommand>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
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
    /// Whether the server may ask the client for model completions. An entry
    /// that disables it advertises no sampling capability, and a server that
    /// asks anyway is refused.
    #[serde(default = "default_sampling_enabled")]
    pub sampling_enabled: bool,
    /// The entry as written, where the configuration it came from knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<McpDeclared>,
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

/// A tool a server published, reference `RemoteTool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTool {
    pub name: String,
    /// Absent and empty differ in the descriptor cache, and both read as "no
    /// description" when the tool is published.
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: Value,
}

/// A configured MCP server this client discovers and calls.
///
/// Reference `mcp/tools.py` opens a session of its own for every discovery and
/// for every HTTP call, and keeps one pooled session per stdio server for its
/// calls. A peer is that policy for one server, so connecting one does no I/O:
/// each operation opens what it needs. The headers are the ones the server's
/// authorization resolved for this operation; a stdio server ignores them.
pub trait McpPeer: Send + Sync {
    fn discover<'a>(
        &'a self,
        headers: &'a BTreeMap<String, String>,
    ) -> McpFuture<'a, Vec<RemoteTool>>;
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        headers: &'a BTreeMap<String, String>,
    ) -> McpFuture<'a, ToolExecutionOutput>;
    /// Ends whatever the peer keeps open between operations.
    fn close<'a>(&'a self) -> McpFuture<'a, ()>;
}

pub trait McpPeerFactory: Send + Sync {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>>;
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
        let sampling = self.sampling.clone();
        Box::pin(async move {
            validate_config(config)?;
            Ok(Arc::new(ServerPeer::new(config, sampling)) as Arc<dyn McpPeer>)
        })
    }
}

/// The HTTP-only factory the connector gateway uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpMcpPeerFactory;

impl McpPeerFactory for HttpMcpPeerFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        Box::pin(async move {
            validate_config(config)?;
            if matches!(config.transport, McpTransportConfig::Stdio { .. }) {
                return Err(McpError::Transport(
                    "the HTTP MCP factory requires an HTTP transport".to_owned(),
                ));
            }
            Ok(Arc::new(ServerPeer::new(config, None)) as Arc<dyn McpPeer>)
        })
    }
}

/// One configured server.
struct ServerPeer {
    config: McpServerConfig,
    /// Reference `server.url` for HTTP, `"stdio:" + " ".join(argv)` for stdio:
    /// what a call result names as its server.
    label: String,
    startup_timeout: Duration,
    tool_timeout: Duration,
    /// Present only when the entry enables sampling and the host installed a
    /// handler, which is exactly when a call session advertises it.
    sampling: Option<Arc<dyn SamplingHandler>>,
    /// Reference `MCPConnectionPool`: the stdio server's long-lived call
    /// session, serialized by this lock as `_StdioConnection` serializes its
    /// queue.
    pooled: Mutex<Option<Session>>,
}

impl ServerPeer {
    fn new(config: &McpServerConfig, sampling: Option<Arc<dyn SamplingHandler>>) -> Self {
        let label = match &config.transport {
            McpTransportConfig::Stdio {
                command, arguments, ..
            } => {
                let mut argv = vec![command.as_str()];
                argv.extend(arguments.iter().map(String::as_str));
                format!("stdio:{}", argv.join(" "))
            }
            McpTransportConfig::Http { url, .. }
            | McpTransportConfig::StreamableHttp { url, .. } => config
                .declared
                .as_ref()
                .and_then(|declared| declared.url.clone())
                .unwrap_or_else(|| url.to_string()),
        };
        Self {
            config: config.clone(),
            label,
            startup_timeout: Duration::from_millis(config.startup_timeout_ms),
            tool_timeout: Duration::from_millis(config.tool_timeout_ms),
            sampling: sampling.filter(|_| config.sampling_enabled),
            pooled: Mutex::new(None),
        }
    }

    fn endpoint<'a>(&'a self, headers: &'a BTreeMap<String, String>) -> Endpoint<'a> {
        match &self.config.transport {
            McpTransportConfig::Stdio {
                command,
                arguments,
                environment,
                working_directory,
            } => Endpoint::Stdio {
                program: command,
                arguments,
                environment,
                working_directory: working_directory.as_deref(),
            },
            McpTransportConfig::Http { url, .. }
            | McpTransportConfig::StreamableHttp { url, .. } => Endpoint::Http { url, headers },
        }
    }

    const fn is_stdio(&self) -> bool {
        matches!(self.config.transport, McpTransportConfig::Stdio { .. })
    }

    async fn discover_tools(
        &self,
        headers: &BTreeMap<String, String>,
    ) -> Result<Vec<RemoteTool>, McpError> {
        let mut session = Session::open(self.endpoint(headers), Some(self.startup_timeout), None)
            .await
            .map_err(session_error)?;
        let listed = session.list_tools().await;
        let terminate = listed.as_ref().err().is_none_or(Session::terminates);
        session.close(terminate).await;
        listed.map_err(session_error)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        headers: &BTreeMap<String, String>,
    ) -> Result<ToolExecutionOutput, McpError> {
        let answer = if self.is_stdio() {
            let mut pooled = self.pooled.lock().await;
            // A session whose server went away is dropped and respawned once
            // before the call, which is what `_StdioConnection._handle` does
            // when the dead session refuses the write.
            if pooled.as_ref().is_some_and(Session::is_dead)
                && let Some(dead) = pooled.take()
            {
                dead.close(true).await;
            }
            if pooled.is_none() {
                *pooled = Some(
                    Session::open(
                        self.endpoint(headers),
                        Some(self.startup_timeout),
                        self.sampling.clone(),
                    )
                    .await
                    .map_err(session_error)?,
                );
            }
            let Some(session) = pooled.as_mut() else {
                return Err(McpError::Transport(
                    "the stdio session is missing".to_owned(),
                ));
            };
            session
                .call_tool(name, arguments, Some(self.tool_timeout))
                .await
        } else {
            let mut session = Session::open(
                self.endpoint(headers),
                Some(self.startup_timeout),
                self.sampling.clone(),
            )
            .await
            .map_err(session_error)?;
            let answer = session
                .call_tool(name, arguments, Some(self.tool_timeout))
                .await;
            let terminate = answer.as_ref().err().is_none_or(Session::terminates);
            session.close(terminate).await;
            answer
        }
        .map_err(session_error)?;
        let result = McpToolResult::parse(
            self.label.clone(),
            name.to_owned(),
            &answer.result,
            &answer.raw,
        );
        Ok(ToolExecutionOutput::new(result.model_text())
            .displayed_as(json!({"kind": "mcp", "isError": false}))
            .typed(result.typed()))
    }
}

impl McpPeer for ServerPeer {
    fn discover<'a>(
        &'a self,
        headers: &'a BTreeMap<String, String>,
    ) -> McpFuture<'a, Vec<RemoteTool>> {
        Box::pin(self.discover_tools(headers))
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        headers: &'a BTreeMap<String, String>,
    ) -> McpFuture<'a, ToolExecutionOutput> {
        Box::pin(self.call_tool(name, arguments, headers))
    }

    fn close<'a>(&'a self) -> McpFuture<'a, ()> {
        Box::pin(async move {
            if let Some(session) = self.pooled.lock().await.take() {
                session.close(true).await;
            }
            Ok(())
        })
    }
}

fn session_error(error: SessionError) -> McpError {
    match error {
        SessionError::Unauthorized => McpError::AuthRequired,
        other => McpError::Transport(other.to_string()),
    }
}

/// Decodes a `tools/call` result the way the connector gateway renders it.
///
/// Connectors publish their own result shape, so this stays beside the MCP
/// rendering rather than inside it.
pub fn decode_tool_result(result: Value) -> Result<ToolExecutionOutput, McpError> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Tool("tools/call omitted content".to_owned()))?;
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

/// One `tools/call` over its own HTTP session, answered with the raw result:
/// the connector gateway's exchange.
pub async fn call_http_tool(
    config: &McpServerConfig,
    name: &str,
    arguments: Value,
) -> Result<Value, McpError> {
    validate_config(config)?;
    let peer = ServerPeer::new(config, None);
    let headers = authorization::http_headers(config);
    let (McpTransportConfig::Http { url, .. } | McpTransportConfig::StreamableHttp { url, .. }) =
        &config.transport
    else {
        return Err(McpError::Transport(
            "a connector call requires an HTTP transport".to_owned(),
        ));
    };
    let mut session = Session::open(
        Endpoint::Http {
            url,
            headers: &headers,
        },
        Some(peer.startup_timeout),
        None,
    )
    .await
    .map_err(session_error)?;
    let answer = session
        .call_tool(name, arguments, Some(peer.tool_timeout))
        .await;
    let terminate = answer.as_ref().err().is_none_or(Session::terminates);
    session.close(terminate).await;
    answer.map(|answer| answer.result).map_err(session_error)
}

/// The JSON-RPC answer to one `sampling/createMessage` request.
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

mod registry;
pub use registry::{
    McpAuthStatus, McpAuthorizationSink, McpRegistry, McpServerStatus, McpServerView,
};

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
            // A plaintext URL on another host is the operator's decision, taken
            // when the entry was written: `vibe mcp add --allow-insecure-http`
            // and `/mcp add` refuse it unless told otherwise, and the reference
            // connects to whatever the configuration names
            // (`vibe/core/config/models.py:367-369`).
            require_web_url(url, "server")?;
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

fn require_web_url(url: &Url, field: &str) -> Result<(), McpError> {
    if url.fragment().is_some() {
        return Err(McpError::InvalidConfig(format!(
            "{field} URL must not contain a fragment"
        )));
    }
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(McpError::InvalidConfig(format!(
            "{field} URL must use HTTP or HTTPS"
        )))
    }
}

pub(crate) fn transport_url(transport: &McpTransportConfig) -> Option<&Url> {
    match transport {
        McpTransportConfig::Http { url, .. } | McpTransportConfig::StreamableHttp { url, .. } => {
            Some(url)
        }
        McpTransportConfig::Stdio { .. } => None,
    }
}

/// The transport name an entry is configured with.
#[must_use]
pub fn transport_name(transport: &McpTransportConfig) -> &'static str {
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
mod mcp_tests;
