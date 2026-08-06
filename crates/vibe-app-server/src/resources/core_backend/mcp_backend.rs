use super::*;
use crate::resources::backend_command::{McpAddCommand, mcp_command_alias};
use vibe_core::config::mcp::{dedupe_mcp_server_name, resolve_new_mcp_server_name};
use vibe_core::mcp::{McpAuthConfig, McpOAuthConfig};

impl CoreResourceBackend {
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
            McpCommand::Read => Ok(read_only([(
                "mcp",
                mcp_view(session.mcp.read().await, &session.tools),
            )])),
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
                let state = mcp_view(session.mcp.read().await, &session.tools);
                let mut dispatch = canonical_mutation("mcp", state, diagnostics);
                dispatch.result.insert("name".to_owned(), json!(alias));
                Ok(dispatch)
            }
            McpCommand::Refresh { name } => {
                session
                    .mcp
                    .refresh(name)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let state = mcp_view(session.mcp.read().await, &session.tools);
                Ok(canonical_mutation("mcp", state, Vec::new()))
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
                let state = mcp_view(session.mcp.read().await, &session.tools);
                Ok(canonical_mutation("mcp", state, Vec::new()))
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
                // The URL also crosses as `mcp/authUrl`, which is where a
                // reference client reads it: the answer serves the caller, the
                // notification serves whoever else is attached.
                let mut dispatch = read_only([(
                    "auth",
                    json!({"name": name, "url": url, "status": "waiting"}),
                )]);
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
                let state = mcp_view(session.mcp.read().await, &session.tools);
                Ok(canonical_mutation("mcp", state, Vec::new()))
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
