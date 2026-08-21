use super::*;

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
    mutation: Arc<Mutex<()>>,
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
        let _mutation = self.mutation.lock().await;
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
        let stale_aliases = self
            .state
            .lock()
            .await
            .configs
            .keys()
            .filter(|alias| !aliases.contains(*alias))
            .cloned()
            .collect::<Vec<_>>();
        for alias in stale_aliases {
            diagnostics.extend(self.retire_alias(&alias).await);
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
                            disabled_tools: config.disabled_tools.clone(),
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
                            disabled_tools: config.disabled_tools.clone(),
                            diagnostic: Some(diagnostic),
                        },
                    );
                }
                Ok((peer, remote_tools)) => {
                    state
                        .epochs
                        .entry(alias.clone())
                        .or_insert_with(|| watch::channel(0).0);
                    let (registered, mut server_diagnostics) = register_remote_tools(
                        self.state.clone(),
                        &alias,
                        &config,
                        peer.clone(),
                        remote_tools,
                        tools,
                        policy.clone(),
                        approval.clone(),
                    );
                    let requested_disabled =
                        normalize_disabled_tools(&alias, &registered, &config.disabled_tools);
                    let disabled_tools =
                        match reconcile_disabled_tools(tools, &registered, requested_disabled) {
                            Ok(disabled) => disabled,
                            Err(error) => {
                                server_diagnostics.push(canonical_diagnostic(&alias, &error));
                                BTreeSet::new()
                            }
                        };
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
                            disabled_tools,
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

    pub async fn toggle(&self, alias: &str, enabled: bool) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        if enabled {
            return self.reconnect_locked(alias).await;
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
        Ok(view.clone())
    }

    pub async fn clear_auth(&self, alias: &str) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let (peer, tool_names, tools, availability) = {
            let mut state = self.state.lock().await;
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
            (state.peers.remove(alias), tool_names, tools, availability)
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
        if let Some(peer) = peer
            && let Err(error) = timeout_operation(peer.close()).await
        {
            cleanup_diagnostic = Some(canonical_diagnostic(alias, &error));
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

    pub async fn refresh(&self, alias: &str) -> Result<McpServerView, McpError> {
        let _mutation = self.mutation.lock().await;
        let reconnect = {
            let state = self.state.lock().await;
            let view = state
                .views
                .get(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            if !view.enabled {
                return Err(McpError::Disabled);
            }
            view.status != McpServerStatus::Healthy
        };
        if reconnect {
            return self.reconnect_locked(alias).await;
        }
        let (peer, config, runtime, old_tools, disabled_tools) = {
            let state = self.state.lock().await;
            let view = state
                .views
                .get(alias)
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
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
                view.disabled_tools.clone(),
            )
        };
        let remote = timeout_operation_for(
            peer.discover(MAX_MCP_DISCOVERY_BYTES),
            config.tool_timeout_ms,
        )
        .await?;
        set_all(
            &runtime.tools,
            ToolSource::Mcp,
            &old_tools,
            ToolAvailability::Unavailable,
        )
        .map_err(|error| McpError::Tool(error.to_string()))?;
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
        let disabled_tools = reconcile_disabled_tools(&runtime.tools, &registered, disabled_tools)?;
        let mut state = self.state.lock().await;
        let view = state
            .views
            .get_mut(alias)
            .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
        view.tools = registered;
        view.disabled_tools = disabled_tools;
        view.diagnostic = diagnostics.first().cloned();
        Ok(view.clone())
    }

    async fn reconnect_locked(&self, alias: &str) -> Result<McpServerView, McpError> {
        let (config, runtime, transport, disabled_tools) = {
            let state = self.state.lock().await;
            let disabled_tools = state
                .views
                .get(alias)
                .map(|view| view.disabled_tools.clone())
                .unwrap_or_default();
            let mut config = state
                .configs
                .get(alias)
                .cloned()
                .ok_or_else(|| McpError::UnknownServer(alias.to_owned()))?;
            config.enabled = true;
            let transport = transport_name(&config.transport).to_owned();
            (
                config,
                state.runtime.clone().ok_or(McpError::ReconnectRequired)?,
                transport,
                disabled_tools,
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
                let disabled_tools = normalize_disabled_tools(alias, &registered, &disabled_tools);
                let disabled_tools =
                    reconcile_disabled_tools(&runtime.tools, &registered, disabled_tools)?;
                let mut state = self.state.lock().await;
                let epoch = state
                    .epochs
                    .entry(alias.to_owned())
                    .or_insert_with(|| watch::channel(0).0);
                epoch.send_modify(|value| *value = value.saturating_add(1));
                state.peers.insert(alias.to_owned(), peer);
                state.configs.insert(alias.to_owned(), config);
                let view = McpServerView {
                    alias: alias.to_owned(),
                    transport,
                    enabled: true,
                    status: McpServerStatus::Healthy,
                    tools: registered,
                    disabled_tools,
                    diagnostic: diagnostics.first().cloned(),
                };
                state.views.insert(alias.to_owned(), view.clone());
                Ok(view)
            }
            Err(error) => Err(error),
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
/// An entry may be the published name, the bare remote name, or the
/// `mcp_{alias}_{tool}` name this port published before it adopted the
/// reference rule. All three are migrated onto the current published name, so
/// a preference written by an older build keeps disabling the tool it named.
fn normalize_disabled_tools(
    alias: &str,
    registered: &[String],
    configured: &BTreeSet<String>,
) -> BTreeSet<String> {
    configured
        .iter()
        .filter_map(|tool| {
            if registered.contains(tool) {
                return Some(tool.clone());
            }
            if let Some(migrated) = tool
                .strip_prefix("mcp_")
                .filter(|migrated| registered.contains(&(*migrated).to_owned()))
            {
                return Some(migrated.to_owned());
            }
            let public = public_tool_name(ToolSource::Mcp, alias, tool);
            registered.contains(&public).then_some(public)
        })
        .collect()
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
        let public_name = public_tool_name(ToolSource::Mcp, alias, &remote.name);
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
                    peer_guard.disarm();
                    match call_result {
                        Ok(result) => Ok(result),
                        Err(error) => {
                            let diagnostic = canonical_diagnostic(&alias, &error);
                            retire_failed_peer(state, alias, peer, diagnostic).await;
                            Err(ToolError::Execution(error.to_string()))
                        }
                    }
                })
            },
        );
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
            // The server's usage hint rides on every tool it publishes, which is
            // where the model reads it.
            description: match &config.prompt {
                Some(prompt) if !prompt.is_empty() => {
                    format!("{}\nHint: {prompt}", remote.description)
                }
                _ => remote.description,
            },
            input_schema: remote.input_schema,
            output_schema: remote.output_schema,
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
        let _ = set_all(
            &tools,
            ToolSource::Mcp,
            &tool_names,
            ToolAvailability::Unavailable,
        );
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
    match timeout_operation_for(peer.discover(MAX_MCP_DISCOVERY_BYTES), timeout_ms).await {
        Ok(remote) => Ok((peer, remote)),
        Err(discovery_error) => match timeout_operation(peer.close()).await {
            Ok(()) => Err(discovery_error),
            Err(cleanup_error) => Err(McpError::Transport(format!(
                "{discovery_error}; cleanup failed: {cleanup_error}"
            ))),
        },
    }
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
}
