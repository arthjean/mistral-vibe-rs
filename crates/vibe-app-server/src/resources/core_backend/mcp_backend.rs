//! The MCP catalog over the sessions this backend holds.
//!
//! Reference `MCPCatalogService` (`vibe/app_server/mcp_catalog.py`): every
//! mutation is written to the configuration first, and the session it targets
//! is then converged on what the configuration says, through the registry and
//! the process-wide authentication service. A sessionless mutation only edits
//! the configuration.

use super::*;
use crate::resources::mcp_catalog::{
    McpCatalogCall, McpCatalogError, McpCatalogNotify, McpCatalogOutcome, McpCatalogTarget,
};
use vibe_core::config::mcp::McpOAuthAddition;
use vibe_core::mcp::{
    McpAuthStatus, McpAuthenticationError, McpAuthorizationRequired, McpAuthorizationSink,
    McpCatalogOwner, McpDescriptorCache, McpServerRemoveError,
};
use vibe_protocol::ProtocolErrorCode;

/// An authorization requirement a session's registry raised, waiting for the
/// catalog to decide whether to publish it.
pub(in crate::resources) struct PendingAuthRequired {
    session_id: String,
    name: String,
    required: McpAuthorizationRequired,
}

impl PendingAuthRequired {
    pub(in crate::resources) fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Reference `_auth_required_seen`'s key: a requirement is published once per
/// session, server, descriptor revision and observed connection.
type AuthRequiredKey = (String, String, String, Option<String>);

/// Reference `project_mcp_sources`: what is not active in the session although
/// the configuration names it.
const INACTIVE_SOURCE: &str = "This MCP server is configured but not active in this session";

impl CoreResourceBackend {
    /// Every configured MCP server a session can call a tool through, in the
    /// list `MCPState` declares. The session's connector rows are the app
    /// server's to publish: `AppServer::overlay_session_connectors` adds them.
    pub(super) async fn mcp_state(&self, session: &CoreResourceSession) -> Value {
        let configured = self.session_mcp_configs(session).unwrap_or_default();
        project_mcp(
            &configured,
            &session.plugin_sources(),
            session.mcp.read().await,
            session.mcp.auth_status().await,
            &session.tools,
        )
    }

    /// The integration surface the runtime snapshot publishes for a session.
    pub(super) async fn integration_state(
        &self,
        session: &CoreResourceSession,
    ) -> crate::resources::IntegrationState {
        crate::resources::IntegrationState {
            mcp: self.mcp_state(session).await,
        }
    }

    /// The servers the session's configuration names, in its order.
    ///
    /// A session opened without a configuration store has only the servers it
    /// was started with, which the registry holds.
    fn session_mcp_configs(
        &self,
        session: &CoreResourceSession,
    ) -> Result<Vec<McpServerConfig>, McpCatalogError> {
        let Some(store) = session.config() else {
            return Ok(session
                .started_mcp
                .lock()
                .map(|configs| configs.clone())
                .unwrap_or_default());
        };
        store
            .load()
            .and_then(|snapshot| {
                snapshot.mcp_servers(std::path::Path::new(&session.working_directory))
            })
            .map_err(config_refusal)
    }

    /// Points a session's registry at the authentication service, the
    /// references its servers resolve to, the descriptor cache and the sink
    /// its authorization requirements go to.
    pub(super) async fn wire_mcp_registry(
        &self,
        session: &CoreResourceSession,
        session_id: &str,
        configs: &[McpServerConfig],
    ) {
        let service = self.mcp_authentication.clone();
        service
            .bind_catalog(configs, Some(session_id.to_owned()))
            .await;
        let mut references = BTreeMap::new();
        for config in configs {
            references.insert(config.alias.clone(), service.reference_for(config).await);
        }
        let pending = self.mcp_auth_required.clone();
        let owner = session_id.to_owned();
        let sink: McpAuthorizationSink = Arc::new(move |name, required| {
            if let Ok(mut pending) = pending.lock() {
                pending.push(PendingAuthRequired {
                    session_id: owner.clone(),
                    name: name.to_owned(),
                    required: required.clone(),
                });
            }
        });
        session
            .mcp
            .configure_authorization(service, references, Some(sink))
            .await;
        let cache = session
            .config()
            .and_then(|store| store.load().ok())
            .and_then(|snapshot| session_log_dir(&snapshot.effective))
            .map(|directory| {
                Arc::new(McpDescriptorCache::new(
                    McpDescriptorCache::root_for(&directory),
                    vibe_core::mcp::descriptor_cache::DESCRIPTOR_CACHE_TTL_SECONDS,
                ))
            });
        session.mcp.configure_descriptor_cache(cache).await;
    }

    /// Reference `_converge` and `authorization_changed`: rediscovers the
    /// session's servers from its configuration, reaching again the ones
    /// `invalidated` names, or all of them for a refresh.
    async fn converge(
        &self,
        session: &CoreResourceSession,
        session_id: &str,
        invalidated: &[&str],
        force_all: bool,
    ) -> Result<(), McpCatalogError> {
        let configs = self.session_mcp_configs(session)?;
        let factory = self.mcp_factory.clone().ok_or_else(|| {
            McpCatalogError::refused(
                ProtocolErrorCode::NotImplemented,
                "MCP transport backend is not configured",
            )
        })?;
        self.wire_mcp_registry(session, session_id, &configs).await;
        for config in &configs {
            if force_all || invalidated.contains(&config.alias.as_str()) {
                session.mcp.invalidate(&config.alias).await;
            }
        }
        let _ = session
            .mcp
            .discover_all(
                configs,
                factory,
                &session.tools,
                session.policy.clone(),
                Arc::new(BackendDenyApproval),
            )
            .await;
        Ok(())
    }

    /// Reference `accept_auth_required`, over what the session's registry
    /// raised since the last call: a requirement is published when its server
    /// still needs a sign-in under that descriptor revision and it was not
    /// published already.
    pub(super) async fn accept_auth_required(
        &self,
        session: &CoreResourceSession,
        session_id: &str,
    ) -> Vec<Map<String, Value>> {
        let raised = self
            .mcp_auth_required
            .lock()
            .map(|mut pending| {
                let (mine, others) = std::mem::take(&mut *pending)
                    .into_iter()
                    .partition::<Vec<_>, _>(|event| event.session_id == session_id);
                *pending = others;
                mine
            })
            .unwrap_or_default();
        let mut accepted = Vec::new();
        for event in raised {
            let needs_auth =
                session.mcp.auth_status().await.get(&event.name) == Some(&McpAuthStatus::NeedsAuth);
            let current = session.mcp.descriptor_revision(&event.name).await;
            if !needs_auth || current != event.required.descriptor_revision {
                continue;
            }
            let key: AuthRequiredKey = (
                session_id.to_owned(),
                event.name.clone(),
                event.required.descriptor_revision.clone(),
                event.required.observed_connection_revision.clone(),
            );
            let fresh = self
                .mcp_auth_required_seen
                .lock()
                .map(|mut seen| seen.insert(key))
                .unwrap_or(false);
            if !fresh {
                continue;
            }
            let mut params = Map::new();
            params.insert("sessionId".to_owned(), json!(session_id));
            params.insert("name".to_owned(), json!(event.name));
            params.insert(
                "descriptorRevision".to_owned(),
                json!(event.required.descriptor_revision),
            );
            params.insert(
                "observedConnectionRevision".to_owned(),
                json!(event.required.observed_connection_revision),
            );
            accepted.push(params);
        }
        accepted
    }

    /// Reference `_clear_resolved_auth_required`: a session's projected
    /// runtime moved, so what it was told needs a sign-in may be told again.
    fn forget_auth_required(&self, session_id: &str) {
        if let Ok(mut seen) = self.mcp_auth_required_seen.lock() {
            seen.retain(|(session, ..)| session != session_id);
        }
    }

    pub(super) async fn run_mcp_catalog(
        &self,
        call: McpCatalogCall,
        target: McpCatalogTarget,
        notify: McpCatalogNotify,
    ) -> Result<McpCatalogOutcome, McpCatalogError> {
        match target {
            McpCatalogTarget::Sessionless => self.sessionless_catalog(call, notify).await,
            McpCatalogTarget::Session(session_id) => {
                let session = self.session(&session_id).map_err(|_| {
                    McpCatalogError::refused(
                        ProtocolErrorCode::NotFound,
                        format!("Session not found: {session_id}"),
                    )
                })?;
                let _mutation = match call {
                    McpCatalogCall::Read { .. } => None,
                    _ => Some(session.mcp_mutation.lock().await),
                };
                self.session_catalog(&session, &session_id, call, notify)
                    .await
            }
        }
    }

    async fn session_catalog(
        &self,
        session: &CoreResourceSession,
        session_id: &str,
        call: McpCatalogCall,
        notify: McpCatalogNotify,
    ) -> Result<McpCatalogOutcome, McpCatalogError> {
        let owner: McpCatalogOwner = Some(session_id.to_owned());
        let mut result = BTreeMap::new();
        match &call {
            McpCatalogCall::Read { .. } => {
                let auth_required = self.accept_auth_required(session, session_id).await;
                result.insert("mcp".to_owned(), self.mcp_state(session).await);
                return Ok(McpCatalogOutcome {
                    result,
                    runtime_updated: false,
                    integrations: None,
                    auth_required,
                });
            }
            McpCatalogCall::Refresh { .. } => {
                self.converge(session, session_id, &[], true).await?;
            }
            McpCatalogCall::Toggle {
                name,
                disabled,
                tool_name,
                ..
            } => {
                reject_plugin_mutation(session, name)?;
                let store = session_store(session)?;
                store
                    .persist_mcp_toggle(name, *disabled, tool_name.as_deref())
                    .map_err(config_refusal)?;
                self.converge(session, session_id, &[], false).await?;
            }
            McpCatalogCall::Add {
                url,
                name,
                scopes,
                legacy_http,
                allow_insecure_http,
                ..
            } => {
                let store = session_store(session)?;
                let added = store
                    .persist_oauth_mcp_server(&McpOAuthAddition {
                        url,
                        name: name.as_deref(),
                        scopes,
                        legacy_http: *legacy_http,
                        allow_insecure_http: *allow_insecure_http,
                    })
                    .map_err(config_refusal)?;
                result.insert("name".to_owned(), json!(added.name));
                result.insert("url".to_owned(), json!(added.url));
                result.insert("created".to_owned(), json!(added.created));
                self.converge(session, session_id, &[], false).await?;
            }
            McpCatalogCall::Remove { name, .. } => {
                reject_plugin_mutation(session, name)?;
                let store = session_store(session)?;
                let removed = self.remove_with_credentials(&store, name, &owner).await?;
                result.insert("name".to_owned(), json!(removed.name));
                result.insert("removed".to_owned(), json!(removed.removed));
                self.converge(session, session_id, &[], false).await?;
            }
            McpCatalogCall::Login { name, .. } => {
                let configs = self.session_mcp_configs(session)?;
                self.mcp_authentication
                    .bind_catalog(&configs, owner.clone())
                    .await;
                self.mcp_authentication
                    .login(name, publish_auth_url(name, &notify), &owner)
                    .await
                    .map_err(authentication_refusal)?;
                self.converge(session, session_id, &[name.as_str()], false)
                    .await?;
            }
            McpCatalogCall::Logout { name, .. } => {
                let configs = self.session_mcp_configs(session)?;
                self.mcp_authentication
                    .bind_catalog(&configs, owner.clone())
                    .await;
                self.mcp_authentication
                    .logout(name, &owner)
                    .await
                    .map_err(authentication_refusal)?;
                self.converge(session, session_id, &[name.as_str()], false)
                    .await?;
            }
        }
        self.forget_auth_required(session_id);
        let auth_required = self.accept_auth_required(session, session_id).await;
        Ok(McpCatalogOutcome {
            result,
            runtime_updated: true,
            integrations: Some(self.integration_state(session).await),
            auth_required,
        })
    }

    async fn sessionless_catalog(
        &self,
        call: McpCatalogCall,
        notify: McpCatalogNotify,
    ) -> Result<McpCatalogOutcome, McpCatalogError> {
        let store = self.config.clone().ok_or_else(|| {
            McpCatalogError::refused(
                ProtocolErrorCode::NotImplemented,
                "Sessionless MCP catalog changes are not available on this server",
            )
        })?;
        let owner: McpCatalogOwner = None;
        let mut result = BTreeMap::new();
        match &call {
            McpCatalogCall::Read { .. } | McpCatalogCall::Refresh { .. } => {
                // Both name a session, which `target` resolved; reaching here
                // would mean a session-less read, which the wire cannot express.
                return Err(McpCatalogError::refused(
                    ProtocolErrorCode::NotFound,
                    "Session not found",
                ));
            }
            McpCatalogCall::Toggle {
                name,
                disabled,
                tool_name,
                ..
            } => {
                store
                    .persist_mcp_toggle(name, *disabled, tool_name.as_deref())
                    .map_err(config_refusal)?;
            }
            McpCatalogCall::Add {
                url,
                name,
                scopes,
                legacy_http,
                allow_insecure_http,
                ..
            } => {
                let added = store
                    .persist_oauth_mcp_server(&McpOAuthAddition {
                        url,
                        name: name.as_deref(),
                        scopes,
                        legacy_http: *legacy_http,
                        allow_insecure_http: *allow_insecure_http,
                    })
                    .map_err(config_refusal)?;
                result.insert("name".to_owned(), json!(added.name));
                result.insert("url".to_owned(), json!(added.url));
                result.insert("created".to_owned(), json!(added.created));
            }
            McpCatalogCall::Remove { name, .. } => {
                let removed = self.remove_with_credentials(&store, name, &owner).await?;
                result.insert("name".to_owned(), json!(removed.name));
                result.insert("removed".to_owned(), json!(removed.removed));
            }
            McpCatalogCall::Login { name, .. } => {
                self.bind_sessionless(&store).await?;
                self.mcp_authentication
                    .login(name, publish_auth_url(name, &notify), &owner)
                    .await
                    .map_err(authentication_refusal)?;
                return Ok(sessionless_outcome(result));
            }
            McpCatalogCall::Logout { name, .. } => {
                self.bind_sessionless(&store).await?;
                self.mcp_authentication
                    .logout(name, &owner)
                    .await
                    .map_err(authentication_refusal)?;
                return Ok(sessionless_outcome(result));
            }
        }
        // Reference `_converge` resolves the catalog, which binds it, before
        // finding no session to converge.
        self.bind_sessionless(&store).await?;
        Ok(sessionless_outcome(result))
    }

    /// Binds the sessionless configuration's servers under the anonymous
    /// owner every sessionless caller shares.
    async fn bind_sessionless(&self, store: &LayeredConfig) -> Result<(), McpCatalogError> {
        let configs = store
            .load()
            .and_then(|snapshot| snapshot.mcp_servers(store.working_directory()))
            .map_err(config_refusal)?;
        self.mcp_authentication.bind_catalog(&configs, None).await;
        Ok(())
    }

    async fn remove_with_credentials(
        &self,
        store: &LayeredConfig,
        name: &str,
        owner: &McpCatalogOwner,
    ) -> Result<vibe_core::config::mcp::McpRemoval, McpCatalogError> {
        self.mcp_authentication
            .remove_with_credentials(store, name, owner)
            .await
            .map_err(|error| match error {
                McpServerRemoveError::Config(error) => config_refusal(error),
                McpServerRemoveError::Credentials(error) => {
                    McpCatalogError::refused(ProtocolErrorCode::InternalError, error.to_string())
                }
            })
    }
}

fn sessionless_outcome(result: BTreeMap<String, Value>) -> McpCatalogOutcome {
    McpCatalogOutcome {
        result,
        runtime_updated: false,
        integrations: None,
        auth_required: Vec::new(),
    }
}

fn session_store(session: &CoreResourceSession) -> Result<LayeredConfig, McpCatalogError> {
    session.config().ok_or_else(|| {
        McpCatalogError::refused(
            ProtocolErrorCode::NotImplemented,
            "This session has no configuration to change",
        )
    })
}

/// Reference `_login`'s `publish_url`: the URL goes out under the catalog's
/// name and then under the alias older clients listen for.
fn publish_auth_url(name: &str, notify: &McpCatalogNotify) -> vibe_core::auth::AuthUrlSink {
    let name = name.to_owned();
    let notify = notify.clone();
    Arc::new(move |url| {
        let mut params = Map::new();
        params.insert("name".to_owned(), json!(name));
        params.insert("url".to_owned(), json!(url));
        notify("mcp_catalog/authUrl", params.clone());
        notify("mcp/authUrl", params);
        Box::pin(async {})
    })
}

fn config_refusal(error: vibe_core::config::ConfigError) -> McpCatalogError {
    match error {
        vibe_core::config::ConfigError::ConcurrentEdit { .. } => {
            McpCatalogError::refused(ProtocolErrorCode::Conflict, error.to_string())
        }
        _ => McpCatalogError::refused(ProtocolErrorCode::InvalidParams, redact(&error.to_string())),
    }
}

fn authentication_refusal(error: McpAuthenticationError) -> McpCatalogError {
    McpCatalogError::refused(ProtocolErrorCode::InvalidParams, redact(&error.to_string()))
}

/// The session log directory the effective configuration resolved.
fn session_log_dir(effective: &toml::Table) -> Option<PathBuf> {
    effective
        .get("session_logging")?
        .get("save_dir")?
        .as_str()
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
}

/// Reference `project_mcp_sources` over the legacy runtime's `project_mcp`:
/// every configured server in configuration order.
fn project_mcp(
    configured: &[McpServerConfig],
    plugin_sources: &[crate::resources::PluginMcpSource],
    views: Vec<McpServerView>,
    auth_status: BTreeMap<String, McpAuthStatus>,
    tools: &ToolRegistry,
) -> Value {
    let specs = tools
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let views = views
        .into_iter()
        .map(|view| (view.alias.clone(), view))
        .collect::<BTreeMap<_, _>>();
    let mut discovery_errors = Map::new();
    let mut sources = Vec::with_capacity(configured.len());
    for server in configured {
        let name = server.alias.as_str();
        let view = views.get(name);
        let failed = view.is_some_and(|view| view.status == McpServerStatus::Failed);
        if let (true, Some(diagnostic)) = (failed, view.and_then(|view| view.diagnostic.as_ref())) {
            discovery_errors.insert(name.to_owned(), json!(redact(diagnostic)));
        }
        let status = match view {
            None if !server.enabled => McpSourceStatus::Disabled,
            None => {
                discovery_errors
                    .entry(name.to_owned())
                    .or_insert_with(|| json!(INACTIVE_SOURCE));
                McpSourceStatus::Unavailable
            }
            Some(_) if !server.enabled => McpSourceStatus::Disabled,
            Some(_) if failed => McpSourceStatus::Unavailable,
            Some(_) => match auth_status.get(name) {
                Some(McpAuthStatus::NeedsAuth) => McpSourceStatus::NeedsAuth,
                Some(McpAuthStatus::Ok) => McpSourceStatus::Connected,
                _ => McpSourceStatus::Enabled,
            },
        };
        let mut summaries = view
            .filter(|view| view.status == McpServerStatus::Healthy && server.enabled)
            .map(|view| {
                view.tools
                    .iter()
                    .map(|public| {
                        let spec = specs.get(public);
                        let remote = view
                            .remote_names
                            .get(public)
                            .cloned()
                            .unwrap_or_else(|| public.clone());
                        let description = spec.map_or_else(String::new, |spec| {
                            display_description(&spec.description, name)
                        });
                        let enabled = spec.is_some_and(|spec| {
                            spec.availability == vibe_core::tools::ToolAvailability::Available
                        });
                        json!({"name": remote, "description": description, "enabled": enabled})
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        summaries.sort_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["name"].as_str().unwrap_or_default())
        });
        sources.push(json!({
            "name": name,
            "displayName": name,
            "kind": McpSourceKind::Server,
            "transport": vibe_core::mcp::transport_name(&server.transport),
            "status": status,
            "tools": summaries,
            "error": null,
            "pluginName": null,
        }));
    }
    project_plugin_sources(
        configured,
        plugin_sources,
        &mut sources,
        &mut discovery_errors,
    );
    json!({
        "sources": sources,
        "discoveryErrors": Value::Object(discovery_errors),
        "connectorError": null,
        "manageConnectorsUrl": null,
    })
}

/// Reference `format_tool_display_description`: the one line a tool row shows,
/// without the `[alias] ` prefix a published MCP description carries.
fn display_description(description: &str, source: &str) -> String {
    let prefix = format!("[{source}] ");
    description
        .strip_prefix(&prefix)
        .unwrap_or(description)
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Refuses to toggle or remove a server a plugin owns: no configuration entry
/// backs it, so the write would add one shadowing the plugin's under the same
/// name (reference `_reject_plugin_mutation`). A configured server of that
/// name owns it outright.
fn reject_plugin_mutation(
    session: &CoreResourceSession,
    name: &str,
) -> Result<(), McpCatalogError> {
    let configured = session.config().is_some_and(|store| {
        store
            .load()
            .and_then(|snapshot| {
                snapshot.mcp_servers(std::path::Path::new(&session.working_directory))
            })
            .is_ok_and(|configs| configs.iter().any(|config| config.alias == name))
    });
    if configured {
        return Ok(());
    }
    let Some(plugin) = session.plugin_owners().remove(name) else {
        return Ok(());
    };
    Err(McpCatalogError::refused(
        vibe_protocol::ProtocolErrorCode::InvalidParams,
        format!(
            "MCP server '{name}' is managed by the '{plugin}' plugin and cannot be toggled or removed from the MCP catalog."
        ),
    ))
}

/// Appends the rows of a unified session's plugin servers after the configured
/// ones, skipping a name the configuration holds (reference
/// `project_mcp_sources` over `_project_plugin_source`).
fn project_plugin_sources(
    configured: &[McpServerConfig],
    plugin_sources: &[crate::resources::PluginMcpSource],
    sources: &mut Vec<Value>,
    discovery_errors: &mut Map<String, Value>,
) {
    let held: Vec<&str> = configured
        .iter()
        .map(|server| server.alias.as_str())
        .collect();
    push_plugin_sources(&held, plugin_sources, sources, discovery_errors);
}

/// Adds the plugin rows an `MCPState` value does not list yet, as
/// [`project_plugin_sources`] lists them. A state projected before the
/// session's plugins were bound, or without a backend, gains them here.
pub(crate) fn overlay_plugin_sources(
    mcp: &mut Value,
    plugin_sources: &[crate::resources::PluginMcpSource],
) {
    let Some(state) = mcp.as_object_mut() else {
        return;
    };
    let held: Vec<String> = state
        .get("sources")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    row.get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    let held: Vec<&str> = held.iter().map(String::as_str).collect();
    let mut rows = Vec::new();
    let mut errors = Map::new();
    push_plugin_sources(&held, plugin_sources, &mut rows, &mut errors);
    if let Some(Value::Array(sources)) = state.get_mut("sources") {
        sources.extend(rows);
    }
    if let Some(Value::Object(discovery_errors)) = state.get_mut("discoveryErrors") {
        for (name, error) in errors {
            discovery_errors.entry(name).or_insert(error);
        }
    }
}

/// The session's harness runtime is handed the same servers and refuses every
/// one of them, so each row carries that refusal as its discovery error,
/// ahead of anything the plugin's own discovery recorded.
fn push_plugin_sources(
    held: &[&str],
    plugin_sources: &[crate::resources::PluginMcpSource],
    sources: &mut Vec<Value>,
    discovery_errors: &mut Map<String, Value>,
) {
    for source in plugin_sources {
        if held.contains(&source.name.as_str()) {
            continue;
        }
        let tools: Vec<Value> = source
            .tools
            .iter()
            .map(|(name, description)| {
                json!({
                    "name": name,
                    // No alias to strip: only the first line is kept.
                    "description": description
                        .as_deref()
                        .unwrap_or_default()
                        .split('\n')
                        .next()
                        .unwrap_or_default()
                        .trim(),
                    "enabled": true,
                })
            })
            .collect();
        sources.push(json!({
            "name": source.name,
            "displayName": source.name,
            "kind": McpSourceKind::Server,
            "transport": source.transport,
            "status": source.status,
            "tools": tools,
            "error": null,
            "pluginName": source.plugin_name,
        }));
        discovery_errors
            .entry(source.name.clone())
            .or_insert_with(|| json!(PLUGIN_RUNTIME_REFUSAL));
    }
}

/// What the reference's harness runtime answers for every plugin server it is
/// handed (observed against the pinned reference).
const PLUGIN_RUNTIME_REFUSAL: &str = "authentication was rejected";
