//! The MCP servers of one session and the tools they publish.
//!
//! Reference `MCPRegistry` (`vibe/core/tools/mcp/registry.py`) resolves each
//! server's authorization, serves its descriptors from memory or from the
//! persistent cache while they are fresh, and otherwise discovers them over a
//! session of their own, rejecting and retrying once when an HTTP server
//! refuses the credential it was given. What a discovery cannot authorize is
//! published to the host as an authorization requirement; what fails for any
//! other reason is kept for the host to read once.

use futures_util::future::join_all;

use super::authorization::authorization_kind;
use super::descriptor_cache::descriptor_cache_key;
use super::*;
use crate::auth::sign_in::UtcTimestamp;

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

#[derive(Default)]
struct McpRegistryState {
    configs: BTreeMap<String, McpServerConfig>,
    views: BTreeMap<String, McpServerView>,
    peers: BTreeMap<String, Arc<dyn McpPeer>>,
    epochs: BTreeMap<String, watch::Sender<u64>>,
    runtime: Option<McpRuntime>,
    authorization: Option<AuthorizationSetup>,
    cache: Option<Arc<McpDescriptorCache>>,
    memory: BTreeMap<String, MemoryRecord>,
    keys_by_alias: BTreeMap<String, BTreeSet<String>>,
    needs_auth: BTreeSet<String>,
    descriptor_revisions: BTreeMap<String, String>,
    failed: BTreeMap<String, String>,
    force_refresh: BTreeSet<String>,
    published: Vec<String>,
}

impl McpRegistryState {
    fn drop_alias_cache(&mut self, alias: &str) {
        for key in self.keys_by_alias.remove(alias).unwrap_or_default() {
            self.memory.remove(&key);
        }
    }

    fn drop_key(&mut self, key: &str, alias: &str) {
        self.memory.remove(key);
        if let Some(keys) = self.keys_by_alias.get_mut(alias) {
            keys.remove(key);
            if keys.is_empty() {
                self.keys_by_alias.remove(alias);
            }
        }
    }

    fn store(&mut self, key: &str, alias: &str, record: MemoryRecord) {
        self.memory.insert(key.to_owned(), record);
        self.keys_by_alias
            .entry(alias.to_owned())
            .or_default()
            .insert(key.to_owned());
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

/// What one server's discovery came to.
enum Outcome {
    Invalid(McpError),
    Disabled,
    Required,
    Cached(Vec<RemoteTool>),
    Discovered(Vec<RemoteTool>),
    Failed(String),
}

/// A server whose descriptors have to be fetched.
struct Pending {
    index: usize,
    peer: Arc<dyn McpPeer>,
    snapshot: McpAuthorizationSnapshot,
    reference: McpAuthorizationRef,
    key: String,
}

enum Fetched {
    Tools(Vec<RemoteTool>, McpAuthorizationSnapshot),
    Required(McpAuthorizationRequired),
    Failed(String),
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

    /// Reference `get_tools_async` over the whole configuration.
    async fn discover_locked(
        &self,
        configs: Vec<McpServerConfig>,
        runtime: &McpRuntime,
    ) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let aliases = configs
            .iter()
            .map(|config| config.alias.clone())
            .collect::<BTreeSet<_>>();
        // Reference `sync_active_servers`, plus retiring what a changed or a
        // removed entry published.
        let replaced = {
            let mut state = self.state.lock().await;
            let oauth = configs
                .iter()
                .filter(|config| authorization_kind(config) == McpAuthorizationKind::Oauth)
                .map(|config| config.alias.clone())
                .collect::<BTreeSet<_>>();
            state.needs_auth.retain(|alias| oauth.contains(alias));
            state.force_refresh.retain(|alias| aliases.contains(alias));
            let removed = state
                .configs
                .keys()
                .filter(|alias| !aliases.contains(*alias))
                .cloned()
                .collect::<Vec<_>>();
            for alias in &removed {
                state.drop_alias_cache(alias);
                state.descriptor_revisions.remove(alias);
            }
            let changed = configs
                .iter()
                .filter(|config| {
                    state
                        .configs
                        .get(&config.alias)
                        .is_some_and(|previous| previous != *config)
                })
                .map(|config| config.alias.clone())
                .collect::<Vec<_>>();
            removed.into_iter().chain(changed).collect::<Vec<_>>()
        };
        for alias in replaced {
            diagnostics.extend(self.retire_alias(&alias).await);
        }
        let now = UtcTimestamp::now();
        let mut outcomes = Vec::with_capacity(configs.len());
        let mut pending = Vec::new();
        for (index, config) in configs.iter().enumerate() {
            outcomes.push(Outcome::Disabled);
            if let Err(error) = validate_config(config) {
                outcomes[index] = Outcome::Invalid(error);
                continue;
            }
            let alias = config.alias.as_str();
            let (setup, forced) = {
                let state = self.state.lock().await;
                (
                    state.authorization.clone(),
                    state.force_refresh.contains(alias),
                )
            };
            if !config.enabled && !forced {
                let mut state = self.state.lock().await;
                if let Some(reference) =
                    setup.as_ref().and_then(|setup| setup.references.get(alias))
                {
                    state
                        .descriptor_revisions
                        .insert(alias.to_owned(), reference.descriptor_revision.clone());
                }
                state.needs_auth.remove(alias);
                continue;
            }
            let (snapshot, reference) = match resolve_authorization(config, setup.as_ref()).await {
                Ok(resolved) => resolved,
                Err(required) => {
                    self.state
                        .lock()
                        .await
                        .descriptor_revisions
                        .insert(alias.to_owned(), required.descriptor_revision.clone());
                    self.publish_required(alias, &required).await;
                    outcomes[index] = Outcome::Required;
                    continue;
                }
            };
            let key =
                descriptor_cache_key(&reference.server_fingerprint, &snapshot.descriptor_revision);
            {
                let mut state = self.state.lock().await;
                state
                    .descriptor_revisions
                    .insert(alias.to_owned(), snapshot.descriptor_revision.clone());
                if !forced {
                    if let Some(descriptors) = memory_hit(&mut state, &key, alias, now) {
                        outcomes[index] = Outcome::Cached(descriptors);
                        continue;
                    }
                    if let Some(record) = state
                        .cache
                        .clone()
                        .and_then(|cache| cache.read(&key, alias, now))
                    {
                        let descriptors = record.descriptors.clone();
                        state.store(
                            &key,
                            alias,
                            MemoryRecord {
                                discovered_at: record.discovered_at,
                                descriptors: record.descriptors,
                            },
                        );
                        state.needs_auth.remove(alias);
                        outcomes[index] = Outcome::Cached(descriptors);
                        continue;
                    }
                }
            }
            let peer = match self.peer_for(config, runtime).await {
                Ok(peer) => peer,
                Err(error) => {
                    outcomes[index] = Outcome::Invalid(error);
                    continue;
                }
            };
            pending.push(Pending {
                index,
                peer,
                snapshot,
                reference,
                key,
            });
        }
        let setup = self.state.lock().await.authorization.clone();
        let fetched = join_all(pending.iter().map(|pending| fetch(pending, setup.as_ref()))).await;
        for (pending, fetched) in pending.into_iter().zip(fetched) {
            let alias = configs[pending.index].alias.clone();
            outcomes[pending.index] = match fetched {
                Fetched::Tools(descriptors, effective) => {
                    let effective_key = descriptor_cache_key(
                        &pending.reference.server_fingerprint,
                        &effective.descriptor_revision,
                    );
                    let now = UtcTimestamp::now();
                    let mut state = self.state.lock().await;
                    state
                        .descriptor_revisions
                        .insert(alias.clone(), effective.descriptor_revision.clone());
                    state.store(
                        &effective_key,
                        &alias,
                        MemoryRecord {
                            discovered_at: now,
                            descriptors: descriptors.clone(),
                        },
                    );
                    if let Some(cache) = state.cache.clone() {
                        cache.write(&effective_key, &alias, &descriptors, now);
                    }
                    state.needs_auth.remove(&alias);
                    state.force_refresh.remove(&alias);
                    if pending.key != effective_key {
                        state.drop_key(&pending.key, &alias);
                    }
                    Outcome::Discovered(descriptors)
                }
                Fetched::Required(required) => {
                    self.publish_required(&alias, &required).await;
                    Outcome::Required
                }
                Fetched::Failed(message) => {
                    self.state
                        .lock()
                        .await
                        .failed
                        .insert(alias.clone(), message.clone());
                    Outcome::Failed(message)
                }
            };
        }
        // Reference `get_tools_async` answers the servers served from a cache
        // first and the ones it had to discover after them, each group in
        // configuration order, and that is the order the tools publish in.
        let (cached, rest): (Vec<_>, Vec<_>) = configs
            .into_iter()
            .zip(outcomes)
            .partition(|(_, outcome)| matches!(outcome, Outcome::Cached(_)));
        let mut published = Vec::new();
        for (config, outcome) in cached.into_iter().chain(rest) {
            diagnostics.extend(self.apply(config, outcome, runtime, &mut published).await);
        }
        self.state.lock().await.published = published;
        diagnostics
    }

    /// The server's peer: the one already connected when its entry did not
    /// change, which keeps a pooled stdio session across rediscoveries.
    async fn peer_for(
        &self,
        config: &McpServerConfig,
        runtime: &McpRuntime,
    ) -> Result<Arc<dyn McpPeer>, McpError> {
        {
            let state = self.state.lock().await;
            if state.configs.get(&config.alias) == Some(config)
                && let Some(peer) = state.peers.get(&config.alias)
            {
                return Ok(peer.clone());
            }
        }
        let peer =
            timeout_operation_for(runtime.factory.connect(config), config.startup_timeout_ms)
                .await?;
        let mut state = self.state.lock().await;
        state.peers.insert(config.alias.clone(), peer.clone());
        state
            .epochs
            .entry(config.alias.clone())
            .or_insert_with(|| watch::channel(0).0);
        Ok(peer)
    }

    /// Publishes what one server's discovery came to: its tools, and the view
    /// the host reads.
    async fn apply(
        &self,
        config: McpServerConfig,
        outcome: Outcome,
        runtime: &McpRuntime,
        published: &mut Vec<String>,
    ) -> Vec<String> {
        let alias = config.alias.clone();
        let transport = transport_name(&config.transport).to_owned();
        let previous = {
            let mut state = self.state.lock().await;
            state.configs.insert(alias.clone(), config.clone());
            state
                .views
                .get(&alias)
                .map(|view| view.tools.clone())
                .unwrap_or_default()
        };
        let _ = set_all(
            &runtime.tools,
            ToolSource::Mcp,
            &previous,
            ToolAvailability::Unavailable,
        );
        let mut diagnostics = Vec::new();
        let view = match outcome {
            Outcome::Cached(descriptors) | Outcome::Discovered(descriptors) => {
                let peer = self.state.lock().await.peers.get(&alias).cloned();
                let peer = match peer {
                    Some(peer) => Ok(peer),
                    None => self.peer_for(&config, runtime).await,
                };
                match peer {
                    Ok(peer) => {
                        let (published_tools, mut server_diagnostics) = register_remote_tools(
                            self.state.clone(),
                            &alias,
                            &config,
                            peer,
                            descriptors,
                            &runtime.tools,
                            runtime.policy.clone(),
                            runtime.approval.clone(),
                        );
                        let registered = published_tools
                            .iter()
                            .map(|(public, _)| public.clone())
                            .collect::<Vec<_>>();
                        published.extend(registered.iter().cloned());
                        let requested =
                            normalize_disabled_tools(&alias, &registered, &config.disabled_tools);
                        let disabled_tools = match reconcile_disabled_tools(
                            &runtime.tools,
                            &registered,
                            requested,
                        ) {
                            Ok(disabled) => disabled,
                            Err(error) => {
                                server_diagnostics.push(canonical_diagnostic(&alias, &error));
                                BTreeSet::new()
                            }
                        };
                        diagnostics.extend(server_diagnostics.iter().cloned());
                        McpServerView {
                            alias: alias.clone(),
                            transport,
                            enabled: true,
                            status: McpServerStatus::Healthy,
                            tools: registered,
                            disabled_tools,
                            diagnostic: server_diagnostics.first().cloned(),
                            remote_names: published_tools.into_iter().collect(),
                        }
                    }
                    Err(error) => {
                        let diagnostic = canonical_diagnostic(&alias, &error);
                        diagnostics.push(diagnostic.clone());
                        failed_view(
                            &config,
                            transport,
                            McpServerStatus::Failed,
                            Some(diagnostic),
                        )
                    }
                }
            }
            Outcome::Disabled => McpServerView {
                alias: alias.clone(),
                transport,
                enabled: false,
                status: McpServerStatus::Disabled,
                tools: Vec::new(),
                disabled_tools: config.disabled_tools.clone(),
                diagnostic: None,
                remote_names: BTreeMap::new(),
            },
            Outcome::Required => {
                let diagnostic = canonical_diagnostic(&alias, &McpError::AuthRequired);
                diagnostics.push(diagnostic.clone());
                failed_view(
                    &config,
                    transport,
                    McpServerStatus::AuthRequired,
                    Some(diagnostic),
                )
            }
            Outcome::Invalid(error) => {
                let diagnostic = canonical_diagnostic(&alias, &error);
                diagnostics.push(diagnostic.clone());
                failed_view(
                    &config,
                    transport,
                    McpServerStatus::Failed,
                    Some(diagnostic),
                )
            }
            Outcome::Failed(message) => {
                let diagnostic = canonical_diagnostic(&alias, &McpError::Transport(message));
                diagnostics.push(diagnostic.clone());
                failed_view(
                    &config,
                    transport,
                    McpServerStatus::Failed,
                    Some(diagnostic),
                )
            }
        };
        self.state.lock().await.views.insert(alias, view);
        diagnostics
    }

    /// Reference `_publish_authorization_required`.
    async fn publish_required(&self, alias: &str, required: &McpAuthorizationRequired) {
        let sink = {
            let mut state = self.state.lock().await;
            state.drop_alias_cache(alias);
            state.needs_auth.insert(alias.to_owned());
            state
                .descriptor_revisions
                .insert(alias.to_owned(), required.descriptor_revision.clone());
            state
                .authorization
                .as_ref()
                .and_then(|setup| setup.sink.clone())
        };
        if let Some(sink) = sink {
            sink(alias, required);
        }
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
            let _ = set_all(
                &tools,
                ToolSource::Mcp,
                &tool_names,
                ToolAvailability::Unavailable,
            );
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

    /// The tools the last discovery published, in the order it published them.
    pub async fn published(&self) -> Vec<String> {
        self.state.lock().await.published.clone()
    }

    /// Reference `needs_auth`.
    pub async fn needs_auth(&self) -> BTreeSet<String> {
        self.state.lock().await.needs_auth.clone()
    }

    /// Reference `descriptor_revision`.
    pub async fn descriptor_revision(&self, alias: &str) -> String {
        self.state
            .lock()
            .await
            .descriptor_revisions
            .get(alias)
            .cloned()
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
            .configs
            .iter()
            .map(|(alias, config)| {
                let status = match authorization_kind(config) {
                    McpAuthorizationKind::None => McpAuthStatus::Stdio,
                    McpAuthorizationKind::Static => McpAuthStatus::Static,
                    McpAuthorizationKind::Oauth if state.needs_auth.contains(alias) => {
                        McpAuthStatus::NeedsAuth
                    }
                    McpAuthorizationKind::Oauth => McpAuthStatus::Ok,
                };
                (alias.clone(), status)
            })
            .collect()
    }

    /// Reference `invalidate`: the next discovery reaches the server.
    pub async fn invalidate(&self, alias: &str) {
        let mut state = self.state.lock().await;
        state.drop_alias_cache(alias);
        state.force_refresh.insert(alias.to_owned());
    }

    pub async fn preflight_add(&self, config: &McpServerConfig) -> Result<(), McpError> {
        validate_config(config)?;
        let state = self.state.lock().await;
        if state.configs.contains_key(&config.alias) {
            return Err(McpError::InvalidConfig(format!(
                "MCP server name `{}` is already registered",
                config.alias
            )));
        }
        let Some(url) = transport_url(&config.transport) else {
            return Ok(());
        };
        if state.configs.values().any(|existing| {
            transport_url(&existing.transport)
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
            .configs
            .get(alias)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))
    }

    /// Every configured server, in alias order.
    pub async fn configs(&self) -> Vec<McpServerConfig> {
        self.state.lock().await.configs.values().cloned().collect()
    }

    pub async fn toggle(&self, alias: &str, enabled: bool) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        if enabled {
            return self.rediscover_locked(alias, true).await;
        }
        let (tool_names, tools) = {
            let state = self.state.lock().await;
            if !state.configs.contains_key(alias) || !state.views.contains_key(alias) {
                return Err(McpError::UnknownServer(alias.to_owned()));
            }
            (
                state.views[alias].tools.clone(),
                state.runtime.as_ref().map(|runtime| runtime.tools.clone()),
            )
        };
        if let Some(tools) = tools
            && !set_all(
                &tools,
                ToolSource::Mcp,
                &tool_names,
                ToolAvailability::Disabled,
            )
            .map_err(|error| McpError::Tool(error.to_string()))?
        {
            return Err(McpError::Tool(
                "MCP tool registry changed during server disable".to_owned(),
            ));
        }
        let peer = {
            let mut state = self.state.lock().await;
            state
                .configs
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?
                .enabled = false;
            let view = state
                .views
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            view.enabled = false;
            view.status = McpServerStatus::Disabled;
            view.diagnostic = None;
            if let Some(epoch) = state.epochs.get(alias) {
                epoch.send_modify(|value| *value = value.saturating_add(1));
            }
            state.peers.remove(alias)
        };
        if let Some(peer) = peer
            && let Err(error) = timeout_operation(peer.close()).await
        {
            self.state
                .lock()
                .await
                .views
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?
                .diagnostic = Some(canonical_diagnostic(alias, &error));
        }
        self.state
            .lock()
            .await
            .views
            .get(alias)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))
    }

    pub async fn toggle_tool(
        &self,
        alias: &str,
        tool_name: &str,
        enabled: bool,
    ) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let mut state = self.state.lock().await;
        let tools = state
            .runtime
            .as_ref()
            .map(|runtime| runtime.tools.clone())
            .ok_or(McpError::ReconnectRequired)?;
        let view = state
            .views
            .get_mut(alias)
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
        if !view.enabled || view.status != McpServerStatus::Healthy {
            return Err(McpError::Disabled);
        }
        if !view.tools.iter().any(|candidate| candidate == tool_name) {
            return Err(McpError::Tool(format!(
                "MCP server `{alias}` has no tool `{tool_name}`"
            )));
        }
        let availability =
            tool_availability(enabled, &BTreeSet::new(), ProviderReach::Ready, tool_name);
        if !tools
            .set_availability(tool_name, ToolSource::Mcp, availability)
            .map_err(|error| McpError::Tool(error.to_string()))?
        {
            return Err(McpError::Tool(format!(
                "MCP tool `{tool_name}` is no longer registered"
            )));
        }
        if enabled {
            view.disabled_tools.remove(tool_name);
        } else {
            view.disabled_tools.insert(tool_name.to_owned());
        }
        let view = view.clone();
        // The entry carries the preference too, so a rediscovery of this
        // server republishes the tool as the operator left it.
        if let Some(config) = state.configs.get_mut(alias) {
            config.disabled_tools.clone_from(&view.disabled_tools);
        }
        Ok(view)
    }

    pub async fn clear_auth(&self, alias: &str) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let (tool_names, tools, availability) = {
            let mut state = self.state.lock().await;
            state.drop_alias_cache(alias);
            let view = state
                .views
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
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
            if let Some(epoch) = state.epochs.get(alias) {
                epoch.send_modify(|value| *value = value.saturating_add(1));
            }
            let tools = state.runtime.as_ref().map(|runtime| runtime.tools.clone());
            (tool_names, tools, availability)
        };
        let mut cleanup_diagnostic = None;
        if let Some(tools) = tools {
            for tool_name in tool_names {
                match tools.set_availability(&tool_name, ToolSource::Mcp, availability) {
                    Ok(true) => {}
                    Ok(false) => {
                        cleanup_diagnostic = Some(format!(
                            "MCP `{alias}`: tool registry changed during authentication cleanup"
                        ));
                    }
                    Err(error) => {
                        cleanup_diagnostic = Some(canonical_diagnostic(
                            alias,
                            &McpError::Tool(error.to_string()),
                        ));
                    }
                }
            }
        }
        if let Some(diagnostic) = cleanup_diagnostic {
            self.state
                .lock()
                .await
                .views
                .get_mut(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?
                .diagnostic = Some(diagnostic);
        }
        self.state
            .lock()
            .await
            .views
            .get(alias)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))
    }

    /// Reaches one server again, bypassing every cache of its descriptors.
    pub async fn refresh(&self, alias: &str) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        {
            let state = self.state.lock().await;
            let view = state
                .views
                .get(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            if !view.enabled {
                return Err(McpError::Disabled);
            }
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
        let (runtime, configs) = {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.clone().ok_or(McpError::ReconnectRequired)?;
            let mut configs = state.configs.values().cloned().collect::<Vec<_>>();
            let config = configs
                .iter_mut()
                .find(|config| config.alias == alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            if enable {
                config.enabled = true;
            }
            state.drop_alias_cache(alias);
            state.force_refresh.insert(alias.to_owned());
            (runtime, configs)
        };
        let _ = self.discover_locked(configs, &runtime).await;
        let state = self.state.lock().await;
        let view = state
            .views
            .get(alias)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
        match view.status {
            McpServerStatus::Healthy | McpServerStatus::Disabled => Ok(view),
            McpServerStatus::AuthRequired => Err(McpError::AuthRequired),
            McpServerStatus::Failed => Err(McpError::Transport(
                view.diagnostic
                    .unwrap_or_else(|| "MCP discovery failed".to_owned()),
            )),
        }
    }

    pub async fn close(&self) -> Vec<String> {
        let _mutation = self.mutation.lock().await;
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
            let _ = set_all(
                &tools,
                ToolSource::Mcp,
                &tool_names,
                ToolAvailability::Disabled,
            );
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

fn failed_view(
    config: &McpServerConfig,
    transport: String,
    status: McpServerStatus,
    diagnostic: Option<String>,
) -> McpServerView {
    McpServerView {
        alias: config.alias.clone(),
        transport,
        enabled: config.enabled,
        status,
        tools: Vec::new(),
        disabled_tools: config.disabled_tools.clone(),
        diagnostic,
        remote_names: BTreeMap::new(),
    }
}

/// Reference `_memory_hit`: a record discovered within the TTL, which the
/// persistent cache is told was used.
fn memory_hit(
    state: &mut McpRegistryState,
    key: &str,
    alias: &str,
    now: UtcTimestamp,
) -> Option<Vec<RemoteTool>> {
    let ttl = state
        .cache
        .as_ref()
        .map_or(descriptor_cache::DESCRIPTOR_CACHE_TTL_SECONDS, |cache| {
            cache.ttl_seconds()
        });
    let record = state.memory.get(key)?;
    if ttl <= 0.0 || record.discovered_at > now || now.seconds_since(record.discovered_at) >= ttl {
        state.drop_key(key, alias);
        return None;
    }
    let (discovered_at, descriptors) = (record.discovered_at, record.descriptors.clone());
    if let Some(cache) = &state.cache {
        cache.touch(key, alias, discovered_at, now);
    }
    Some(descriptors)
}

/// Reference `_resolve_authorization`: the headers a discovery or a call of
/// this server uses, or why it has none.
async fn resolve_authorization(
    config: &McpServerConfig,
    setup: Option<&AuthorizationSetup>,
) -> Result<(McpAuthorizationSnapshot, McpAuthorizationRef), McpAuthorizationRequired> {
    if let Some(setup) = setup
        && let Some(reference) = setup.references.get(&config.alias)
    {
        return match setup.service.resolve(reference).await {
            McpAuthorization::Required(required) => Err(required),
            McpAuthorization::Snapshot(snapshot)
                if snapshot.descriptor_revision != reference.descriptor_revision =>
            {
                Err(McpAuthorizationRequired {
                    reason: McpAuthorizationReason::Invalid,
                    descriptor_revision: snapshot.descriptor_revision,
                    observed_connection_revision: None,
                })
            }
            McpAuthorization::Snapshot(snapshot) => Ok((snapshot, reference.clone())),
        };
    }
    // A registry nobody configured connects with what the entry declares, as
    // the reference's does, and has nothing to sign an OAuth entry in with.
    let kind = authorization_kind(config);
    let reference = McpAuthorizationRef {
        server_name: config.alias.clone(),
        server_fingerprint: authorization::server_fingerprint(config),
        kind,
        descriptor_revision: "legacy-static".to_owned(),
    };
    if kind == McpAuthorizationKind::Oauth {
        return Err(McpAuthorizationRequired {
            reason: McpAuthorizationReason::Missing,
            descriptor_revision: "legacy-oauth-unconfigured".to_owned(),
            observed_connection_revision: None,
        });
    }
    Ok((
        McpAuthorizationSnapshot {
            headers: authorization::http_headers(config),
            connection_revision: "legacy-static".to_owned(),
            descriptor_revision: "legacy-static".to_owned(),
        },
        reference,
    ))
}

/// Reference `authorization_required_result`.
fn required_result(
    result: McpAuthorization,
    observed_connection_revision: &str,
) -> McpAuthorizationRequired {
    match result {
        McpAuthorization::Required(required) => required,
        McpAuthorization::Snapshot(snapshot) => McpAuthorizationRequired {
            reason: McpAuthorizationReason::Rejected,
            descriptor_revision: snapshot.descriptor_revision,
            observed_connection_revision: Some(observed_connection_revision.to_owned()),
        },
    }
}

/// Reference `_discover_server`, with `_reject_and_retry_discovery` for a
/// server that refuses the credential it was given.
async fn fetch(pending: &Pending, setup: Option<&AuthorizationSetup>) -> Fetched {
    let rejected = match pending.peer.discover(&pending.snapshot.headers).await {
        Ok(descriptors) => return Fetched::Tools(descriptors, pending.snapshot.clone()),
        Err(McpError::AuthRequired) => pending.snapshot.clone(),
        Err(error) => return Fetched::Failed(failure_message(&error)),
    };
    let Some(setup) = setup else {
        return Fetched::Required(McpAuthorizationRequired {
            reason: McpAuthorizationReason::Rejected,
            descriptor_revision: rejected.descriptor_revision,
            observed_connection_revision: Some(rejected.connection_revision),
        });
    };
    let mut replacement = setup
        .service
        .reject(&pending.reference, &rejected.connection_revision)
        .await;
    if let McpAuthorization::Snapshot(retry) = &replacement
        && retry.connection_revision != rejected.connection_revision
    {
        match pending.peer.discover(&retry.headers).await {
            Ok(descriptors) => return Fetched::Tools(descriptors, retry.clone()),
            Err(McpError::AuthRequired) => {
                replacement = setup
                    .service
                    .reject(&pending.reference, &retry.connection_revision)
                    .await;
            }
            Err(error) => return Fetched::Failed(failure_message(&error)),
        }
    }
    Fetched::Required(required_result(replacement, &rejected.connection_revision))
}

fn failure_message(error: &McpError) -> String {
    crate::integrations::redact(&error.to_string())
}

fn reconcile_disabled_tools(
    tools: &ToolRegistry,
    registered: &[String],
    disabled: BTreeSet<String>,
) -> Result<BTreeSet<String>, McpError> {
    let disabled = disabled
        .into_iter()
        .filter(|tool| registered.contains(tool))
        .collect::<BTreeSet<_>>();
    let names = disabled.iter().cloned().collect::<Vec<_>>();
    if !set_all(tools, ToolSource::Mcp, &names, ToolAvailability::Disabled)
        .map_err(|error| McpError::Tool(error.to_string()))?
    {
        return Err(McpError::Tool(
            "MCP tool registry changed during reconciliation".to_owned(),
        ));
    }
    Ok(disabled)
}

/// Resolves persisted per-tool disable entries onto the names published today.
///
/// An entry names a remote tool, as reference `ToolManager._is_source_disabled`
/// reads it (`vibe/core/tools/manager.py`), and that reading wins. Only an
/// entry naming no remote tool of the server falls back to this port's own
/// spellings: the published name its toggle persists, then the
/// `mcp_{alias}_{tool}` name it published before it adopted the reference
/// rule, so a preference an older build wrote keeps disabling its tool.
fn normalize_disabled_tools(
    alias: &str,
    registered: &[String],
    configured: &BTreeSet<String>,
) -> BTreeSet<String> {
    configured
        .iter()
        .filter_map(|tool| {
            let public = public_tool_name(ToolSource::Mcp, alias, tool);
            if registered.contains(&public) {
                return Some(public);
            }
            if registered.contains(tool) {
                return Some(tool.clone());
            }
            tool.strip_prefix("mcp_")
                .filter(|migrated| registered.contains(&(*migrated).to_owned()))
                .map(str::to_owned)
        })
        .collect()
}

/// The description a published tool carries, reference
/// `create_mcp_http_proxy_tool_class` and `create_mcp_stdio_proxy_tool_class`:
/// the server's name in brackets, the tool's own description or a sentence
/// naming where it comes from, and the server's usage hint.
fn published_description(alias: &str, config: &McpServerConfig, remote: &RemoteTool) -> String {
    let description = remote
        .description
        .clone()
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| match &config.transport {
            McpTransportConfig::Stdio {
                command, arguments, ..
            } => {
                let mut argv = vec![command.as_str()];
                argv.extend(arguments.iter().map(String::as_str));
                format!(
                    "MCP tool '{}' from stdio command: {}",
                    remote.name,
                    argv.join(" ")
                )
            }
            McpTransportConfig::Http { url, .. }
            | McpTransportConfig::StreamableHttp { url, .. } => {
                let url = config
                    .declared
                    .as_ref()
                    .and_then(|declared| declared.url.clone())
                    .unwrap_or_else(|| url.to_string());
                format!("MCP tool '{}' from {url}", remote.name)
            }
        });
    let hint = config
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
        .map(|prompt| format!("\nHint: {prompt}"))
        .unwrap_or_default();
    format!("[{alias}] {description}{hint}")
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
) -> (Vec<(String, String)>, Vec<String>) {
    let mut registered = Vec::new();
    let mut diagnostics = Vec::new();
    for remote in remote_tools {
        let public_name = public_tool_name(ToolSource::Mcp, alias, &remote.name);
        if let Err(error) = validate_remote_tool(&remote, &config.transport) {
            diagnostics.push(canonical_diagnostic(alias, &error));
            continue;
        }
        let description = published_description(alias, config, &remote);
        let handler: Arc<dyn ToolHandler> = Arc::new(McpToolHandler {
            state: state.clone(),
            alias: alias.to_owned(),
            remote_name: remote.name.clone(),
            config: config.clone(),
            peer: peer.clone(),
        });
        let origin = format!("MCP server `{alias}` tool `{}`", remote.name);
        let guarded = Arc::new(PolicyGuardedTool::new(
            public_name.clone(),
            policy.clone(),
            approval.clone(),
            // The reference publishes an MCP tool through the same base class as
            // a builtin and declares no `resolve_permission` for it, so its
            // configured permission is the whole decision and an approval for
            // the session grants the tool itself. There is no fifth scope to
            // name a server under, and inventing one would be a value the Python
            // client cannot read.
            Arc::new(|_invocation| Ok(PermissionContext::deferred())),
            handler,
        ));
        let spec = ToolSpec {
            name: public_name.clone(),
            description,
            input_schema: remote.input_schema,
            // The server's output schema is checked by the client session
            // against the structured content; what the tool returns here is
            // reference `MCPToolResult`, which that schema does not describe.
            output_schema: None,
            config: Value::Null,
            state: Value::Null,
            availability: ToolAvailability::Available,
            presentation: ToolPresentationKind::Mcp,
            source: ToolSource::Mcp,
            selection_priority: 50,
        };
        match tools.register_exclusive(
            spec,
            guarded,
            origin,
            Some(crate::events::RemoteToolOrigin::mcp(&remote.name)),
        ) {
            Ok(_) => registered.push((public_name, remote.name)),
            Err(error) => diagnostics.push(canonical_diagnostic(
                alias,
                &McpError::Tool(error.to_string()),
            )),
        }
    }
    (registered, diagnostics)
}

/// One published tool, reference `MCPHttpProxyTool` or `MCPStdioProxyTool`.
struct McpToolHandler {
    state: Arc<Mutex<McpRegistryState>>,
    alias: String,
    remote_name: String,
    config: McpServerConfig,
    peer: Arc<dyn McpPeer>,
}

impl ToolHandler for McpToolHandler {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        _output: ToolOutputSink,
    ) -> crate::tools::ToolHandlerFuture<'a> {
        Box::pin(async move {
            let (mut epoch, setup) = {
                let state = self.state.lock().await;
                let available =
                    state.views.get(&self.alias).is_some_and(|view| {
                        view.enabled && view.status == McpServerStatus::Healthy
                    }) && state
                        .peers
                        .get(&self.alias)
                        .is_some_and(|active| Arc::ptr_eq(active, &self.peer));
                let disabled =
                    || ToolError::Unavailable(format!("MCP server `{}` is disabled", self.alias));
                if !available {
                    return Err(disabled());
                }
                (
                    state
                        .epochs
                        .get(&self.alias)
                        .ok_or_else(disabled)?
                        .subscribe(),
                    state.authorization.clone(),
                )
            };
            // Reference `_OpenArgs.model_dump(exclude_none=True)`.
            let arguments = match invocation.arguments.clone() {
                Value::Object(fields) => Value::Object(
                    fields
                        .into_iter()
                        .filter(|(_, value)| !value.is_null())
                        .collect(),
                ),
                other => other,
            };
            tokio::select! {
                biased;
                changed = epoch.changed() => {
                    let _ = changed;
                    Err(ToolError::Unavailable(format!(
                        "MCP server `{}` changed while the tool was running",
                        self.alias
                    )))
                }
                result = self.call(arguments, setup.as_ref()) => result,
            }
        })
    }
}

impl McpToolHandler {
    async fn call(
        &self,
        arguments: Value,
        setup: Option<&AuthorizationSetup>,
    ) -> Result<ToolExecutionOutput, ToolError> {
        let failed = |error: &McpError| {
            ToolError::Execution(crate::integrations::redact(&format!(
                "MCP server `{}` could not run `{}`: {error}",
                self.alias, self.remote_name
            )))
        };
        let reference = setup.and_then(|setup| setup.references.get(&self.alias));
        let (
            Some(setup),
            Some(reference),
            McpAuthorizationKind::Static | McpAuthorizationKind::Oauth,
        ) = (setup, reference, authorization_kind(&self.config))
        else {
            let headers = authorization::http_headers(&self.config);
            return self
                .peer
                .call(&self.remote_name, arguments, &headers)
                .await
                .map_err(|error| failed(&error));
        };
        // Reference `_call_authorized`.
        let authorization = match setup.service.resolve(reference).await {
            McpAuthorization::Snapshot(snapshot) => snapshot,
            McpAuthorization::Required(required) => {
                self.publish(setup, &required).await;
                return Err(ToolError::Execution(format!(
                    "MCP server `{}` has to be signed in to again before its tools can run",
                    self.alias
                )));
            }
        };
        match self
            .peer
            .call(&self.remote_name, arguments.clone(), &authorization.headers)
            .await
        {
            Ok(output) => return Ok(output),
            Err(McpError::AuthRequired) => {}
            Err(error) => return Err(failed(&error)),
        }
        let mut replacement = setup
            .service
            .reject(reference, &authorization.connection_revision)
            .await;
        if let McpAuthorization::Snapshot(retry) = &replacement
            && retry.connection_revision != authorization.connection_revision
        {
            match self
                .peer
                .call(&self.remote_name, arguments, &retry.headers)
                .await
            {
                Ok(output) => return Ok(output),
                Err(McpError::AuthRequired) => {
                    replacement = setup
                        .service
                        .reject(reference, &retry.connection_revision)
                        .await;
                }
                Err(error) => return Err(failed(&error)),
            }
        }
        let required = required_result(replacement, &authorization.connection_revision);
        self.publish(setup, &required).await;
        Err(ToolError::Execution(format!(
            "MCP server `{}` refused the credentials it was given",
            self.alias
        )))
    }

    /// Reference `_publish_authorization_required` from a call.
    async fn publish(&self, setup: &AuthorizationSetup, required: &McpAuthorizationRequired) {
        {
            let mut state = self.state.lock().await;
            state.drop_alias_cache(&self.alias);
            state.needs_auth.insert(self.alias.clone());
            state
                .descriptor_revisions
                .insert(self.alias.clone(), required.descriptor_revision.clone());
        }
        if let Some(sink) = &setup.sink {
            sink(&self.alias, required);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_disable_entries_resolve_onto_the_published_name() {
        let registered = vec!["docs_search".to_owned(), "docs_read".to_owned()];
        let configured = BTreeSet::from([
            // The published name, written by a current build.
            "docs_search".to_owned(),
            // The bare remote name, which the operator may type.
            "read".to_owned(),
            // The `mcp_`-prefixed name this port published before it adopted
            // the reference naming rule.
            "mcp_docs_read".to_owned(),
            // An entry naming no tool of this server.
            "absent".to_owned(),
        ]);

        assert_eq!(
            normalize_disabled_tools("docs", &registered, &configured),
            BTreeSet::from(["docs_search".to_owned(), "docs_read".to_owned()]),
            "a preference written before the rename must still disable its tool"
        );
    }

    /// An entry that is both a remote name and another tool's published name
    /// disables the remote tool it names, as the reference reads it.
    #[test]
    fn a_remote_name_outranks_the_published_name_it_collides_with() {
        let registered = vec!["docs_search".to_owned(), "docs_docs_search".to_owned()];
        let configured = BTreeSet::from(["docs_search".to_owned()]);

        assert_eq!(
            normalize_disabled_tools("docs", &registered, &configured),
            BTreeSet::from(["docs_docs_search".to_owned()])
        );
    }
}
