use super::*;
use crate::resources::backend_command::{McpAddCommand, mcp_command_alias};
use vibe_core::config::mcp::{dedupe_mcp_server_name, resolve_new_mcp_server_name};
use vibe_core::mcp::{McpAuthConfig, McpOAuthConfig};

impl CoreResourceBackend {
    /// Every source a session can call a tool through: its configured MCP
    /// servers and its connectors, in the one list `MCPState` declares.
    ///
    /// The connectors are enumerated best-effort. A catalog this session cannot
    /// reach leaves them out of the list rather than failing the read: the MCP
    /// servers are still there to publish, and a client that cannot read them
    /// loses more than one that sees no connector.
    pub(super) async fn mcp_state(&self, session: &CoreResourceSession) -> Value {
        let _ = self.ensure_connectors(session).await;
        let connectors = session.connectors.views().unwrap_or_default();
        mcp_view(session.mcp.read().await, connectors, &session.tools)
    }

    /// The integration surface the runtime snapshot publishes for a session.
    pub(super) async fn integration_state(
        &self,
        session: &CoreResourceSession,
    ) -> crate::resources::IntegrationState {
        let counts = connector_counts_value(&session.connectors.views().unwrap_or_default());
        crate::resources::IntegrationState {
            mcp: self.mcp_state(session).await,
            counts,
        }
    }

    pub(super) async fn dispatch_mcp(
        &self,
        session: &CoreResourceSession,
        session_id: &str,
        command: &McpCommand,
    ) -> Result<ResourceDispatch, ResourceError> {
        let _mutation = match command {
            McpCommand::Read => None,
            McpCommand::Add(_)
            | McpCommand::Refresh { .. }
            | McpCommand::Toggle { .. }
            | McpCommand::Login { .. }
            | McpCommand::CompleteAuth { .. }
            | McpCommand::Logout { .. } => Some(session.mcp_mutation.lock().await),
        };
        match command {
            McpCommand::Read => Ok(read_only([("mcp", self.mcp_state(session).await)])),
            McpCommand::Add(add) => {
                let factory = self.mcp_factory.clone().ok_or_else(|| {
                    ResourceError::Unavailable("MCP transport backend is not configured".to_owned())
                })?;
                let transport = match &add.transport {
                    McpAddTransport::Stdio {
                        command,
                        arguments,
                        environment,
                        working_directory,
                    } => {
                        let session_root = PathBuf::from(&session.working_directory);
                        let working_directory = working_directory
                            .as_ref()
                            .map(|path| {
                                if path.is_absolute() {
                                    path.clone()
                                } else {
                                    session_root.join(path)
                                }
                            })
                            .unwrap_or(session_root);
                        if !matches!(
                            session
                                .policy
                                .try_trust_decision(&working_directory)
                                .map_err(policy_error)?,
                            Some(TrustDecision::Trusted | TrustDecision::SessionTrusted)
                        ) {
                            return Err(ResourceError::Unavailable(
                                "workspace trust is required before launching a project MCP executable"
                                    .to_owned(),
                            ));
                        }
                        McpTransportConfig::Stdio {
                            command: command.clone(),
                            arguments: arguments.clone(),
                            environment: environment.clone(),
                            working_directory: Some(working_directory),
                        }
                    }
                    McpAddTransport::Http { url, legacy: true } => McpTransportConfig::Http {
                        url: url.clone(),
                        headers: BTreeMap::new(),
                    },
                    McpAddTransport::Http { url, legacy: false } => {
                        McpTransportConfig::StreamableHttp {
                            url: url.clone(),
                            headers: BTreeMap::new(),
                        }
                    }
                };
                let alias = resolve_add_alias(session, add, &transport).await?;
                let config = McpServerConfig {
                    alias,
                    transport,
                    enabled: add.enabled,
                    disabled_tools: Default::default(),
                    startup_timeout_ms: vibe_core::mcp::DEFAULT_MCP_STARTUP_TIMEOUT_MS,
                    tool_timeout_ms: vibe_core::mcp::DEFAULT_MCP_TOOL_TIMEOUT_MS,
                    // A remote server added here authenticates through OAuth, as
                    // it does upstream, so the entry records that intent instead
                    // of leaving the runtime to infer it.
                    auth: match &add.transport {
                        McpAddTransport::Http { .. } => McpAuthConfig::Oauth(McpOAuthConfig {
                            scopes: add.scopes.clone(),
                            ..McpOAuthConfig::default()
                        }),
                        McpAddTransport::Stdio { .. } => McpAuthConfig::default(),
                    },
                    prompt: None,
                    sampling_enabled: true,
                };
                session
                    .mcp
                    .preflight_add(&config)
                    .await
                    .map_err(mcp_error)?;
                if let Some(store) = session.config() {
                    store.preflight_mcp_add(&config).map_err(config_error)?;
                    store.persist_mcp_add(&config).map_err(config_error)?;
                    session
                        .persistent_mcp_aliases
                        .lock()
                        .await
                        .insert(config.alias.clone());
                }
                let alias = config.alias.clone();
                // A stdio source has no URL to publish, and the field is
                // required: the empty string says "not reachable by URL" the
                // way an absent one cannot.
                let url = match &config.transport {
                    McpTransportConfig::Http { url, .. }
                    | McpTransportConfig::StreamableHttp { url, .. } => url.to_string(),
                    McpTransportConfig::Stdio { .. } => String::new(),
                };
                let diagnostics = session
                    .mcp
                    .discover_all(
                        vec![config],
                        factory,
                        &session.tools,
                        session.policy.clone(),
                        self.approval.clone(),
                    )
                    .await;
                // The alias was resolved against what is already configured, so
                // reaching here means a source was created rather than reused.
                Ok(runtime_mutation(
                    [
                        ("created", json!(true)),
                        ("name", json!(alias)),
                        ("url", json!(url)),
                    ],
                    diagnostics,
                ))
            }
            McpCommand::Refresh { name } => {
                session
                    .mcp
                    .refresh(name)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                Ok(runtime_mutation([], Vec::new()))
            }
            McpCommand::Toggle {
                name,
                disabled,
                tool_name,
            } => {
                let previous = session
                    .mcp
                    .read()
                    .await
                    .into_iter()
                    .find(|view| view.alias == *name)
                    .ok_or_else(|| {
                        ResourceError::NotFound(format!("MCP source `{name}` was not found"))
                    })?;
                let mut desired = previous.clone();
                if let Some(tool_name) = tool_name {
                    if !previous.enabled || previous.status != McpServerStatus::Healthy {
                        return Err(ResourceError::Unavailable(
                            "MCP server is disabled".to_owned(),
                        ));
                    }
                    if !previous.tools.iter().any(|tool| tool == tool_name) {
                        return Err(ResourceError::NotFound(format!(
                            "MCP source `{name}` has no tool `{tool_name}`"
                        )));
                    }
                    if *disabled {
                        desired.disabled_tools.insert(tool_name.clone());
                    } else {
                        desired.disabled_tools.remove(tool_name);
                    }
                } else {
                    desired.enabled = !disabled;
                }
                let persistent = session.persistent_mcp_aliases.lock().await.contains(name);
                let persisted = if persistent && let Some(store) = session.config() {
                    Some((
                        store.clone(),
                        persist_mcp_view(&store, name, &desired).map_err(config_error)?,
                    ))
                } else {
                    None
                };
                let mutation = if let Some(tool_name) = tool_name {
                    session
                        .mcp
                        .toggle_tool(name, tool_name, !disabled)
                        .await
                        .map_err(mcp_error)
                } else {
                    session.mcp.toggle(name, !disabled).await.map_err(mcp_error)
                };
                if let Err(error) = mutation {
                    if let Some((store, committed)) = persisted
                        && let Err(rollback) =
                            rollback_mcp_view(&store, name, &previous, &committed)
                    {
                        return Err(config_rollback_error("MCP toggle", error, rollback));
                    }
                    return Err(error);
                }
                Ok(runtime_mutation([], Vec::new()))
            }
            McpCommand::Login { name } => {
                if !session
                    .mcp
                    .read()
                    .await
                    .iter()
                    .any(|source| source.alias == name.as_str())
                {
                    return Err(ResourceError::NotFound(format!(
                        "MCP source `{name}` was not found"
                    )));
                }
                let backend = self.mcp_auth.as_ref().ok_or_else(|| {
                    ResourceError::Unavailable(
                        "MCP OAuth interaction backend is not configured".to_owned(),
                    )
                })?;
                let config = session.mcp.config(name).await.map_err(mcp_error)?;
                let url = validate_auth_url(
                    backend.login(session_id, &config).await?,
                    "MCP OAuth backend",
                )?;
                // The answer declares only the runtime; the URL crosses as
                // `mcp/authUrl`, which is where every attached client reads it,
                // including the one that asked.
                let mut dispatch = runtime_mutation([], Vec::new());
                dispatch.signals.auth_url = Some(crate::resources::McpAuthUrl {
                    name: name.clone(),
                    url,
                });
                Ok(dispatch)
            }
            McpCommand::CompleteAuth { name } => {
                let backend = self.mcp_auth.as_ref().ok_or_else(|| {
                    ResourceError::Unavailable(
                        "MCP OAuth interaction backend is not configured".to_owned(),
                    )
                })?;
                let config = session.mcp.config(name).await.map_err(mcp_error)?;
                let verified = backend.complete(session_id, &config).await?;
                Ok(read_only([(
                    "auth",
                    json!({
                        "name": name,
                        "verified": verified,
                        "status": if verified { "complete" } else { "waiting" },
                    }),
                )]))
            }
            McpCommand::Logout { name } => {
                if !session
                    .mcp
                    .read()
                    .await
                    .iter()
                    .any(|source| source.alias == name.as_str())
                {
                    return Err(ResourceError::NotFound(format!(
                        "MCP source `{name}` was not found"
                    )));
                }
                let backend = self.mcp_auth.as_ref().ok_or_else(|| {
                    ResourceError::Unavailable(
                        "MCP OAuth interaction backend is not configured".to_owned(),
                    )
                })?;
                let config = session.mcp.config(name).await.map_err(mcp_error)?;
                backend.logout(session_id, &config).await?;
                session
                    .mcp
                    .clear_auth(name)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                Ok(runtime_mutation([], Vec::new()))
            }
        }
    }
}

/// The alias a new server is added under.
///
/// A name the caller asked for is taken as it is and refused when it collides,
/// so nothing is silently added under a different one; a name they left out is
/// derived from the URL, or from the executable for a stdio server, and then
/// numbered until it is free. Both the configured entries and the servers only
/// this session knows count as taken.
async fn resolve_add_alias(
    session: &CoreResourceSession,
    add: &McpAddCommand,
    transport: &McpTransportConfig,
) -> Result<String, ResourceError> {
    let mut existing = session
        .mcp
        .read()
        .await
        .into_iter()
        .map(|view| view.alias)
        .collect::<BTreeSet<_>>();
    if let Some(store) = session.config() {
        existing.extend(
            store
                .load()
                .and_then(|snapshot| snapshot.mcp_aliases())
                .map_err(config_error)?,
        );
    }
    match transport {
        McpTransportConfig::Http { url, .. } | McpTransportConfig::StreamableHttp { url, .. } => {
            resolve_new_mcp_server_name(add.requested_alias.as_deref(), url.as_str(), &existing)
                .map_err(config_error)
        }
        McpTransportConfig::Stdio { command, .. } => match &add.requested_alias {
            Some(alias) if existing.contains(alias) => {
                Err(config_error(vibe_core::config::ConfigError::InvalidMcp(
                    format!("MCP server name `{alias}` is already configured"),
                )))
            }
            Some(alias) => Ok(alias.clone()),
            None => Ok(dedupe_mcp_server_name(
                &mcp_command_alias(command),
                &existing,
            )),
        },
    }
}
