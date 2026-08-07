use super::*;

mod connector_backend;
mod mcp_backend;
mod shell_backend;

struct BackendDenyApproval;

impl ApprovalAgent for BackendDenyApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

pub(super) struct CoreResourceSession {
    working_directory: String,
    policy: PermissionStore,
    tools: ToolRegistry,
    mcp: McpRegistry,
    persistent_mcp_aliases: Mutex<BTreeSet<String>>,
    pub(super) connectors: ConnectorRegistry,
    connectors_initialized: AtomicBool,
    config: Option<LayeredConfig>,
    mcp_mutation: Mutex<()>,
    connector_mutation: Mutex<()>,
    terminals: TerminalManager,
    shell_operations: Mutex<BTreeMap<String, String>>,
}

impl CoreResourceSession {
    fn config(&self) -> Option<LayeredConfig> {
        let trusted = matches!(
            self.policy.try_trust_decision(&self.working_directory),
            Ok(Some(TrustDecision::Trusted | TrustDecision::SessionTrusted))
        );
        self.config
            .clone()
            .map(|config| config.with_project_trusted(trusted))
    }
}

struct CoreResourceEntry {
    generation: u64,
    session: Arc<CoreResourceSession>,
}

#[derive(Clone)]
pub struct CoreResourceBackend {
    sessions: Arc<StdMutex<BTreeMap<String, CoreResourceEntry>>>,
    mcp_factory: Option<Arc<dyn McpPeerFactory>>,
    mcp_auth: Option<Arc<dyn McpAuthBackend>>,
    connector_definitions: Arc<Vec<ConnectorDefinition>>,
    connector_catalog: Option<Arc<dyn ConnectorCatalogBackend>>,
    connector_backend: Option<Arc<dyn ConnectorBackend>>,
    connector_auth: Option<Arc<dyn ConnectorAuthBackend>>,
    connector_base_url: Option<Url>,
    connector_credential_reference: Arc<str>,
    config: Option<LayeredConfig>,
    approval: Arc<dyn ApprovalAgent>,
}

impl Default for CoreResourceBackend {
    fn default() -> Self {
        Self {
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            mcp_factory: Some(Arc::new(DefaultMcpPeerFactory::default())),
            mcp_auth: None,
            connector_definitions: Arc::new(Vec::new()),
            connector_catalog: None,
            connector_backend: None,
            connector_auth: None,
            connector_base_url: None,
            connector_credential_reference: Arc::from("unconfigured"),
            config: None,
            approval: Arc::new(BackendDenyApproval),
        }
    }
}

impl CoreResourceBackend {
    #[must_use]
    pub fn with_mcp_factory(mut self, factory: Arc<dyn McpPeerFactory>) -> Self {
        self.mcp_factory = Some(factory);
        self
    }

    #[must_use]
    pub fn with_mcp_auth(mut self, backend: Arc<dyn McpAuthBackend>) -> Self {
        self.mcp_auth = Some(backend);
        self
    }

    #[must_use]
    pub fn with_config(mut self, config: LayeredConfig) -> Self {
        self.config = Some(config);
        self
    }

    #[must_use]
    pub fn with_connectors(
        mut self,
        definitions: Vec<ConnectorDefinition>,
        backend: Arc<dyn ConnectorBackend>,
        credential_reference: impl Into<Arc<str>>,
        base_url: Url,
    ) -> Self {
        self.connector_definitions = Arc::new(definitions);
        self.connector_backend = Some(backend);
        self.connector_credential_reference = credential_reference.into();
        self.connector_base_url = Some(base_url);
        self
    }

    #[must_use]
    pub fn with_connector_catalog(
        mut self,
        catalog: Arc<dyn ConnectorCatalogBackend>,
        backend: Arc<dyn ConnectorBackend>,
        credential_reference: impl Into<Arc<str>>,
        base_url: Url,
    ) -> Self {
        self.connector_catalog = Some(catalog);
        self.connector_backend = Some(backend);
        self.connector_credential_reference = credential_reference.into();
        self.connector_base_url = Some(base_url);
        self
    }

    #[must_use]
    pub fn with_connector_auth(mut self, backend: Arc<dyn ConnectorAuthBackend>) -> Self {
        self.connector_auth = Some(backend);
        self
    }

    #[must_use]
    pub fn with_approval(mut self, approval: Arc<dyn ApprovalAgent>) -> Self {
        self.approval = approval;
        self
    }

    pub(super) fn session(
        &self,
        session_id: &str,
    ) -> Result<Arc<CoreResourceSession>, ResourceError> {
        self.sessions
            .lock()
            .map_err(|_| {
                ResourceError::Unavailable("resource backend lock is poisoned".to_owned())
            })?
            .get(session_id)
            .map(|entry| Arc::clone(&entry.session))
            .ok_or_else(|| ResourceError::NotFound(format!("session `{session_id}` was not found")))
    }
}

fn config_error(error: vibe_core::config::ConfigError) -> ResourceError {
    match error {
        vibe_core::config::ConfigError::ConcurrentEdit { .. } => {
            ResourceError::Conflict(error.to_string())
        }
        _ => ResourceError::Unavailable(redact(&error.to_string())),
    }
}

fn persist_mcp_view(
    store: &LayeredConfig,
    name: &str,
    view: &McpServerView,
) -> Result<ConfigSnapshot, vibe_core::config::ConfigError> {
    let disabled_tools = persisted_tool_names("mcp", name, &view.disabled_tools);
    store.persist_mcp_state(name, view.enabled, &disabled_tools)
}

fn persist_connector_view(
    store: &LayeredConfig,
    view: &ConnectorView,
) -> Result<ConfigSnapshot, vibe_core::config::ConfigError> {
    let disabled_tools = persisted_tool_names("connector", &view.alias, &view.disabled_tools);
    store.persist_connector_state(&view.alias, view.enabled, &disabled_tools)
}

fn rollback_mcp_view(
    store: &LayeredConfig,
    name: &str,
    view: &McpServerView,
    committed: &ConfigSnapshot,
) -> Result<ConfigSnapshot, vibe_core::config::ConfigError> {
    let target = committed.selected_target;
    let disabled_tools = persisted_tool_names("mcp", name, &view.disabled_tools);
    store.persist_mcp_state_cas(
        name,
        view.enabled,
        &disabled_tools,
        target,
        committed.fingerprints[&target].clone(),
    )
}

fn rollback_connector_view(
    store: &LayeredConfig,
    view: &ConnectorView,
    committed: &ConfigSnapshot,
) -> Result<ConfigSnapshot, vibe_core::config::ConfigError> {
    let target = committed.selected_target;
    let disabled_tools = persisted_tool_names("connector", &view.alias, &view.disabled_tools);
    store.persist_connector_state_cas(
        &view.alias,
        view.enabled,
        &disabled_tools,
        target,
        committed.fingerprints[&target].clone(),
    )
}

fn config_rollback_error(
    operation: &str,
    error: ResourceError,
    rollback: vibe_core::config::ConfigError,
) -> ResourceError {
    let message = format!(
        "{operation} failed ({}) and configuration rollback failed ({})",
        redact(&error.to_string()),
        redact(&rollback.to_string())
    );
    match rollback {
        vibe_core::config::ConfigError::ConcurrentEdit { .. } => ResourceError::Conflict(message),
        _ => ResourceError::Unavailable(message),
    }
}

fn mcp_error(error: vibe_core::mcp::McpError) -> ResourceError {
    match error {
        vibe_core::mcp::McpError::UnknownServer(name) => {
            ResourceError::NotFound(format!("MCP source `{name}` was not found"))
        }
        vibe_core::mcp::McpError::InvalidConfig(message) => ResourceError::Conflict(message),
        _ => ResourceError::Unavailable(redact(&error.to_string())),
    }
}

fn persisted_tool_names(
    kind: &str,
    alias: &str,
    public_names: &BTreeSet<String>,
) -> BTreeSet<String> {
    let prefix = format!("{kind}_{alias}_");
    public_names
        .iter()
        .map(|name| name.strip_prefix(&prefix).unwrap_or(name).to_owned())
        .collect()
}

impl ResourceBackend for CoreResourceBackend {
    fn open_session(&self, session: ResourceSession) -> Result<(), ResourceError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ResourceError::Unavailable("resource backend lock is poisoned".to_owned())
        })?;
        if let Some(existing) = sessions.get_mut(&session.session_id) {
            if session.generation > existing.generation {
                existing.generation = session.generation;
            }
            return Ok(());
        }
        if !sessions.contains_key(&session.session_id) && sessions.len() >= MAX_RESOURCE_SESSIONS {
            return Err(ResourceError::Conflict(
                "resource backend session capacity was reached".to_owned(),
            ));
        }
        let scoped_config = self.config.as_ref().map(|config| {
            config.scoped_to_working_directory(
                PathBuf::from(&session.working_directory),
                session.project_trusted,
            )
        });
        sessions.insert(
            session.session_id,
            CoreResourceEntry {
                generation: session.generation,
                session: Arc::new(CoreResourceSession {
                    working_directory: session.working_directory,
                    policy: session.policy,
                    tools: session.tools,
                    mcp: McpRegistry::default(),
                    persistent_mcp_aliases: Mutex::new(BTreeSet::new()),
                    connectors: ConnectorRegistry::default(),
                    connectors_initialized: AtomicBool::new(false),
                    config: scoped_config,
                    mcp_mutation: Mutex::new(()),
                    connector_mutation: Mutex::new(()),
                    terminals: TerminalManager::default(),
                    shell_operations: Mutex::new(BTreeMap::new()),
                }),
            },
        );
        Ok(())
    }

    fn configure_mcp<'a>(
        &'a self,
        session_id: &'a str,
        configs: Vec<McpServerConfig>,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move {
            let session = self.session(session_id)?;
            let configured_aliases = if let Some(store) = session.config() {
                store
                    .load()
                    .and_then(|snapshot| snapshot.mcp_aliases())
                    .map_err(config_error)?
            } else {
                BTreeSet::new()
            };
            *session.persistent_mcp_aliases.lock().await = configured_aliases;
            let factory = self.mcp_factory.clone().ok_or_else(|| {
                ResourceError::Unavailable("MCP transport backend is not configured".to_owned())
            })?;
            let diagnostics = session
                .mcp
                .discover_all(
                    configs,
                    factory,
                    &session.tools,
                    session.policy.clone(),
                    self.approval.clone(),
                )
                .await;
            let mut dispatch = runtime_mutation([], diagnostics);
            dispatch.signals.integrations = Some(self.integration_state(&session).await);
            Ok(dispatch)
        })
    }

    fn dispatch<'a>(
        &'a self,
        request: ResourceBackendRequest,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move {
            let session = self.session(&request.session_id)?;
            let mut dispatch = match &request.command {
                ResourceBackendCommand::Mcp(command) => {
                    self.dispatch_mcp(&session, &request.session_id, command)
                        .await
                }
                ResourceBackendCommand::Connector(command) => {
                    self.dispatch_connectors(&session, &request.session_id, command)
                        .await
                }
                ResourceBackendCommand::Shell(command) => {
                    self.dispatch_shell(&session, command).await
                }
            }?;
            // Every integration call learns the current state on its way
            // through, so it carries it back: the runtime snapshot is composed
            // synchronously and cannot ask an async backend for it.
            if matches!(
                request.command,
                ResourceBackendCommand::Mcp(_) | ResourceBackendCommand::Connector(_)
            ) {
                dispatch.signals.integrations = Some(self.integration_state(&session).await);
            }
            Ok(dispatch)
        })
    }

    fn close_session<'a>(&'a self, session_id: &'a str, generation: u64) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            let session = {
                let mut sessions = self.sessions.lock().map_err(|_| {
                    ResourceError::Unavailable("resource backend lock is poisoned".to_owned())
                })?;
                let matches_generation = sessions
                    .get(session_id)
                    .is_some_and(|entry| entry.generation == generation);
                matches_generation
                    .then(|| sessions.remove(session_id))
                    .flatten()
                    .map(|entry| entry.session)
            };
            let Some(session) = session else {
                return Ok(());
            };
            let mut failures = Vec::new();
            if let Some(auth) = &self.mcp_auth
                && let Err(error) = auth.close_session(session_id).await
            {
                failures.push(redact(&error.to_string()));
            }
            failures.extend(session.mcp.close().await);
            if let Err(error) = session.connectors.close().await {
                failures.push(redact(&error.to_string()));
            }
            if let Err(error) = session.terminals.cleanup_all().await {
                failures.push(redact(&error.to_string()));
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(ResourceError::Unavailable(failures.join("; ")))
            }
        })
    }
}
