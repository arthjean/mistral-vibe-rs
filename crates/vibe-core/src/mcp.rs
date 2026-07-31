use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use url::Url;

#[cfg(windows)]
use command_group::{AsyncCommandGroup, AsyncGroupChild};
#[cfg(not(windows))]
use tokio::process::Child;

use crate::policy::{ApprovalAgent, PermissionRequirement, PermissionStore, PolicyGuardedTool};
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

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

#[cfg(windows)]
type StdioChild = AsyncGroupChild;
#[cfg(not(windows))]
type StdioChild = Child;

pub type McpFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, McpError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum McpTransportConfig {
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
    #[serde(default)]
    pub oauth: Option<McpOAuthConfig>,
}

const fn default_startup_timeout_ms() -> u64 {
    DEFAULT_MCP_STARTUP_TIMEOUT_MS
}

const fn default_tool_timeout_ms() -> u64 {
    DEFAULT_MCP_TOOL_TIMEOUT_MS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    pub resource: Url,
    pub issuer: Url,
    pub client_id: String,
    pub redirect_uri: Url,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl McpOAuthConfig {
    pub fn validate(&self) -> Result<(), McpError> {
        require_secure_url(&self.resource, "resource")?;
        require_secure_url(&self.issuer, "issuer")?;
        if self.client_id.is_empty() {
            return Err(McpError::InvalidConfig(
                "OAuth client ID is empty".to_owned(),
            ));
        }
        let redirect_valid = self.redirect_uri.scheme() == "https"
            || (self.redirect_uri.scheme() == "http"
                && self.redirect_uri.host_str().is_some_and(is_loopback_host));
        if !redirect_valid {
            return Err(McpError::InvalidConfig(
                "OAuth redirect must use HTTPS or loopback HTTP".to_owned(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut scopes = self.scopes.clone();
        scopes.sort();
        let payload = format!(
            "{}\n{}\n{}\n{}\n{}",
            canonical_resource(&self.resource),
            self.issuer,
            self.client_id,
            self.redirect_uri,
            scopes.join(" ")
        );
        format!("sha256:{}", hex::encode(Sha256::digest(payload.as_bytes())))
    }
}

#[derive(Clone)]
pub struct StoredOAuthToken {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub audience: Url,
    pub issuer: Url,
    pub expires_at: u64,
    pub fingerprint: String,
}

pub trait TokenStore: Send + Sync {
    fn load(&self, alias: &str) -> Result<Option<StoredOAuthToken>, McpError>;
    fn save(&self, alias: &str, token: StoredOAuthToken) -> Result<(), McpError>;
    fn delete(&self, alias: &str) -> Result<(), McpError>;
}

pub struct McpOAuthManager {
    storage: Arc<dyn TokenStore>,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl McpOAuthManager {
    #[must_use]
    pub fn new(storage: Arc<dyn TokenStore>) -> Self {
        Self {
            storage,
            locks: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn save(
        &self,
        alias: &str,
        config: &McpOAuthConfig,
        token: StoredOAuthToken,
    ) -> Result<(), McpError> {
        config.validate()?;
        self.validate_token(config, &token, 0)?;
        let lock = self.alias_lock(alias).await?;
        let _guard = lock.lock().await;
        self.storage.save(alias, token)
    }

    pub async fn load_valid(
        &self,
        alias: &str,
        config: &McpOAuthConfig,
        now: u64,
    ) -> Result<StoredOAuthToken, McpError> {
        config.validate()?;
        let lock = self.alias_lock(alias).await?;
        let _guard = lock.lock().await;
        let token = self.storage.load(alias)?.ok_or(McpError::AuthRequired)?;
        if let Err(error) = self.validate_token(config, &token, now) {
            self.storage.delete(alias)?;
            return Err(error);
        }
        Ok(token)
    }

    pub async fn logout(&self, alias: &str) -> Result<(), McpError> {
        let lock = self.alias_lock(alias).await?;
        let _guard = lock.lock().await;
        self.storage.delete(alias)
    }

    pub async fn request_headers(
        &self,
        alias: &str,
        config: &McpServerConfig,
        now: u64,
    ) -> Result<BTreeMap<String, SecretString>, McpError> {
        validate_config(config)?;
        if alias != config.alias {
            return Err(McpError::InvalidConfig(
                "credential alias does not match the MCP server alias".to_owned(),
            ));
        }
        match &config.oauth {
            Some(oauth) => {
                let token = self.load_valid(alias, oauth, now).await?;
                Ok(BTreeMap::from([(
                    "Authorization".to_owned(),
                    SecretString::from(format!("Bearer {}", token.access_token.expose_secret())),
                )]))
            }
            None => Ok(static_headers(&config.transport)
                .into_iter()
                .map(|(name, value)| (name, SecretString::from(value)))
                .collect()),
        }
    }

    async fn alias_lock(&self, alias: &str) -> Result<Arc<Mutex<()>>, McpError> {
        if alias.is_empty() || alias.len() > 128 || sanitize_name(alias) != alias {
            return Err(McpError::InvalidConfig(
                "invalid credential alias".to_owned(),
            ));
        }
        let mut locks = self.locks.lock().await;
        if let Some(lock) = locks.get(alias) {
            return Ok(lock.clone());
        }
        if locks.len() >= MAX_MCP_SERVERS {
            return Err(McpError::RegistryFull(MAX_MCP_SERVERS));
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(alias.to_owned(), lock.clone());
        Ok(lock)
    }

    fn validate_token(
        &self,
        config: &McpOAuthConfig,
        token: &StoredOAuthToken,
        now: u64,
    ) -> Result<(), McpError> {
        if canonical_resource(&token.audience) != canonical_resource(&config.resource) {
            return Err(McpError::AudienceMismatch);
        }
        if token.issuer != config.issuer {
            return Err(McpError::IssuerMismatch);
        }
        if token.fingerprint != config.fingerprint() {
            return Err(McpError::FingerprintMismatch);
        }
        if now > 0 && token.expires_at <= now {
            return Err(McpError::TokenExpired);
        }
        Ok(())
    }
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

pub trait McpPeer: Send + Sync {
    fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>>;
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        max_response_bytes: usize,
        output: ToolOutputSink,
    ) -> McpFuture<'a, Vec<u8>>;
    fn refresh<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>>;
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
            let peer = StdioMcpPeer::connect(config).await?;
            Ok(Arc::new(peer) as Arc<dyn McpPeer>)
        })
    }
}

struct StdioMcpPeer {
    state: Mutex<StdioMcpState>,
}

struct StdioMcpState {
    child: StdioChild,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: JoinHandle<()>,
    next_request_id: u64,
    process_group: Option<i32>,
    closed: bool,
}

impl StdioMcpPeer {
    async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
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
        command_builder
            .args(arguments)
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(working_directory) = working_directory {
            command_builder.current_dir(working_directory);
        }
        #[cfg(unix)]
        command_builder.process_group(0);

        #[cfg(not(windows))]
        let mut child = command_builder
            .spawn()
            .map_err(|error| McpError::Transport(format!("cannot launch `{command}`: {error}")))?;
        #[cfg(windows)]
        let mut child = {
            let mut group = command_builder.group();
            group.kill_on_drop(true);
            group.spawn().map_err(|error| {
                McpError::Transport(format!("cannot launch `{command}`: {error}"))
            })?
        };
        let process_group = child.id().and_then(|id| i32::try_from(id).ok());
        #[cfg(not(windows))]
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        #[cfg(windows)]
        let (stdin, stdout, stderr) = {
            let inner = child.inner();
            (inner.stdin.take(), inner.stdout.take(), inner.stderr.take())
        };
        let stdin =
            stdin.ok_or_else(|| McpError::Transport("stdio stdin is missing".to_owned()))?;
        let stdout =
            stdout.ok_or_else(|| McpError::Transport("stdio stdout is missing".to_owned()))?;
        let stderr =
            stderr.ok_or_else(|| McpError::Transport("stdio stderr is missing".to_owned()))?;
        let stderr = tokio::spawn(drain_stderr(stderr));
        let peer = Self {
            state: Mutex::new(StdioMcpState {
                child,
                stdin: Some(stdin),
                stdout: BufReader::new(stdout),
                stderr,
                next_request_id: 1,
                process_group,
                closed: false,
            }),
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
                    "capabilities": {},
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
                reply_method_not_found(&mut state, &message).await?;
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
            let message = if model_text.is_empty() {
                "remote MCP tool reported an error".to_owned()
            } else {
                crate::integrations::redact(&model_text)
            };
            return Err(McpError::Tool(message));
        }
        let typed_result = result
            .get("structuredContent")
            .cloned()
            .unwrap_or_else(|| json!({"content": content}));
        Ok(ToolExecutionOutput {
            typed_result,
            model_text,
            display: json!({
                "kind": "mcp",
                "isError": result.get("isError").and_then(Value::as_bool).unwrap_or(false),
            }),
            chunks: Vec::new(),
        })
    }

    async fn close_transport(&self) -> Result<(), McpError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        state.stdin.take();
        let mut group_signalled = false;
        let graceful = tokio::time::timeout(MCP_CLEANUP_GRACE, wait_stdio_leader(&mut state)).await;
        match graceful {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                return Err(McpError::Transport(format!(
                    "cannot wait for stdio server: {error}"
                )));
            }
            Err(_) => {
                signal_stdio_child(&mut state, false)?;
                group_signalled = true;
                match tokio::time::timeout(MCP_CLEANUP_GRACE, wait_stdio_leader(&mut state)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        return Err(McpError::Transport(format!(
                            "cannot reap the stdio server after SIGTERM: {error}"
                        )));
                    }
                    Err(_) => {
                        signal_stdio_child(&mut state, true)?;
                        tokio::time::timeout(MCP_CLEANUP_GRACE, wait_stdio_leader(&mut state))
                            .await
                            .map_err(|_| {
                                McpError::Transport(
                                    "stdio server did not exit before the cleanup deadline"
                                        .to_owned(),
                                )
                            })?
                            .map_err(|error| {
                                McpError::Transport(format!(
                                    "cannot reap the stdio server: {error}"
                                ))
                            })?;
                    }
                }
            }
        }
        cleanup_stdio_process_group(&mut state, group_signalled).await?;
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
    fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let tools = self.list_tools(max_response_bytes).await?;
            serde_json::to_vec(&tools).map_err(|error| McpError::Tool(error.to_string()))
        })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        max_response_bytes: usize,
        output: ToolOutputSink,
    ) -> McpFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let result = self
                .call_tool(name, arguments, max_response_bytes, &output)
                .await?;
            let encoded =
                serde_json::to_vec(&result).map_err(|error| McpError::Tool(error.to_string()))?;
            if encoded.len() > max_response_bytes {
                return Err(McpError::Tool(
                    "MCP tool result exceeded its byte budget".to_owned(),
                ));
            }
            Ok(encoded)
        })
    }

    fn refresh<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
        self.discover(max_response_bytes)
    }

    fn close<'a>(&'a self) -> McpFuture<'a, ()> {
        Box::pin(self.close_transport())
    }
}

impl Drop for StdioMcpPeer {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.try_lock() {
            state.stdin.take();
            let _ = signal_stdio_child(&mut state, true);
            state.stderr.abort();
        }
    }
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

async fn reply_method_not_found(
    state: &mut StdioMcpState,
    request: &Value,
) -> Result<(), McpError> {
    write_message(
        state,
        &json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": {
                "code": -32601,
                "message": "Method not found",
            },
        }),
    )
    .await
}

fn bounded_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown JSON-RPC error");
    crate::integrations::redact(&message.chars().take(512).collect::<String>())
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

#[cfg(not(windows))]
async fn wait_stdio_leader(state: &mut StdioMcpState) -> std::io::Result<()> {
    state.child.wait().await.map(drop)
}

#[cfg(windows)]
async fn wait_stdio_leader(state: &mut StdioMcpState) -> std::io::Result<()> {
    state.child.inner().wait().await.map(drop)
}

#[cfg(unix)]
fn signal_stdio_child(state: &mut StdioMcpState, force: bool) -> Result<(), McpError> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let Some(group) = state.process_group else {
        return state
            .child
            .start_kill()
            .map_err(|error| McpError::Transport(error.to_string()));
    };
    let signal = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    match killpg(Pid::from_raw(group), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(McpError::Transport(format!(
            "cannot signal stdio process group: {error}"
        ))),
    }
}

#[cfg(unix)]
async fn cleanup_stdio_process_group(
    state: &mut StdioMcpState,
    _group_signalled: bool,
) -> Result<(), McpError> {
    if !stdio_process_group_alive(state)? {
        return Ok(());
    }
    signal_stdio_child(state, false)?;
    if wait_for_stdio_process_group(state).await? {
        return Ok(());
    }
    signal_stdio_child(state, true)?;
    if wait_for_stdio_process_group(state).await? {
        return Ok(());
    }
    Err(McpError::Transport(
        "stdio process group did not exit before the cleanup deadline".to_owned(),
    ))
}

#[cfg(unix)]
async fn wait_for_stdio_process_group(state: &StdioMcpState) -> Result<bool, McpError> {
    let deadline = tokio::time::Instant::now() + MCP_CLEANUP_GRACE;
    loop {
        if !stdio_process_group_alive(state)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn stdio_process_group_alive(state: &StdioMcpState) -> Result<bool, McpError> {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Some(group) = state.process_group else {
        return Ok(false);
    };
    match kill(Pid::from_raw(-group), None) {
        Ok(()) | Err(Errno::EPERM) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(McpError::Transport(format!(
            "cannot inspect stdio process group: {error}"
        ))),
    }
}

#[cfg(windows)]
fn signal_stdio_child(state: &mut StdioMcpState, _force: bool) -> Result<(), McpError> {
    state
        .child
        .start_kill()
        .map_err(|error| McpError::Transport(error.to_string()))
}

#[cfg(windows)]
async fn cleanup_stdio_process_group(
    state: &mut StdioMcpState,
    group_signalled: bool,
) -> Result<(), McpError> {
    if !group_signalled {
        signal_stdio_child(state, true)?;
    }
    tokio::time::timeout(MCP_CLEANUP_GRACE, state.child.wait())
        .await
        .map_err(|_| {
            McpError::Transport(
                "stdio process job did not exit before the cleanup deadline".to_owned(),
            )
        })?
        .map(drop)
        .map_err(|error| McpError::Transport(format!("cannot reap stdio process job: {error}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    Healthy,
    Disabled,
    Failed,
    AuthRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerView {
    pub alias: String,
    pub transport: String,
    pub enabled: bool,
    pub status: McpServerStatus,
    pub tools: Vec<String>,
    pub diagnostic: Option<String>,
}

#[derive(Default)]
struct McpRegistryState {
    configs: BTreeMap<String, McpServerConfig>,
    views: BTreeMap<String, McpServerView>,
    peers: BTreeMap<String, Arc<dyn McpPeer>>,
    epochs: BTreeMap<String, watch::Sender<u64>>,
    runtime: Option<McpRuntime>,
}

#[derive(Clone)]
struct McpRuntime {
    factory: Arc<dyn McpPeerFactory>,
    tools: ToolRegistry,
    policy: PermissionStore,
    approval: Arc<dyn ApprovalAgent>,
}

#[derive(Clone, Default)]
pub struct McpRegistry {
    state: Arc<Mutex<McpRegistryState>>,
}

impl McpRegistry {
    pub async fn discover_all(
        &self,
        configs: Vec<McpServerConfig>,
        factory: Arc<dyn McpPeerFactory>,
        tools: &ToolRegistry,
        policy: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
    ) -> Vec<String> {
        let exceeded_limit = configs.len() > MAX_MCP_SERVERS;
        let mut pending = FuturesUnordered::new();
        let mut aliases = BTreeSet::new();
        let mut diagnostics = Vec::new();
        for config in configs.into_iter().take(MAX_MCP_SERVERS) {
            if !aliases.insert(config.alias.clone()) {
                let alias = config.alias.chars().take(128).collect::<String>();
                diagnostics.push(crate::integrations::redact(&format!(
                    "MCP `{alias}`: duplicate server alias was ignored"
                )));
                continue;
            }
            let factory = factory.clone();
            pending.push(
                async move {
                    let result = match validate_config(&config) {
                        Ok(()) if config.enabled => Ok(()),
                        Ok(()) => Err(McpError::Disabled),
                        Err(error) => Err(error),
                    };
                    let result = match result {
                        Ok(()) => match timeout_operation_for(
                            factory.connect(&config),
                            config.startup_timeout_ms,
                        )
                        .await
                        {
                            Ok(peer) => discover_peer(peer, config.startup_timeout_ms).await,
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    };
                    (config, result)
                }
                .boxed(),
            );
        }
        if exceeded_limit {
            diagnostics.push(format!(
                "MCP registry: server count exceeds limit of {MAX_MCP_SERVERS}"
            ));
        }
        while let Some((config, result)) = pending.next().await {
            let alias = config.alias.clone();
            let transport = transport_name(&config.transport).to_owned();
            diagnostics.extend(self.retire_alias(&alias).await);
            let mut state = self.state.lock().await;
            state.configs.insert(alias.clone(), config.clone());
            match result {
                Err(McpError::Disabled) => {
                    state.views.insert(
                        alias.clone(),
                        McpServerView {
                            alias,
                            transport,
                            enabled: false,
                            status: McpServerStatus::Disabled,
                            tools: Vec::new(),
                            diagnostic: None,
                        },
                    );
                }
                Err(error) => {
                    let diagnostic = canonical_diagnostic(&alias, &error);
                    diagnostics.push(diagnostic.clone());
                    state.views.insert(
                        alias.clone(),
                        McpServerView {
                            alias,
                            transport,
                            enabled: config.enabled,
                            status: if matches!(error, McpError::AuthRequired) {
                                McpServerStatus::AuthRequired
                            } else {
                                McpServerStatus::Failed
                            },
                            tools: Vec::new(),
                            diagnostic: Some(diagnostic),
                        },
                    );
                }
                Ok((peer, remote_tools)) => {
                    state
                        .epochs
                        .entry(alias.clone())
                        .or_insert_with(|| watch::channel(0).0);
                    let (registered, server_diagnostics) = register_remote_tools(
                        self.state.clone(),
                        &alias,
                        &config,
                        peer.clone(),
                        remote_tools,
                        tools,
                        policy.clone(),
                        approval.clone(),
                    );
                    diagnostics.extend(server_diagnostics.iter().cloned());
                    state.peers.insert(alias.clone(), peer);
                    state.views.insert(
                        alias.clone(),
                        McpServerView {
                            alias,
                            transport,
                            enabled: true,
                            status: McpServerStatus::Healthy,
                            tools: registered,
                            diagnostic: server_diagnostics.first().cloned(),
                        },
                    );
                }
            }
        }
        self.state.lock().await.runtime = Some(McpRuntime {
            factory,
            tools: tools.clone(),
            policy,
            approval,
        });
        diagnostics.sort();
        diagnostics
    }

    async fn retire_alias(&self, alias: &str) -> Vec<String> {
        let (peer, tool_names, tools) = {
            let mut state = self.state.lock().await;
            if let Some(epoch) = state.epochs.get(alias) {
                epoch.send_modify(|value| *value = value.saturating_add(1));
            }
            let tool_names = state
                .views
                .remove(alias)
                .map(|view| view.tools)
                .unwrap_or_default();
            state.configs.remove(alias);
            let tools = state.runtime.as_ref().map(|runtime| runtime.tools.clone());
            (state.peers.remove(alias), tool_names, tools)
        };
        if let Some(tools) = tools {
            for tool_name in tool_names {
                let _ = tools.set_availability(
                    &tool_name,
                    ToolSource::Mcp,
                    ToolAvailability::Unavailable,
                );
            }
        }
        match peer {
            Some(peer) => timeout_operation(peer.close())
                .await
                .err()
                .map(|error| canonical_diagnostic(alias, &error))
                .into_iter()
                .collect(),
            None => Vec::new(),
        }
    }

    pub async fn read(&self) -> Vec<McpServerView> {
        self.state.lock().await.views.values().cloned().collect()
    }

    pub async fn toggle(&self, alias: &str, enabled: bool) -> Result<McpServerView, McpError> {
        if enabled {
            return self.reconnect(alias).await;
        }
        let (peer, tool_names, tools) = {
            let mut state = self.state.lock().await;
            let config = state
                .configs
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            config.enabled = enabled;
            let view = state
                .views
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            view.enabled = enabled;
            view.status = McpServerStatus::Disabled;
            view.diagnostic = None;
            let tool_names = view.tools.clone();
            if let Some(epoch) = state.epochs.get(alias) {
                epoch.send_modify(|value| *value = value.saturating_add(1));
            }
            let tools = state.runtime.as_ref().map(|runtime| runtime.tools.clone());
            (state.peers.remove(alias), tool_names, tools)
        };
        if let Some(tools) = tools {
            for tool_name in tool_names {
                tools
                    .set_availability(&tool_name, ToolSource::Mcp, ToolAvailability::Disabled)
                    .map_err(|error| McpError::Tool(error.to_string()))?;
            }
        }
        if let Some(peer) = peer {
            timeout_operation(peer.close()).await?;
        }
        self.state
            .lock()
            .await
            .views
            .get(alias)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))
    }

    pub async fn refresh(&self, alias: &str) -> Result<McpServerView, McpError> {
        let (peer, config, runtime, old_tools) = {
            let state = self.state.lock().await;
            let view = state
                .views
                .get(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            if !view.enabled || view.status != McpServerStatus::Healthy {
                return Err(McpError::Disabled);
            }
            (
                state
                    .peers
                    .get(alias)
                    .cloned()
                    .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?,
                state
                    .configs
                    .get(alias)
                    .cloned()
                    .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?,
                state.runtime.clone().ok_or(McpError::ReconnectRequired)?,
                view.tools.clone(),
            )
        };
        let remote = timeout_operation_for(
            peer.refresh(MAX_MCP_DISCOVERY_BYTES),
            config.tool_timeout_ms,
        )
        .await
        .and_then(decode_remote_tools)?;
        for tool_name in old_tools {
            runtime
                .tools
                .set_availability(&tool_name, ToolSource::Mcp, ToolAvailability::Unavailable)
                .map_err(|error| McpError::Tool(error.to_string()))?;
        }
        let (mut registered, diagnostics) = register_remote_tools(
            self.state.clone(),
            alias,
            &config,
            peer,
            remote,
            &runtime.tools,
            runtime.policy,
            runtime.approval,
        );
        registered.sort();
        let mut state = self.state.lock().await;
        let view = state
            .views
            .get_mut(alias)
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
        view.tools = registered;
        view.diagnostic = diagnostics.first().cloned();
        Ok(view.clone())
    }

    async fn reconnect(&self, alias: &str) -> Result<McpServerView, McpError> {
        let (config, runtime, transport) = {
            let mut state = self.state.lock().await;
            let config = {
                let config = state
                    .configs
                    .get_mut(alias)
                    .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
                config.enabled = true;
                config.clone()
            };
            let transport = transport_name(&config.transport).to_owned();
            (
                config,
                state.runtime.clone().ok_or(McpError::ReconnectRequired)?,
                transport,
            )
        };
        let result = async {
            validate_config(&config)?;
            let peer =
                timeout_operation_for(runtime.factory.connect(&config), config.startup_timeout_ms)
                    .await?;
            discover_peer(peer, config.startup_timeout_ms).await
        }
        .await;
        match result {
            Ok((peer, remote)) => {
                let (mut registered, diagnostics) = register_remote_tools(
                    self.state.clone(),
                    alias,
                    &config,
                    peer.clone(),
                    remote,
                    &runtime.tools,
                    runtime.policy,
                    runtime.approval,
                );
                registered.sort();
                let mut state = self.state.lock().await;
                let epoch = state
                    .epochs
                    .entry(alias.to_owned())
                    .or_insert_with(|| watch::channel(0).0);
                epoch.send_modify(|value| *value = value.saturating_add(1));
                state.peers.insert(alias.to_owned(), peer);
                let view = McpServerView {
                    alias: alias.to_owned(),
                    transport,
                    enabled: true,
                    status: McpServerStatus::Healthy,
                    tools: registered,
                    diagnostic: diagnostics.first().cloned(),
                };
                state.views.insert(alias.to_owned(), view.clone());
                Ok(view)
            }
            Err(error) => {
                let diagnostic = canonical_diagnostic(alias, &error);
                let mut state = self.state.lock().await;
                state.views.insert(
                    alias.to_owned(),
                    McpServerView {
                        alias: alias.to_owned(),
                        transport,
                        enabled: true,
                        status: if matches!(error, McpError::AuthRequired) {
                            McpServerStatus::AuthRequired
                        } else {
                            McpServerStatus::Failed
                        },
                        tools: Vec::new(),
                        diagnostic: Some(diagnostic),
                    },
                );
                Err(error)
            }
        }
    }

    pub async fn close(&self) -> Vec<String> {
        let (peers, tools, tool_names) = {
            let mut state = self.state.lock().await;
            let peers = state
                .peers
                .iter()
                .map(|(alias, peer)| (alias.clone(), peer.clone()))
                .collect::<Vec<_>>();
            let tools = state.runtime.as_ref().map(|runtime| runtime.tools.clone());
            let tool_names = state
                .views
                .values()
                .flat_map(|view| view.tools.iter().cloned())
                .collect::<Vec<_>>();
            for view in state.views.values_mut() {
                view.enabled = false;
                view.status = McpServerStatus::Disabled;
            }
            for epoch in state.epochs.values() {
                epoch.send_modify(|value| *value = value.saturating_add(1));
            }
            state.peers.clear();
            (peers, tools, tool_names)
        };
        if let Some(tools) = tools {
            for tool_name in tool_names {
                let _ =
                    tools.set_availability(&tool_name, ToolSource::Mcp, ToolAvailability::Disabled);
            }
        }
        let mut diagnostics = Vec::new();
        for (alias, peer) in peers {
            if let Err(error) = timeout_operation(peer.close()).await {
                diagnostics.push(canonical_diagnostic(&alias, &error));
            }
        }
        diagnostics
    }
}

#[allow(clippy::too_many_arguments)]
fn register_remote_tools(
    state: Arc<Mutex<McpRegistryState>>,
    alias: &str,
    config: &McpServerConfig,
    peer: Arc<dyn McpPeer>,
    remote_tools: Vec<RemoteTool>,
    tools: &ToolRegistry,
    policy: PermissionStore,
    approval: Arc<dyn ApprovalAgent>,
) -> (Vec<String>, Vec<String>) {
    let mut registered = Vec::new();
    let mut diagnostics = Vec::new();
    for remote in remote_tools {
        if config.disabled_tools.contains(&remote.name) {
            continue;
        }
        let public_name = format!(
            "mcp_{}_{}",
            sanitize_name(alias),
            sanitize_name(&remote.name)
        );
        if let Err(error) = validate_remote_tool(&remote, &config.transport) {
            diagnostics.push(canonical_diagnostic(alias, &error));
            continue;
        }
        let peer_handler = peer.clone();
        let remote_name = remote.name.clone();
        let live_state = state.clone();
        let live_alias = alias.to_owned();
        let tool_timeout_ms = config.tool_timeout_ms;
        let handler: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let peer = peer_handler.clone();
                let remote_name = remote_name.clone();
                let arguments = invocation.arguments.clone();
                let state = live_state.clone();
                let alias = live_alias.clone();
                Box::pin(async move {
                    let mut epoch = {
                        let state = state.lock().await;
                        let available = state.views.get(&alias).is_some_and(|view| {
                            view.enabled && view.status == McpServerStatus::Healthy
                        }) && state
                            .peers
                            .get(&alias)
                            .is_some_and(|active| Arc::ptr_eq(active, &peer));
                        if !available {
                            return Err(ToolError::Unavailable(format!(
                                "MCP server `{alias}` is disabled"
                            )));
                        }
                        state
                            .epochs
                            .get(&alias)
                            .ok_or_else(|| {
                                ToolError::Unavailable(format!("MCP server `{alias}` is disabled"))
                            })?
                            .subscribe()
                    };
                    let mut peer_guard =
                        InvocationPeerGuard::new(state.clone(), alias.clone(), peer.clone());
                    let call_result = tokio::select! {
                        biased;
                        changed = epoch.changed() => {
                            let _ = changed;
                            peer_guard.disarm();
                            return Err(ToolError::Unavailable(format!(
                                "MCP server `{alias}` changed while the tool was running"
                            )));
                        }
                        result = timeout_operation_for(
                            peer.call(
                                &remote_name,
                                arguments,
                                output.remaining_bytes(),
                                output.clone(),
                            ),
                            tool_timeout_ms,
                        ) => result,
                    };
                    let response = match call_result {
                        Ok(response) => response,
                        Err(error) => {
                            peer_guard.disarm();
                            let diagnostic = canonical_diagnostic(&alias, &error);
                            retire_failed_peer(state, alias, peer, diagnostic).await;
                            return Err(ToolError::Execution(error.to_string()));
                        }
                    };
                    if response.len() > output.remaining_bytes() {
                        peer_guard.disarm();
                        retire_failed_peer(
                            state,
                            alias,
                            peer,
                            "MCP invocation exceeded its output budget".to_owned(),
                        )
                        .await;
                        return Err(ToolError::OutputTooLarge {
                            actual: response.len(),
                            limit: output.remaining_bytes(),
                        });
                    }
                    match serde_json::from_slice(&response) {
                        Ok(result) => {
                            peer_guard.disarm();
                            Ok(result)
                        }
                        Err(error) => {
                            peer_guard.disarm();
                            retire_failed_peer(
                                state,
                                alias,
                                peer,
                                "MCP invocation returned an invalid result".to_owned(),
                            )
                            .await;
                            Err(ToolError::InvalidResult(error.to_string()))
                        }
                    }
                })
            },
        );
        let server = alias.to_owned();
        let remote_permission_name = remote.name.clone();
        let guarded = Arc::new(PolicyGuardedTool::new(
            public_name.clone(),
            policy.clone(),
            approval.clone(),
            Arc::new(move |_invocation| {
                Ok(vec![PermissionRequirement::Mcp {
                    server: server.clone(),
                    tool: remote_permission_name.clone(),
                }])
            }),
            handler,
        ));
        let spec = ToolSpec {
            name: public_name.clone(),
            description: remote.description,
            input_schema: remote.input_schema,
            output_schema: remote.output_schema,
            config: Value::Null,
            state: Value::Null,
            availability: ToolAvailability::Available,
            presentation: ToolPresentationKind::Mcp,
            source: ToolSource::Mcp,
            selection_priority: 50,
        };
        match tools.register(spec, guarded) {
            Ok(_) => registered.push(public_name),
            Err(error) => diagnostics.push(canonical_diagnostic(
                alias,
                &McpError::Tool(error.to_string()),
            )),
        }
    }
    (registered, diagnostics)
}

struct InvocationPeerGuard {
    state: Arc<Mutex<McpRegistryState>>,
    alias: String,
    peer: Arc<dyn McpPeer>,
    armed: bool,
}

impl InvocationPeerGuard {
    fn new(state: Arc<Mutex<McpRegistryState>>, alias: String, peer: Arc<dyn McpPeer>) -> Self {
        Self {
            state,
            alias,
            peer,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InvocationPeerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = self.state.clone();
        let alias = self.alias.clone();
        let peer = self.peer.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                retire_failed_peer(
                    state,
                    alias,
                    peer,
                    "MCP invocation was cancelled".to_owned(),
                )
                .await;
            });
        }
    }
}

async fn retire_failed_peer(
    state: Arc<Mutex<McpRegistryState>>,
    alias: String,
    peer: Arc<dyn McpPeer>,
    diagnostic: String,
) {
    let (tool_names, tools, removed) = {
        let mut state = state.lock().await;
        let is_active = state
            .peers
            .get(&alias)
            .is_some_and(|active| Arc::ptr_eq(active, &peer));
        if !is_active {
            return;
        }
        if let Some(epoch) = state.epochs.get(&alias) {
            epoch.send_modify(|value| *value = value.saturating_add(1));
        }
        let tool_names = state
            .views
            .get_mut(&alias)
            .map(|view| {
                view.status = McpServerStatus::Failed;
                view.diagnostic = Some(diagnostic);
                view.tools.clone()
            })
            .unwrap_or_default();
        let tools = state.runtime.as_ref().map(|runtime| runtime.tools.clone());
        let removed = state.peers.remove(&alias).is_some();
        (tool_names, tools, removed)
    };
    if let Some(tools) = tools {
        for tool_name in tool_names {
            let _ =
                tools.set_availability(&tool_name, ToolSource::Mcp, ToolAvailability::Unavailable);
        }
    }
    if removed {
        let _ = timeout_operation(peer.close()).await;
    }
}

async fn timeout_operation<T>(future: McpFuture<'_, T>) -> Result<T, McpError> {
    tokio::time::timeout(MCP_OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| McpError::Transport("operation timed out".to_owned()))?
}

async fn timeout_operation_for<T>(
    future: McpFuture<'_, T>,
    timeout_ms: u64,
) -> Result<T, McpError> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), future)
        .await
        .map_err(|_| McpError::Transport("operation timed out".to_owned()))?
}

async fn discover_peer(
    peer: Arc<dyn McpPeer>,
    timeout_ms: u64,
) -> Result<(Arc<dyn McpPeer>, Vec<RemoteTool>), McpError> {
    match timeout_operation_for(peer.discover(MAX_MCP_DISCOVERY_BYTES), timeout_ms)
        .await
        .and_then(decode_remote_tools)
    {
        Ok(remote) => Ok((peer, remote)),
        Err(discovery_error) => match timeout_operation(peer.close()).await {
            Ok(()) => Err(discovery_error),
            Err(cleanup_error) => Err(McpError::Transport(format!(
                "{discovery_error}; cleanup failed: {cleanup_error}"
            ))),
        },
    }
}

fn decode_remote_tools(response: Vec<u8>) -> Result<Vec<RemoteTool>, McpError> {
    if response.len() > MAX_MCP_DISCOVERY_BYTES {
        return Err(McpError::Tool(format!(
            "MCP discovery response exceeds {MAX_MCP_DISCOVERY_BYTES} bytes"
        )));
    }
    let tools = serde_json::from_slice::<Vec<RemoteTool>>(&response)
        .map_err(|error| McpError::Tool(format!("invalid MCP discovery response: {error}")))?;
    if tools.len() > MAX_MCP_TOOLS_PER_SERVER {
        return Err(McpError::Tool(format!(
            "MCP discovery returned more than {MAX_MCP_TOOLS_PER_SERVER} tools"
        )));
    }
    Ok(tools)
}

pub fn rejected_root_claims(claims: &[PathBuf], authorized_roots: &[PathBuf]) -> Vec<PathBuf> {
    let roots = authorized_roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let mut rejected = claims
        .iter()
        .filter_map(|claim| match std::fs::canonicalize(claim) {
            Ok(canonical) => {
                (!roots.iter().any(|root| canonical.starts_with(root))).then_some(canonical)
            }
            Err(_) => Some(claim.clone()),
        })
        .collect::<Vec<_>>();
    rejected.sort();
    rejected.dedup();
    rejected
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    InvalidConfig(String),
    #[error("MCP server is disabled")]
    Disabled,
    #[error("MCP authorization is required")]
    AuthRequired,
    #[error("OAuth token audience mismatch")]
    AudienceMismatch,
    #[error("OAuth token issuer mismatch")]
    IssuerMismatch,
    #[error("OAuth configuration fingerprint mismatch")]
    FingerprintMismatch,
    #[error("OAuth token expired")]
    TokenExpired,
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
    #[error("credential storage failed")]
    CredentialStore,
}

pub fn validate_config(config: &McpServerConfig) -> Result<(), McpError> {
    if sanitize_name(&config.alias) != config.alias || config.alias.is_empty() {
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
    if let Some(oauth) = &config.oauth {
        oauth.validate()?;
        let transport_resource = match &config.transport {
            McpTransportConfig::Http { url, .. }
            | McpTransportConfig::StreamableHttp { url, .. } => Some(url),
            McpTransportConfig::Stdio { .. } => None,
        };
        if transport_resource
            .is_none_or(|url| canonical_resource(url) != canonical_resource(&oauth.resource))
        {
            return Err(McpError::InvalidConfig(
                "OAuth resource must exactly match the HTTP transport destination".to_owned(),
            ));
        }
        if static_headers(&config.transport)
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(McpError::InvalidConfig(
                "OAuth transport cannot also carry a static Authorization header".to_owned(),
            ));
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

fn static_headers(transport: &McpTransportConfig) -> BTreeMap<String, String> {
    match transport {
        McpTransportConfig::Http { headers, .. }
        | McpTransportConfig::StreamableHttp { headers, .. } => headers.clone(),
        McpTransportConfig::Stdio { .. } => BTreeMap::new(),
    }
}

fn require_secure_url(url: &Url, field: &str) -> Result<(), McpError> {
    let secure = url.scheme() == "https"
        || (url.scheme() == "http" && url.host_str().is_some_and(is_loopback_host));
    if secure {
        Ok(())
    } else {
        Err(McpError::InvalidConfig(format!(
            "{field} URL must use HTTPS or loopback HTTP"
        )))
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn canonical_resource(url: &Url) -> String {
    let mut canonical = url.clone();
    canonical.set_fragment(None);
    canonical.to_string().trim_end_matches('/').to_owned()
}

fn transport_name(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Http { .. } => "http",
        McpTransportConfig::StreamableHttp { .. } => "streamable-http",
        McpTransportConfig::Stdio { .. } => "stdio",
    }
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
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
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;
    use crate::policy::{
        ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode, PermissionRule,
    };
    use crate::tools::ToolExecutionOutput;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct MemoryTokenStore {
        tokens: StdMutex<BTreeMap<String, StoredOAuthToken>>,
        deletes: AtomicU64,
    }

    impl TokenStore for MemoryTokenStore {
        fn load(&self, alias: &str) -> Result<Option<StoredOAuthToken>, McpError> {
            Ok(self
                .tokens
                .lock()
                .map_err(|_| McpError::CredentialStore)?
                .get(alias)
                .cloned())
        }

        fn save(&self, alias: &str, token: StoredOAuthToken) -> Result<(), McpError> {
            self.tokens
                .lock()
                .map_err(|_| McpError::CredentialStore)?
                .insert(alias.to_owned(), token);
            Ok(())
        }

        fn delete(&self, alias: &str) -> Result<(), McpError> {
            self.tokens
                .lock()
                .map_err(|_| McpError::CredentialStore)?
                .remove(alias);
            self.deletes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

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

    impl McpPeer for FakePeer {
        fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
            Box::pin(async move {
                let bytes = serde_json::to_vec(&self.tools)
                    .map_err(|error| McpError::Transport(error.to_string()))?;
                if bytes.len() > max_response_bytes {
                    return Err(McpError::Transport(
                        "discovery response exceeded budget".to_owned(),
                    ));
                }
                Ok(bytes)
            })
        }

        fn call<'a>(
            &'a self,
            name: &'a str,
            arguments: Value,
            max_response_bytes: usize,
            _output: ToolOutputSink,
        ) -> McpFuture<'a, Vec<u8>> {
            Box::pin(async move {
                if self.fail_calls {
                    Err(McpError::Transport("peer crashed".to_owned()))
                } else if self.oversized {
                    Ok(vec![b' '; max_response_bytes.saturating_add(1)])
                } else {
                    let output = ToolExecutionOutput {
                        typed_result: json!({"tool": name, "arguments": arguments}),
                        model_text: format!("{name} completed"),
                        display: Value::Null,
                        chunks: Vec::new(),
                    };
                    let bytes = serde_json::to_vec(&output)
                        .map_err(|error| McpError::Transport(error.to_string()))?;
                    if bytes.len() > max_response_bytes {
                        return Err(McpError::Transport("response exceeded budget".to_owned()));
                    }
                    Ok(bytes)
                }
            })
        }

        fn refresh<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
            self.discover(max_response_bytes)
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
        fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
            Box::pin(async move {
                let bytes = serde_json::to_vec(&vec![remote_tool()])
                    .map_err(|error| McpError::Transport(error.to_string()))?;
                if bytes.len() > max_response_bytes {
                    return Err(McpError::Transport(
                        "discovery response exceeded budget".to_owned(),
                    ));
                }
                Ok(bytes)
            })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            _arguments: Value,
            _max_response_bytes: usize,
            _output: ToolOutputSink,
        ) -> McpFuture<'a, Vec<u8>> {
            Box::pin(async move {
                self.entered.notify_one();
                std::future::pending().await
            })
        }

        fn refresh<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
            self.discover(max_response_bytes)
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
            oauth: None,
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
        policy
            .add_rule(PermissionRule {
                tool: "mcp_good_search".to_owned(),
                scope: "mcp good/search".to_owned(),
                mode: PermissionMode::Always,
                rationale: "test server".to_owned(),
            })
            .await;
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
                "mcp_good_search",
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
                    "mcp_good_search",
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
        policy
            .add_rule(PermissionRule {
                tool: "mcp_good_search".to_owned(),
                scope: "mcp good/search".to_owned(),
                mode: PermissionMode::Always,
                rationale: "test server".to_owned(),
            })
            .await;
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
                        "mcp_good_search",
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
        policy
            .add_rule(PermissionRule {
                tool: "mcp_good_search".to_owned(),
                scope: "mcp good/search".to_owned(),
                mode: PermissionMode::Always,
                rationale: "test server".to_owned(),
            })
            .await;
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
                    "mcp_good_search",
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
        policy
            .add_rule(PermissionRule {
                tool: "mcp_good_search".to_owned(),
                scope: "mcp good/search".to_owned(),
                mode: PermissionMode::Always,
                rationale: "test server".to_owned(),
            })
            .await;
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
                        "mcp_good_search",
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
                    "mcp_good_search",
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

    #[tokio::test]
    async fn oauth_binds_resource_fingerprint_and_never_passes_static_token() {
        let storage = Arc::new(MemoryTokenStore::default());
        let manager = McpOAuthManager::new(storage.clone());
        let oauth = McpOAuthConfig {
            resource: Url::parse("https://mcp.example/service").expect("resource"),
            issuer: Url::parse("https://auth.example").expect("issuer"),
            client_id: "client".to_owned(),
            redirect_uri: Url::parse("http://127.0.0.1:8123/callback").expect("redirect"),
            scopes: vec!["tools".to_owned()],
        };
        manager
            .save(
                "server",
                &oauth,
                StoredOAuthToken {
                    access_token: SecretString::from("secret-token".to_owned()),
                    refresh_token: None,
                    audience: oauth.resource.clone(),
                    issuer: oauth.issuer.clone(),
                    expires_at: 100,
                    fingerprint: oauth.fingerprint(),
                },
            )
            .await
            .expect("save");
        let mut server = config("server");
        server.transport = McpTransportConfig::StreamableHttp {
            url: oauth.resource.clone(),
            headers: BTreeMap::new(),
        };
        server.oauth = Some(oauth.clone());
        let headers = manager
            .request_headers("server", &server, 50)
            .await
            .expect("headers");
        assert_eq!(headers.len(), 1);
        assert!(
            headers["Authorization"]
                .expose_secret()
                .starts_with("Bearer ")
        );
        let mut wrong = oauth;
        wrong.resource = Url::parse("https://other.example/mcp").expect("other");
        assert!(matches!(
            manager.load_valid("server", &wrong, 50).await,
            Err(McpError::AudienceMismatch | McpError::FingerprintMismatch)
        ));
        assert_eq!(storage.deletes.load(Ordering::Relaxed), 1);

        let mut redirected = server;
        redirected.transport = McpTransportConfig::StreamableHttp {
            url: Url::parse("https://attacker.example/mcp").expect("attacker"),
            headers: BTreeMap::new(),
        };
        assert!(matches!(
            manager.request_headers("server", &redirected, 50).await,
            Err(McpError::InvalidConfig(_))
        ));
    }

    #[test]
    fn roots_are_hints_and_cannot_expand_authority() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let rejected = rejected_root_claims(
            &[
                workspace.path().to_path_buf(),
                outside.path().to_path_buf(),
                outside.path().join("not-created"),
            ],
            &[workspace.path().to_path_buf()],
        );
        assert_eq!(
            rejected,
            [
                std::fs::canonicalize(outside.path()).expect("outside"),
                outside.path().join("not-created"),
            ]
        );
    }
}
