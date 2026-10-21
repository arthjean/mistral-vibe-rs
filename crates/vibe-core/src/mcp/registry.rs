//! The MCP servers of one session and the tools they publish.
//!
//! Reference `MCPRegistry` (`vibe/core/tools/mcp/registry.py`) resolves each
//! server's authorization, serves its descriptors from memory or from the
//! persistent cache while they are fresh, and otherwise discovers them over a
//! session of their own, rejecting and retrying once when an HTTP server
//! refuses the credential it was given. What a discovery cannot authorize is
//! published to the host as an authorization requirement; what fails for any
//! other reason is kept for the host to read once.
//!
//! The reference keeps that state in one map per concern, each keyed by the
//! server's alias. Here it is one [`ServerState`] per alias, so retiring,
//! disabling or signing a server out touches one record instead of keeping
//! eight maps in step.

use super::authorization::authorization_kind;
use super::*;
use crate::time::UtcTimestamp;

mod credentials;
mod discovery;
mod tools;

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
    #[serde(default)]
    pub disabled_tools: BTreeSet<String>,
    pub diagnostic: Option<String>,
    /// The name each published tool has on its server, keyed by the name it
    /// publishes under, which is what reference `MCPToolSummary` lists.
    #[serde(default)]
    pub remote_names: BTreeMap<String, String>,
}

/// Reference `AuthStatus`: how a server's credential stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthStatus {
    Ok,
    NeedsAuth,
    Static,
    Stdio,
}

impl McpAuthStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NeedsAuth => "needs_auth",
            Self::Static => "static",
            Self::Stdio => "stdio",
        }
    }
}

/// Where the registry publishes what a server needs before it can connect,
/// reference `MCPAuthorizationRequiredSink`.
pub type McpAuthorizationSink = Arc<dyn Fn(&str, &McpAuthorizationRequired) + Send + Sync>;

#[derive(Clone)]
struct AuthorizationSetup {
    service: Arc<McpAuthenticationService>,
    references: BTreeMap<String, McpAuthorizationRef>,
    sink: Option<McpAuthorizationSink>,
}

/// Reference `_MemoryDescriptorRecord`.
struct MemoryRecord {
    discovered_at: UtcTimestamp,
    descriptors: Vec<RemoteTool>,
}

/// What the last discovery published for one server.
struct Published {
    /// The entry it ran with, which the next discovery compares against.
    config: McpServerConfig,
    /// What the host reads of the server.
    view: McpServerView,
}

/// Everything the registry holds about one server.
struct ServerState {
    /// Absent until a discovery publishes the server, and again once its entry
    /// is retired.
    published: Option<Published>,
    /// Kept across rediscoveries while the entry does not change, which keeps
    /// a pooled stdio session alive.
    peer: Option<Arc<dyn McpPeer>>,
    /// Advanced whenever the server changes under a running call, which ends
    /// the call.
    epoch: watch::Sender<u64>,
    /// Reference `_memory_records`, by descriptor cache key.
    memory: BTreeMap<String, MemoryRecord>,
    /// Reference `_needs_auth`.
    needs_auth: bool,
    /// Reference `_descriptor_revisions`.
    descriptor_revision: Option<String>,
    /// Reference `_force_refresh`: the next discovery bypasses every cache.
    force_refresh: bool,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            published: None,
            peer: None,
            epoch: watch::channel(0).0,
            memory: BTreeMap::new(),
            needs_auth: false,
            descriptor_revision: None,
            force_refresh: false,
        }
    }
}

impl ServerState {
    /// Ends every call running against the server.
    fn end_running_calls(&self) {
        self.epoch
            .send_modify(|value| *value = value.saturating_add(1));
    }
}

#[derive(Default)]
struct McpRegistryState {
    servers: BTreeMap<String, ServerState>,
    runtime: Option<McpRuntime>,
    authorization: Option<AuthorizationSetup>,
    cache: Option<Arc<McpDescriptorCache>>,
    /// Reference `_failed`: the discovery failures the host has not read yet,
    /// which outlive the entry that failed.
    failed: BTreeMap<String, String>,
    /// The tools the last discovery published, in the order it published them.
    published_tools: Vec<String>,
}

impl McpRegistryState {
    fn server(&mut self, alias: &str) -> &mut ServerState {
        self.servers.entry(alias.to_owned()).or_default()
    }

    fn published(&self, alias: &str) -> Option<&Published> {
        self.servers.get(alias)?.published.as_ref()
    }

    fn published_mut(&mut self, alias: &str) -> Option<&mut Published> {
        self.servers.get_mut(alias)?.published.as_mut()
    }

    /// Every published server, in alias order.
    fn configured(&self) -> impl Iterator<Item = &Published> {
        self.servers
            .values()
            .filter_map(|server| server.published.as_ref())
    }

    /// Reference `_drop_alias_cache`.
    fn drop_memory(&mut self, alias: &str) {
        if let Some(server) = self.servers.get_mut(alias) {
            server.memory.clear();
        }
    }

    fn tools(&self) -> Option<ToolRegistry> {
        self.runtime.as_ref().map(|runtime| runtime.tools.clone())
    }
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
    mutation: Arc<Mutex<()>>,
}

impl McpRegistry {
    /// Reference `configure_authorization`: the service a discovery resolves
    /// credentials through, the reference each server was resolved to by the
    /// catalog, and where authorization requirements are published.
    pub async fn configure_authorization(
        &self,
        service: Arc<McpAuthenticationService>,
        references: BTreeMap<String, McpAuthorizationRef>,
        sink: Option<McpAuthorizationSink>,
    ) {
        self.state.lock().await.authorization = Some(AuthorizationSetup {
            service,
            references,
            sink,
        });
    }

    /// The persistent descriptor cache discoveries read and write.
    pub async fn configure_descriptor_cache(&self, cache: Option<Arc<McpDescriptorCache>>) {
        self.state.lock().await.cache = cache;
    }

    pub async fn discover_all(
        &self,
        configs: Vec<McpServerConfig>,
        factory: Arc<dyn McpPeerFactory>,
        tools: &ToolRegistry,
        policy: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
    ) -> Vec<String> {
        let _mutation = self.mutation.lock().await;
        let runtime = McpRuntime {
            factory,
            tools: tools.clone(),
            policy,
            approval,
        };
        self.state.lock().await.runtime = Some(runtime.clone());
        let mut diagnostics = Vec::new();
        let exceeded_limit = configs.len() > MAX_MCP_SERVERS;
        let mut aliases = BTreeSet::new();
        let mut accepted = Vec::new();
        for config in configs.into_iter().take(MAX_MCP_SERVERS) {
            if !aliases.insert(config.alias.clone()) {
                let alias = config.alias.chars().take(128).collect::<String>();
                diagnostics.push(crate::integrations::redact(&format!(
                    "MCP `{alias}`: duplicate server alias was ignored"
                )));
                continue;
            }
            accepted.push(config);
        }
        if exceeded_limit {
            diagnostics.push(format!(
                "MCP registry: server count exceeds limit of {MAX_MCP_SERVERS}"
            ));
        }
        diagnostics.extend(self.discover_locked(accepted, &runtime).await);
        diagnostics.sort();
        diagnostics
    }

    pub async fn read(&self) -> Vec<McpServerView> {
        self.state
            .lock()
            .await
            .configured()
            .map(|published| published.view.clone())
            .collect()
    }

    /// The tools the last discovery published, in the order it published them.
    pub async fn published(&self) -> Vec<String> {
        self.state.lock().await.published_tools.clone()
    }

    /// Reference `needs_auth`.
    pub async fn needs_auth(&self) -> BTreeSet<String> {
        self.state
            .lock()
            .await
            .servers
            .iter()
            .filter(|(_, server)| server.needs_auth)
            .map(|(alias, _)| alias.clone())
            .collect()
    }

    /// Reference `descriptor_revision`.
    pub async fn descriptor_revision(&self, alias: &str) -> String {
        self.state
            .lock()
            .await
            .servers
            .get(alias)
            .and_then(|server| server.descriptor_revision.clone())
            .unwrap_or_default()
    }

    /// Reference `pop_failed`: the discovery failures since the last read.
    pub async fn pop_failed(&self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.state.lock().await.failed)
    }

    /// Reference `status`.
    pub async fn auth_status(&self) -> BTreeMap<String, McpAuthStatus> {
        let state = self.state.lock().await;
        state
            .servers
            .iter()
            .filter_map(|(alias, server)| {
                let published = server.published.as_ref()?;
                let status = match authorization_kind(&published.config) {
                    McpAuthorizationKind::None => McpAuthStatus::Stdio,
                    McpAuthorizationKind::Static => McpAuthStatus::Static,
                    McpAuthorizationKind::Oauth if server.needs_auth => McpAuthStatus::NeedsAuth,
                    McpAuthorizationKind::Oauth => McpAuthStatus::Ok,
                };
                Some((alias.clone(), status))
            })
            .collect()
    }

    /// Reference `invalidate`: the next discovery reaches the server.
    pub async fn invalidate(&self, alias: &str) {
        let mut state = self.state.lock().await;
        let server = state.server(alias);
        server.memory.clear();
        server.force_refresh = true;
    }

    pub async fn preflight_add(&self, config: &McpServerConfig) -> Result<(), McpError> {
        validate_config(config)?;
        let state = self.state.lock().await;
        if state.published(&config.alias).is_some() {
            return Err(McpError::InvalidConfig(format!(
                "MCP server name `{}` is already registered",
                config.alias
            )));
        }
        let Some(url) = transport_url(&config.transport) else {
            return Ok(());
        };
        if state.configured().any(|existing| {
            transport_url(&existing.config.transport)
                .is_some_and(|existing| canonical_url(existing) == canonical_url(url))
        }) {
            return Err(McpError::InvalidConfig(
                "an MCP server with this URL is already registered".to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn config(&self, alias: &str) -> Result<McpServerConfig, McpError> {
        self.state
            .lock()
            .await
            .published(alias)
            .map(|published| published.config.clone())
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))
    }

    /// Every configured server, in alias order.
    pub async fn configs(&self) -> Vec<McpServerConfig> {
        self.state
            .lock()
            .await
            .configured()
            .map(|published| published.config.clone())
            .collect()
    }

    pub async fn toggle(&self, alias: &str, enabled: bool) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        if enabled {
            return self.rediscover_locked(alias, true).await;
        }
        let unknown = || McpError::UnknownServer(alias.to_owned());
        let (tool_names, tools) = {
            let state = self.state.lock().await;
            let published = state.published(alias).ok_or_else(unknown)?;
            (published.view.tools.clone(), state.tools())
        };
        if let Some(tools) = tools
            && !set_all(
                &tools,
                ToolSource::Mcp,
                &tool_names,
                ToolAvailability::Disabled,
            )
        {
            return Err(McpError::Tool(
                "MCP tool registry changed during server disable".to_owned(),
            ));
        }
        let peer = {
            let mut state = self.state.lock().await;
            let server = state.servers.get_mut(alias).ok_or_else(unknown)?;
            let published = server.published.as_mut().ok_or_else(unknown)?;
            published.config.enabled = false;
            published.view.enabled = false;
            published.view.status = McpServerStatus::Disabled;
            published.view.diagnostic = None;
            server.end_running_calls();
            server.peer.take()
        };
        if let Some(peer) = peer
            && let Err(error) = within(peer.close(), MCP_OPERATION_TIMEOUT).await
        {
            self.set_diagnostic(alias, canonical_diagnostic(alias, &error))
                .await?;
        }
        self.view(alias).await
    }

    pub async fn toggle_tool(
        &self,
        alias: &str,
        tool_name: &str,
        enabled: bool,
    ) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let mut state = self.state.lock().await;
        let tools = state.tools().ok_or(McpError::ReconnectRequired)?;
        let published = state
            .published_mut(alias)
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
        let view = &mut published.view;
        if !view.enabled || view.status != McpServerStatus::Healthy {
            return Err(McpError::Disabled);
        }
        if !view.tools.iter().any(|candidate| candidate == tool_name) {
            return Err(McpError::Tool(format!(
                "MCP server `{alias}` has no tool `{tool_name}`"
            )));
        }
        let availability = tool_availability(enabled, &BTreeSet::new(), tool_name);
        if !tools.set_availability(tool_name, ToolSource::Mcp, availability) {
            return Err(McpError::Tool(format!(
                "MCP tool `{tool_name}` is no longer registered"
            )));
        }
        if enabled {
            view.disabled_tools.remove(tool_name);
        } else {
            view.disabled_tools.insert(tool_name.to_owned());
        }
        // The entry carries the preference too, so a rediscovery of this
        // server republishes the tool as the operator left it.
        published
            .config
            .disabled_tools
            .clone_from(&published.view.disabled_tools);
        Ok(published.view.clone())
    }

    pub async fn clear_auth(&self, alias: &str) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let unknown = || McpError::UnknownServer(alias.to_owned());
        let (tool_names, tools, availability) = {
            let mut state = self.state.lock().await;
            state.drop_memory(alias);
            let tools = state.tools();
            let server = state.servers.get_mut(alias).ok_or_else(unknown)?;
            let view = &mut server.published.as_mut().ok_or_else(unknown)?.view;
            view.status = if view.enabled {
                McpServerStatus::AuthRequired
            } else {
                McpServerStatus::Disabled
            };
            view.diagnostic = None;
            let availability = if view.enabled {
                ToolAvailability::Unavailable
            } else {
                ToolAvailability::Disabled
            };
            let tool_names = view.tools.clone();
            server.end_running_calls();
            (tool_names, tools, availability)
        };
        let mut cleanup_diagnostic = None;
        if let Some(tools) = tools {
            for tool_name in tool_names {
                if !tools.set_availability(&tool_name, ToolSource::Mcp, availability) {
                    cleanup_diagnostic = Some(format!(
                        "MCP `{alias}`: tool registry changed during authentication cleanup"
                    ));
                }
            }
        }
        if let Some(diagnostic) = cleanup_diagnostic {
            self.set_diagnostic(alias, diagnostic).await?;
        }
        self.view(alias).await
    }

    /// Reaches one server again, bypassing every cache of its descriptors.
    pub async fn refresh(&self, alias: &str) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let enabled = self
            .state
            .lock()
            .await
            .published(alias)
            .map(|published| published.view.enabled)
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
        if !enabled {
            return Err(McpError::Disabled);
        }
        self.rediscover_locked(alias, false).await
    }

    /// Forces one server's discovery and republishes the whole configuration
    /// around it, the others answering from memory.
    async fn rediscover_locked(
        &self,
        alias: &str,
        enable: bool,
    ) -> Result<McpServerView, McpError> {
        let unknown = || McpError::UnknownServer(alias.to_owned());
        let (runtime, configs) = {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.clone().ok_or(McpError::ReconnectRequired)?;
            let mut configs = state
                .configured()
                .map(|published| published.config.clone())
                .collect::<Vec<_>>();
            let config = configs
                .iter_mut()
                .find(|config| config.alias == alias)
                .ok_or_else(unknown)?;
            if enable {
                config.enabled = true;
            }
            let server = state.server(alias);
            server.memory.clear();
            server.force_refresh = true;
            (runtime, configs)
        };
        let _ = self.discover_locked(configs, &runtime).await;
        let view = self.view(alias).await?;
        match view.status {
            McpServerStatus::Healthy | McpServerStatus::Disabled => Ok(view),
            McpServerStatus::AuthRequired => Err(McpError::AuthRequired),
            McpServerStatus::Failed => Err(McpError::Transport(
                view.diagnostic
                    .unwrap_or_else(|| "MCP discovery failed".to_owned()),
            )),
        }
    }

    /// What the host reads of one published server.
    async fn view(&self, alias: &str) -> Result<McpServerView, McpError> {
        self.state
            .lock()
            .await
            .published(alias)
            .map(|published| published.view.clone())
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))
    }

    async fn set_diagnostic(&self, alias: &str, diagnostic: String) -> Result<(), McpError> {
        self.state
            .lock()
            .await
            .published_mut(alias)
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?
            .view
            .diagnostic = Some(diagnostic);
        Ok(())
    }

    pub async fn close(&self) -> Vec<String> {
        let _mutation = self.mutation.lock().await;
        let (peers, tools, tool_names) = {
            let mut state = self.state.lock().await;
            let tools = state.tools();
            let mut peers = Vec::new();
            let mut tool_names = Vec::new();
            for (alias, server) in &mut state.servers {
                if let Some(published) = &mut server.published {
                    tool_names.extend(published.view.tools.iter().cloned());
                    published.view.enabled = false;
                    published.view.status = McpServerStatus::Disabled;
                }
                server.end_running_calls();
                if let Some(peer) = server.peer.take() {
                    peers.push((alias.clone(), peer));
                }
            }
            (peers, tools, tool_names)
        };
        if let Some(tools) = tools {
            let _ = set_all(
                &tools,
                ToolSource::Mcp,
                &tool_names,
                ToolAvailability::Disabled,
            );
        }
        let mut diagnostics = Vec::new();
        for (alias, peer) in peers {
            if let Err(error) = within(peer.close(), MCP_OPERATION_TIMEOUT).await {
                diagnostics.push(canonical_diagnostic(&alias, &error));
            }
        }
        diagnostics
    }
}

/// `future`, failed as a timed out transport when it outlasts `timeout`.
async fn within<T>(future: McpFuture<'_, T>, timeout: Duration) -> Result<T, McpError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| McpError::Transport("operation timed out".to_owned()))?
}
