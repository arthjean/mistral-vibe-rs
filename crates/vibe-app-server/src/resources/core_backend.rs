use super::*;

mod mcp_backend;
pub(crate) use mcp_backend::overlay_plugin_sources;
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
    /// The servers the session was started with, which are all it knows
    /// when it has no configuration store to read them from.
    started_mcp: StdMutex<Vec<McpServerConfig>>,
    /// The servers the session's plugins declare, listed after the
    /// configured ones.
    plugin_mcp: StdMutex<Vec<crate::resources::PluginMcpSource>>,
    config: Option<LayeredConfig>,
    mcp_mutation: Mutex<()>,
    terminals: TerminalManager,
    shell_operations: Mutex<BTreeMap<String, String>>,
}

impl CoreResourceSession {
    /// The plugin that owns each plugin server, by catalog name.
    fn plugin_owners(&self) -> BTreeMap<String, String> {
        self.plugin_sources()
            .into_iter()
            .map(|source| (source.name, source.plugin_name))
            .collect()
    }

    fn plugin_sources(&self) -> Vec<crate::resources::PluginMcpSource> {
        self.plugin_mcp
            .lock()
            .map(|sources| sources.clone())
            .unwrap_or_default()
    }

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

/// What `mcp_catalog/authRequired` is deduplicated on, reference
/// `_auth_required_key`: the session, the server, the descriptor revision and
/// the connection revision observed.
type AuthRequiredKey = (String, String, String, Option<String>);

#[derive(Clone)]
pub struct CoreResourceBackend {
    sessions: Arc<StdMutex<BTreeMap<String, CoreResourceEntry>>>,
    mcp_factory: Option<Arc<dyn McpPeerFactory>>,
    /// Reference `MCPAuthenticationService`: one per process, holding every
    /// session's catalog and the credentials they sign in with.
    mcp_authentication: Arc<McpAuthenticationService>,
    mcp_auth_required: Arc<StdMutex<Vec<mcp_backend::PendingAuthRequired>>>,
    mcp_auth_required_seen: Arc<StdMutex<BTreeSet<AuthRequiredKey>>>,
    config: Option<LayeredConfig>,
}

impl Default for CoreResourceBackend {
    fn default() -> Self {
        Self {
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            mcp_factory: Some(Arc::new(DefaultMcpPeerFactory::default())),
            // No credential store: every OAuth server reads as never signed
            // in, which is what a host that attached none can offer.
            mcp_authentication: Arc::new(McpAuthenticationService::new(None)),
            mcp_auth_required: Arc::new(StdMutex::new(Vec::new())),
            mcp_auth_required_seen: Arc::new(StdMutex::new(BTreeSet::new())),
            config: None,
        }
    }
}

impl CoreResourceBackend {
    #[must_use]
    pub fn with_mcp_factory(mut self, factory: Arc<dyn McpPeerFactory>) -> Self {
        self.mcp_factory = Some(factory);
        self
    }

    /// The authentication service every session's MCP servers resolve their
    /// credentials through.
    #[must_use]
    pub fn with_mcp_authentication(mut self, service: Arc<McpAuthenticationService>) -> Self {
        self.mcp_authentication = service;
        self
    }

    #[must_use]
    pub fn with_config(mut self, config: LayeredConfig) -> Self {
        self.config = Some(config);
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
                    started_mcp: StdMutex::new(Vec::new()),
                    plugin_mcp: StdMutex::new(Vec::new()),
                    config: scoped_config,
                    mcp_mutation: Mutex::new(()),
                    terminals: TerminalManager::default(),
                    shell_operations: Mutex::new(BTreeMap::new()),
                }),
            },
        );
        Ok(())
    }

    fn set_plugin_mcp(&self, session_id: &str, sources: Vec<crate::resources::PluginMcpSource>) {
        if let Ok(session) = self.session(session_id)
            && let Ok(mut held) = session.plugin_mcp.lock()
        {
            *held = sources;
        }
    }

    fn configure_mcp<'a>(
        &'a self,
        session_id: &'a str,
        configs: Vec<McpServerConfig>,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move {
            let session = self.session(session_id)?;
            if let Ok(mut started) = session.started_mcp.lock() {
                started.clone_from(&configs);
            }
            let factory = self.mcp_factory.clone().ok_or_else(|| {
                ResourceError::Unavailable("MCP transport backend is not configured".to_owned())
            })?;
            let _mutation = session.mcp_mutation.lock().await;
            self.wire_mcp_registry(&session, session_id, &configs).await;
            let diagnostics = session
                .mcp
                .discover_all(
                    configs,
                    factory,
                    &session.tools,
                    session.policy.clone(),
                    Arc::new(BackendDenyApproval),
                )
                .await;
            let mut dispatch = runtime_mutation([], diagnostics);
            dispatch.signals.integrations = Some(self.integration_state(&session).await);
            dispatch.signals.auth_required = self.accept_auth_required(&session, session_id).await;
            Ok(dispatch)
        })
    }

    fn mcp_catalog<'a>(
        &'a self,
        call: McpCatalogCall,
        target: McpCatalogTarget,
        notify: McpCatalogNotify,
    ) -> McpCatalogFuture<'a> {
        Box::pin(self.run_mcp_catalog(call, target, notify))
    }

    fn dispatch<'a>(
        &'a self,
        request: ResourceBackendRequest,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move {
            let session = self.session(&request.session_id)?;
            match &request.command {
                ResourceBackendCommand::Shell(command) => {
                    self.dispatch_shell(&session, command).await
                }
            }
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
            // Reference files a session's catalog under a weak key, which goes
            // with the session.
            self.mcp_authentication
                .release(&Some(session_id.to_owned()))
                .await;
            if let Ok(mut seen) = self.mcp_auth_required_seen.lock() {
                seen.retain(|(session, ..)| session != session_id);
            }
            if let Ok(mut pending) = self.mcp_auth_required.lock() {
                pending.retain(|event| event.session_id() != session_id);
            }
            failures.extend(session.mcp.close().await);
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
