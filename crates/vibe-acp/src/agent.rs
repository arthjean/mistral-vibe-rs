//! The ACP agent: what an editor session is opened with, and the operations
//! that do not belong to one of the surfaces beside this file.

pub(crate) mod lifecycle;
pub(crate) mod listing;
pub(crate) mod services;
pub(crate) mod state;
pub(crate) mod telemetry;
pub(crate) mod transcript;
pub(crate) mod turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::TurnDriver;
use vibe_app_server::experiments::Credentials;
use vibe_app_server::release4::Release4Service;
use vibe_core::telemetry::{
    ClientTelemetry, ExperimentExposures, LaunchContext, NoClientTelemetry,
};

use crate::agent::state::AgentState;
use crate::auth::{
    AcpAuthEnvironment, AuthController, ProductionAuthEnvironment, default_vibe_home,
    terminal_method,
};
use crate::client_tools::{AcpClientPort, DEFAULT_CLIENT_TOOL_TIMEOUT, require_client_method};
use crate::commands;
use crate::protocol::{
    ACP_PROTOCOL_VERSION, AcpAgentCapabilities, AcpError, AcpImplementation, AcpInitializeRequest,
    AcpInitializeResponse, AcpPromptCapabilities,
};
use crate::session::{AcpHarness, Mode, Thinking, thinking_config_options};

pub(in crate::agent) const MAX_ADDITIONAL_DIRECTORIES: usize = 128;
pub(crate) const SESSION_LIST_PAGE_SIZE: usize = 50;

pub struct AcpAgent<D>
where
    D: TurnDriver,
{
    pub(in crate::agent) driver: Arc<D>,
    state: Mutex<AgentState<D>>,
    pub(in crate::agent) client: Option<Arc<dyn AcpClientPort>>,
    pub(in crate::agent) client_tool_timeout: Duration,
    pub(in crate::agent) session_root: Option<PathBuf>,
    auth: AuthController,
    pub(in crate::agent) credential_environment: String,
    pub(in crate::agent) production_cloud: bool,
    pub(in crate::agent) release4: Mutex<Option<Release4Service>>,
    /// Where an event the editor records reaches the datalake. Every session's
    /// app server is built over the same sink, so an editor-side event and a
    /// turn's own travel through one client, as they do upstream.
    pub(in crate::agent) telemetry: Arc<dyn ClientTelemetry>,
    /// What every session of this process needs to resolve its enrollment, or
    /// [`None`] for an adapter that publishes no telemetry and therefore has no
    /// census to fill.
    pub(in crate::agent) experiments: Option<AcpExperiments>,
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
            release4: Mutex::new(None),
            telemetry: Arc::new(NoClientTelemetry),
            experiments: None,
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
    pub fn with_production_cloud(mut self) -> Self {
        self.production_cloud = true;
        self
    }

    #[must_use]
    pub fn with_release4_service(mut self, service: Release4Service) -> Self {
        self.production_cloud = true;
        self.release4 = Mutex::new(Some(service));
        self
    }

    pub(crate) const fn vibe_code_enabled(&self) -> bool {
        self.production_cloud
    }

    #[must_use]
    pub fn advertised_commands(&self) -> Vec<Value> {
        commands::advertised(self.production_cloud)
    }

    pub fn initialize(&self) -> Result<AcpInitializeResponse, AcpError> {
        self.initialize_with(AcpInitializeRequest::default())
    }

    pub fn initialize_with(
        &self,
        request: AcpInitializeRequest,
    ) -> Result<AcpInitializeResponse, AcpError> {
        if request.protocol_version != ACP_PROTOCOL_VERSION {
            return Err(AcpError::UnsupportedProtocol(request.protocol_version));
        }
        let auth_methods = self.advertised_auth_methods(&request)?;
        self.lock_state()?
            .initialize(request.client_capabilities, request.client_info)?;
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
                    "list": {},
                    "fork": {},
                    "close": {},
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
                .meta
                .as_ref()
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
        self.require_initialized()?;
        self.auth.authenticate(method_id, arguments).await
    }

    /// The `auth/status` extension payload.
    pub fn auth_status(&self) -> Result<Value, AcpError> {
        self.auth.status_payload()
    }

    /// The `auth/signOut` extension method: the product's only credential
    /// removal path.
    pub fn auth_sign_out(&self) -> Result<Value, AcpError> {
        self.auth.sign_out()?;
        Ok(json!({}))
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        let mode = Mode::parse(mode_id)
            .ok_or_else(|| AcpError::InvalidParams(format!("unknown session mode `{mode_id}`")))?;
        let harness = self.session_harness(session_id)?;
        harness.service.lock().await.public_call(
            "session/overrides/write",
            json!({"sessionId": session_id, "mode": mode.as_str()}),
        )?;
        harness.update_settings(|settings| settings.mode = mode)
    }

    pub async fn set_config_option(
        &self,
        session_id: &str,
        option_id: &str,
        value: &str,
    ) -> Result<Vec<Value>, AcpError> {
        if option_id != "thinking" {
            return Err(AcpError::InvalidParams(
                "unknown config option; expected `thinking`".to_owned(),
            ));
        }
        let thinking = Thinking::parse(value).ok_or_else(|| {
            AcpError::InvalidParams("thinking must be off, low, medium, high, or max".to_owned())
        })?;
        let harness = self.session_harness(session_id)?;
        let mut params = json!({
            "sessionId": session_id,
            "thinking": thinking.enabled(),
        });
        if thinking.enabled() {
            params["reasoningEffort"] = json!(thinking.as_str());
        }
        harness
            .service
            .lock()
            .await
            .public_call("session/overrides/write", params)?;
        harness.update_settings(|settings| settings.thinking = thinking)?;
        Ok(thinking_config_options(thinking))
    }

    pub async fn request_permission(
        &self,
        session_id: &str,
        tool_call: Value,
        options: Vec<Value>,
    ) -> Result<Value, AcpError> {
        self.session_harness(session_id)?;
        self.call_client(
            "session/request_permission",
            json!({
                "sessionId": session_id,
                "toolCall": tool_call,
                "options": options,
            }),
        )
        .await
    }

    /// Calls an ACP client method on behalf of the caller, gated by the
    /// capabilities the client advertised at initialization.
    pub async fn client_tool(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let capabilities = self.lock_state()?.client_capabilities();
        require_client_method(method, &capabilities)?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::InvalidParams("sessionId is required".to_owned()))?;
        self.session_harness(session_id)?;
        self.call_client(method, params).await
    }

    async fn call_client(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| AcpError::UnsupportedClientFlow(method.to_owned()))?;
        tokio::time::timeout(self.client_tool_timeout, client.request(method, params))
            .await
            .map_err(|_| AcpError::ClientToolTimeout(method.to_owned()))?
            .map_err(AcpError::ClientTool)
    }

    pub(crate) fn session_harness(&self, session_id: &str) -> Result<Arc<AcpHarness<D>>, AcpError> {
        self.require_initialized()?;
        self.lock_state()?
            .active(session_id)
            .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))
    }

    pub(crate) fn require_initialized(&self) -> Result<(), AcpError> {
        if self.lock_state()?.initialized {
            Ok(())
        } else {
            Err(AcpError::NotInitialized)
        }
    }

    pub(crate) fn lock_state(&self) -> Result<MutexGuard<'_, AgentState<D>>, AcpError> {
        self.state.lock().map_err(|_| AcpError::StatePoisoned)
    }
}
