use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use url::Url;

use crate::policy::{ApprovalAgent, PermissionRequirement, PermissionStore, PolicyGuardedTool};
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolHandler, ToolInvocation,
    ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

const MAX_MCP_SERVERS: usize = 256;
const MAX_MCP_TOOLS_PER_SERVER: usize = 256;
const MAX_MCP_DISCOVERY_BYTES: usize = 1_048_576;
const MCP_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

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
    pub oauth: Option<McpOAuthConfig>,
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
        format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
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
    ) -> McpFuture<'a, Vec<u8>>;
    fn refresh<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>>;
    fn close<'a>(&'a self) -> McpFuture<'a, ()>;
}

pub trait McpPeerFactory: Send + Sync {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>>;
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
        self.state.lock().await.runtime = Some(McpRuntime {
            factory: factory.clone(),
            tools: tools.clone(),
            policy: policy.clone(),
            approval: approval.clone(),
        });
        let exceeded_limit = configs.len() > MAX_MCP_SERVERS;
        let mut pending = FuturesUnordered::new();
        for config in configs.into_iter().take(MAX_MCP_SERVERS) {
            let factory = factory.clone();
            pending.push(
                async move {
                    let result = match validate_config(&config) {
                        Ok(()) if config.enabled => Ok(()),
                        Ok(()) => Err(McpError::Disabled),
                        Err(error) => Err(error),
                    };
                    let result = match result {
                        Ok(()) => match timeout_operation(factory.connect(&config)).await {
                            Ok(peer) => timeout_operation(peer.discover(MAX_MCP_DISCOVERY_BYTES))
                                .await
                                .and_then(decode_remote_tools)
                                .map(|remote| (peer, remote)),
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    };
                    (config, result)
                }
                .boxed(),
            );
        }
        let mut diagnostics = exceeded_limit
            .then(|| format!("MCP registry: server count exceeds limit of {MAX_MCP_SERVERS}"))
            .into_iter()
            .collect::<Vec<_>>();
        while let Some((config, result)) = pending.next().await {
            let alias = config.alias.clone();
            let transport = transport_name(&config.transport).to_owned();
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
        diagnostics.sort();
        diagnostics
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
        let remote = timeout_operation(peer.refresh(MAX_MCP_DISCOVERY_BYTES))
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
            let peer = timeout_operation(runtime.factory.connect(&config)).await?;
            let remote = timeout_operation(peer.discover(MAX_MCP_DISCOVERY_BYTES))
                .await
                .and_then(decode_remote_tools)?;
            Ok::<_, McpError>((peer, remote))
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
                    let response = tokio::select! {
                        biased;
                        changed = epoch.changed() => {
                            let _ = changed;
                            return Err(ToolError::Unavailable(format!(
                                "MCP server `{alias}` changed while the tool was running"
                            )));
                        }
                        result = timeout_operation(peer.call(
                            &remote_name,
                            arguments,
                            output.remaining_bytes(),
                        )) => result.map_err(|error| ToolError::Execution(error.to_string()))?,
                    };
                    if response.len() > output.remaining_bytes() {
                        return Err(ToolError::OutputTooLarge {
                            actual: response.len(),
                            limit: output.remaining_bytes(),
                        });
                    }
                    serde_json::from_slice(&response)
                        .map_err(|error| ToolError::InvalidResult(error.to_string()))
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

async fn timeout_operation<T>(future: McpFuture<'_, T>) -> Result<T, McpError> {
    tokio::time::timeout(MCP_OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| McpError::Transport("operation timed out".to_owned()))?
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
