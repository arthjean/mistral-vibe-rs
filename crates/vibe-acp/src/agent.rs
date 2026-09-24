//! The ACP agent: what an editor connection is opened with, the handshake,
//! and the operations that do not belong to one of the surfaces beside this
//! file.
//!
//! Reference `VibeAcpAgent` (`vibe/acp/agent.py`). It is a client of the app
//! server: every session is one canonical session, and this adapter projects
//! what the app server publishes onto the editor protocol.

pub(crate) mod extensions;
pub(crate) mod services;
pub(crate) mod sessions;
pub(crate) mod state;
pub(crate) mod surface;
pub(crate) mod telemetry;
pub(crate) mod turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::TurnDriver;
use vibe_app_server::experiments::Credentials;
use vibe_app_server::harness::HarnessSelection;
use vibe_app_server::projects::ProjectsService;
use vibe_core::telemetry::{
    ClientTelemetry, ExperimentExposures, LaunchContext, NoClientTelemetry,
};

use crate::agent::state::AgentState;
use crate::auth::{
    AcpAuthEnvironment, AuthController, ProductionAuthEnvironment, default_vibe_home,
    terminal_method,
};
use crate::client_tools::{AcpClientPort, DEFAULT_CLIENT_TOOL_TIMEOUT};
use crate::protocol::{
    ACP_PROTOCOL_VERSION, AcpAgentCapabilities, AcpError, AcpImplementation, AcpInitializeRequest,
    AcpInitializeResponse, AcpPromptCapabilities,
};
use crate::session::AcpHarness;

pub struct AcpAgent<D>
where
    D: TurnDriver,
{
    pub(in crate::agent) driver: Arc<D>,
    state: Mutex<AgentState<D>>,
    pub(crate) client: Option<Arc<dyn AcpClientPort>>,
    pub(crate) client_tool_timeout: Duration,
    pub(in crate::agent) session_root: Option<PathBuf>,
    pub(crate) auth: AuthController,
    pub(in crate::agent) credential_environment: String,
    pub(in crate::agent) production_cloud: bool,
    pub(in crate::agent) projects: Mutex<Option<ProjectsService>>,
    /// Where an event the editor records reaches the datalake. Every session's
    /// app server is built over the same sink, so an editor-side event and a
    /// turn's own travel through one client, as they do upstream.
    pub(in crate::agent) telemetry: Arc<dyn ClientTelemetry>,
    /// What every session of this process needs to resolve its enrollment, or
    /// [`None`] for an adapter that publishes no telemetry and therefore has no
    /// census to fill.
    pub(in crate::agent) experiments: Option<AcpExperiments>,
    /// The harness the launch flags picked, which every session's app server
    /// reports.
    pub(in crate::agent) harness: HarnessSelection,
}

/// The three things a session's enrollment is built from, installed once for
/// the process.
#[derive(Clone)]
pub struct AcpExperiments {
    pub exposures: ExperimentExposures,
    pub credentials: Credentials,
    pub launch: LaunchContext,
}

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    pub fn new(driver: D) -> Result<Self, AcpError> {
        Ok(Self {
            driver: Arc::new(driver),
            state: Mutex::new(AgentState::new()),
            client: None,
            client_tool_timeout: DEFAULT_CLIENT_TOOL_TIMEOUT,
            session_root: None,
            auth: AuthController::new(Arc::new(
                ProductionAuthEnvironment::new(default_vibe_home()),
            )),
            credential_environment: "MISTRAL_API_KEY".to_owned(),
            production_cloud: false,
            projects: Mutex::new(None),
            telemetry: Arc::new(NoClientTelemetry),
            experiments: None,
            harness: HarnessSelection::default(),
        })
    }

    /// Installs the telemetry client every session's app server ships a
    /// client-recorded event through.
    #[must_use]
    pub fn with_client_telemetry(mut self, telemetry: Arc<dyn ClientTelemetry>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Installs what every session of this process resolves its enrollment
    /// with. An adapter that installs none runs on the declared defaults and
    /// reports no exposure, which is what a process without telemetry does.
    #[must_use]
    pub fn with_experiments(mut self, experiments: AcpExperiments) -> Self {
        self.experiments = Some(experiments);
        self
    }

    #[must_use]
    pub fn with_client_port(mut self, client: Arc<dyn AcpClientPort>, timeout: Duration) -> Self {
        self.client = Some(client);
        self.client_tool_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_session_root(mut self, session_root: impl Into<PathBuf>) -> Self {
        self.session_root = Some(session_root.into());
        self
    }

    /// Replaces the ambient authentication environment, which is how the
    /// binary supplies the production home and the tests script the world.
    #[must_use]
    pub fn with_auth_environment(mut self, environment: Arc<dyn AcpAuthEnvironment>) -> Self {
        self.auth = AuthController::new(environment);
        self
    }

    /// Names the dotenv variable the lazy cloud services read their
    /// credential from.
    #[must_use]
    pub fn with_credential_environment(
        mut self,
        credential_environment: impl Into<String>,
    ) -> Self {
        self.credential_environment = credential_environment.into();
        self
    }

    #[must_use]
    pub fn with_harness_selection(mut self, harness: HarnessSelection) -> Self {
        self.harness = harness;
        self
    }

    #[must_use]
    pub fn with_production_cloud(mut self) -> Self {
        self.production_cloud = true;
        self
    }

    #[must_use]
    pub fn with_projects_service(mut self, service: ProjectsService) -> Self {
        self.production_cloud = true;
        self.projects = Mutex::new(Some(service));
        self
    }

    /// Reference `initialize`: any protocol version is accepted and the
    /// handshake may be repeated, each one replacing what the client declared.
    pub fn initialize_with(
        &self,
        request: AcpInitializeRequest,
    ) -> Result<AcpInitializeResponse, AcpError> {
        let auth_methods = self.advertised_auth_methods(&request)?;
        {
            let mut state = self.lock_state()?;
            state.client_capabilities = request.client_capabilities;
            state.client_info = request.client_info;
        }
        Ok(AcpInitializeResponse {
            protocol_version: ACP_PROTOCOL_VERSION,
            agent_capabilities: AcpAgentCapabilities {
                load_session: true,
                prompt_capabilities: AcpPromptCapabilities {
                    audio: false,
                    embedded_context: true,
                    image: true,
                },
                session_capabilities: json!({
                    "close": {},
                    "list": {},
                    "fork": {},
                }),
            },
            auth_methods,
            agent_info: AcpImplementation {
                name: "@mistralai/mistral-vibe".to_owned(),
                title: "Mistral Vibe".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        })
    }

    /// The method set the reference advertises: the browser methods under the
    /// provider predicate, the delegated variant and the terminal method under
    /// their client-capability gates, and nothing at all for a JetBrains
    /// client whose active provider is already usable.
    fn advertised_auth_methods(
        &self,
        request: &AcpInitializeRequest,
    ) -> Result<Vec<Value>, AcpError> {
        let capability = |name: &str| {
            request
                .client_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.meta.as_ref())
                .and_then(|meta| meta.get(name))
                == Some(&Value::Bool(true))
        };
        let mut auth_methods = self
            .auth
            .browser_methods(capability("browser-auth-delegated"));
        if capability("terminal-auth") {
            auth_methods.push(terminal_method());
        }
        let jetbrains = request
            .client_info
            .as_ref()
            .is_some_and(|info| info.name.starts_with("JetBrains."));
        if jetbrains && self.auth.status()?.can_use_active_provider {
            auth_methods.clear();
        }
        Ok(auth_methods)
    }

    /// Routes an `authenticate` call to the controller. Reference
    /// `Agent.authenticate`: the two browser methods are served here, a
    /// terminal method is executed by the client, and any other id is refused.
    pub async fn authenticate(
        &self,
        method_id: &str,
        arguments: &Value,
    ) -> Result<Value, AcpError> {
        self.auth.authenticate(method_id, arguments).await
    }

    /// Sends a request to the client and waits for its answer.
    pub(crate) async fn call_client(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let client = self.client.as_ref().ok_or_else(|| {
            AcpError::Internal(format!("no client is connected to answer `{method}`"))
        })?;
        client
            .request(method, params)
            .await
            .map_err(AcpError::ClientTool)
    }

    /// The live session the client names, or the reference's
    /// `SessionNotFoundError`.
    pub(crate) fn session_harness(&self, session_id: &str) -> Result<Arc<AcpHarness<D>>, AcpError> {
        self.lock_state()?
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))
    }

    /// A live session addressed either by its ACP identity or by the canonical
    /// identity it runs under. Reference `_find_live_session`.
    pub(crate) fn find_live_session(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<AcpHarness<D>>>, AcpError> {
        let state = self.lock_state()?;
        Ok(state.sessions.get(session_id).cloned().or_else(|| {
            state
                .sessions
                .values()
                .find(|harness| harness.canonical_id() == session_id)
                .cloned()
        }))
    }

    pub(crate) fn lock_state(&self) -> Result<MutexGuard<'_, AgentState<D>>, AcpError> {
        self.state.lock().map_err(|_| AcpError::StatePoisoned)
    }
}
