//! ACP agent: session lifecycle, settings, and client-facing operations.

pub(crate) mod state;
pub(crate) mod turn;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, SessionOptions, TurnDriver};
use vibe_app_server::release3::Release3Service;
use vibe_app_server::release4::{Release4Service, VibeCodeCloudConfig};
use vibe_app_server::server::AppServer;
use vibe_core::config::DotenvValues;
use vibe_core::observability::{LogLevel, log};
use vibe_core::telemetry::{ClientTelemetry, NoClientTelemetry};
use vibe_protocol::{
    CallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, TerminalEmulator,
};

use crate::agent::state::AgentState;
use crate::auth::{
    AcpAuthEnvironment, AuthController, ProductionAuthEnvironment, default_vibe_home,
    terminal_method,
};
use crate::client_tools::{
    AcpClientPort, AcpClientToolFactory, DEFAULT_CLIENT_TOOL_TIMEOUT, declared_client_tools,
    require_client_method,
};
use crate::commands;
use crate::history::{
    MAX_HISTORY_PAGE_SIZE, all_history_entries, history_page, parse_history_user_message_id,
};
use crate::mcp::project_acp_mcp_servers;
use crate::protocol::{
    ACP_PROTOCOL_VERSION, AcpAgentCapabilities, AcpError, AcpForkSession, AcpHistoryPage,
    AcpImplementation, AcpInitializeRequest, AcpInitializeResponse, AcpLoadSession, AcpNewSession,
    AcpPromptCapabilities, AcpSession, AcpSessionInfo, AcpSessionList, AcpSessionUpdate,
    AcpTelemetryNotification,
};
use crate::session::{
    AcpHarness, ActivePhase, SessionSettings, Thinking, acp_session_info, decode_session_cursor,
    encode_session_cursor, ensure_matching_cwd, metadata_session_id, require_absolute_cwd,
    same_path, session_options, session_response, thinking_config_options, validate_session_paths,
};
use crate::updates::history_entry_updates;

/// The two event names the reference routes off `telemetry/send`; every other
/// one is ignored with a warning.
const AT_MENTION_INSERTED_EVENT: &str = "vibe.at_mention_inserted";
const USER_RATING_FEEDBACK_EVENT: &str = "vibe.user_rating_feedback";

const MAX_ADDITIONAL_DIRECTORIES: usize = 128;
const MAX_SESSION_LIST_SCAN: usize = 100_000;
const SESSION_LIST_SCAN_PAGE: usize = 500;
pub(crate) const SESSION_LIST_PAGE_SIZE: usize = 50;

pub struct AcpAgent<D>
where
    D: TurnDriver,
{
    driver: Arc<D>,
    state: Mutex<AgentState<D>>,
    client: Option<Arc<dyn AcpClientPort>>,
    client_tool_timeout: Duration,
    session_root: Option<PathBuf>,
    auth: AuthController,
    credential_environment: String,
    production_cloud: bool,
    release4: Mutex<Option<Release4Service>>,
    /// Where an event the editor records reaches the datalake. Every session's
    /// app server is built over the same sink, so an editor-side event and a
    /// turn's own travel through one client, as they do upstream.
    telemetry: Arc<dyn ClientTelemetry>,
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
        })
    }

    /// Installs the telemetry client every session's app server ships a
    /// client-recorded event through.
    #[must_use]
    pub fn with_client_telemetry(mut self, telemetry: Arc<dyn ClientTelemetry>) -> Self {
        self.telemetry = telemetry;
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

    pub fn new_session(&self, request: AcpNewSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(
            &request.cwd,
            request.additional_directories.as_deref(),
            MAX_ADDITIONAL_DIRECTORIES,
        )?;
        let additional_directories = request.additional_directories.unwrap_or_default();
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let session_id = self.lock_state()?.mint_session_id();
        let settings = SessionSettings::default();
        let mut service = self.new_service(&request.cwd, &additional_directories)?;
        let app_session_id = service.start_session(&session_options(
            &request.cwd,
            Some(session_id),
            additional_directories,
            mcp_servers,
            None,
            &settings,
        ))?;
        self.publish_session(service, &app_session_id)
    }

    pub fn load_session(&self, request: AcpLoadSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(
            &request.cwd,
            Some(&request.additional_directories),
            MAX_ADDITIONAL_DIRECTORIES,
        )?;
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let requested_id = request.session_id.clone();
        self.lock_state()?.begin_load(&requested_id)?;
        let loaded = self.attach_saved_session(&request, mcp_servers);
        let (session_id, harness) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.lock_state()?.abort_load(&requested_id);
                return Err(error);
            }
        };
        let settings = harness.settings()?;
        self.lock_state()?
            .finish_load(&requested_id, session_id.clone(), harness)?;
        Ok(session_response(session_id, &settings))
    }

    fn attach_saved_session(
        &self,
        request: &AcpLoadSession,
        mcp_servers: Vec<Value>,
    ) -> Result<(String, Arc<AcpHarness<D>>), AcpError> {
        let mut probe = self.new_service(&request.cwd, &request.additional_directories)?;
        let result =
            probe.public_call("session/resume", json!({"sessionId": request.session_id}))?;
        let session_id = metadata_session_id(&result)?;
        if session_id != request.session_id {
            return Err(AcpError::InvalidResponse(format!(
                "requested session `{}` resumed as `{session_id}`",
                request.session_id
            )));
        }
        let persisted_cwd = probe.session(&session_id)?.working_directory;
        ensure_matching_cwd(&request.cwd, &persisted_cwd, "load")?;
        let settings = SessionSettings::from_release3_result(&result);
        probe.shutdown()?;

        let service = self.new_service(&request.cwd, &request.additional_directories)?;
        let harness = self.attach_session(
            service,
            &session_id,
            session_options(
                &request.cwd,
                None,
                request.additional_directories.clone(),
                mcp_servers,
                Some(session_id.clone()),
                &settings,
            ),
            "loaded",
        )?;
        Ok((session_id, harness))
    }

    pub fn fork_session(&self, request: AcpForkSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(
            &request.cwd,
            Some(&request.additional_directories),
            MAX_ADDITIONAL_DIRECTORIES,
        )?;
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let mut probe = self.new_service(&request.cwd, &request.additional_directories)?;
        let source =
            probe.public_call("session/resume", json!({"sessionId": request.session_id}))?;
        let source_id = metadata_session_id(&source)?;
        let source_cwd = probe.session(&source_id)?.working_directory;
        ensure_matching_cwd(&request.cwd, &source_cwd, "fork")?;
        let keep_messages = self.fork_keep_messages(&mut probe, &source_id, &request)?;
        let settings = self
            .live_session_settings(&request.session_id)?
            .unwrap_or_else(|| SessionSettings::from_release3_result(&source));
        let mut fork_params = json!({
            "sessionId": source_id,
            "newSessionId": request.new_session_id,
            "config": settings.as_config(),
        });
        if let Some(keep_messages) = keep_messages {
            fork_params["keepMessages"] = json!(keep_messages);
        }
        let result = probe.public_call("session/fork", fork_params)?;
        let session_id = metadata_session_id(&result)?;
        probe.shutdown()?;

        let service = self.new_service(&request.cwd, &request.additional_directories)?;
        let harness = self.attach_session(
            service,
            &session_id,
            session_options(
                &request.cwd,
                None,
                request.additional_directories,
                mcp_servers,
                Some(session_id.clone()),
                &settings,
            ),
            "forked",
        )?;
        let settings = harness.settings()?;
        self.lock_state()?
            .insert_active(session_id.clone(), harness);
        Ok(session_response(session_id, &settings))
    }

    fn fork_keep_messages(
        &self,
        probe: &mut HeadlessService<D>,
        source_id: &str,
        request: &AcpForkSession,
    ) -> Result<Option<usize>, AcpError> {
        let Some(message_id) = request.message_id.as_deref() else {
            return Ok(None);
        };
        let history = all_history_entries(probe, source_id)?;
        let anchor = self.resolve_user_message_anchor(&request.session_id, message_id)?;
        crate::history::fork_source_keep_messages(&history, anchor, message_id).map(Some)
    }

    pub fn list_sessions(
        &self,
        cwd: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<AcpSessionList, AcpError> {
        self.require_initialized()?;
        if let Some(cwd) = cwd {
            require_absolute_cwd(cwd)?;
        }
        let offset = decode_session_cursor(cursor)?;
        let working_directory = cwd.map_or_else(current_directory, ToOwned::to_owned);
        let live = self.live_sessions(cwd)?;
        let mut service = self.new_service(&working_directory, &[])?;
        let scan = self.scan_saved_sessions(&mut service, cwd, offset, &live)?;
        service.shutdown()?;

        let mut sessions = scan
            .saved
            .iter()
            .map(|session| {
                let mut info = acp_session_info(session)?;
                if live.contains_key(&info.session_id) {
                    info.meta.insert("active".to_owned(), json!(true));
                }
                Ok(info)
            })
            .collect::<Result<Vec<_>, AcpError>>()?;
        // Live sessions the store has not recorded only have a stable position
        // once the whole listing is known, so they are appended at the end.
        if scan.complete {
            sessions.extend(
                live.into_iter()
                    .filter(|(session_id, _)| !scan.matched.contains(session_id))
                    .map(|(session_id, cwd)| AcpSessionInfo {
                        session_id,
                        cwd,
                        title: None,
                        updated_at: None,
                        additional_directories: None,
                        meta: BTreeMap::from([("active".to_owned(), json!(true))]),
                    }),
            );
        }
        let start = offset.min(sessions.len());
        let end = start
            .saturating_add(SESSION_LIST_PAGE_SIZE)
            .min(sessions.len());
        let exhausted = scan.complete && end == sessions.len();
        Ok(AcpSessionList {
            sessions: sessions.drain(start..end).collect(),
            next_cursor: (!exhausted).then(|| encode_session_cursor(end)),
        })
    }

    /// Reads saved sessions until the requested page can be served and every
    /// live session has been located. Live sessions sort first in the store
    /// (most recently updated), so this normally stops after one page.
    fn scan_saved_sessions(
        &self,
        service: &mut HeadlessService<D>,
        cwd: Option<&str>,
        offset: usize,
        live: &BTreeMap<String, String>,
    ) -> Result<SavedScan, AcpError> {
        let needed = offset.saturating_add(SESSION_LIST_PAGE_SIZE);
        let mut scan = SavedScan {
            saved: Vec::new(),
            matched: BTreeSet::new(),
            complete: false,
        };
        let mut app_offset = 0_usize;
        loop {
            let mut params = json!({"offset": app_offset, "limit": SESSION_LIST_SCAN_PAGE});
            if let Some(cwd) = cwd {
                params["cwd"] = json!(cwd);
            }
            let result = service.public_call("session/list", params)?;
            let page = result
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for session in &page {
                if let Some(session_id) = session.get("id").and_then(Value::as_str)
                    && live.contains_key(session_id)
                {
                    scan.matched.insert(session_id.to_owned());
                }
            }
            let received = page.len();
            scan.saved.extend(page);
            if scan.saved.len() > MAX_SESSION_LIST_SCAN {
                return Err(AcpError::InvalidResponse(format!(
                    "session/list exceeded the {MAX_SESSION_LIST_SCAN}-session scan limit"
                )));
            }
            // `SessionListResponse` declares the page and nothing else, so the
            // scan advances by what it received and stops on a short page,
            // which is the same signal a cursor carried.
            if received < SESSION_LIST_SCAN_PAGE {
                scan.complete = true;
                return Ok(scan);
            }
            if scan.saved.len() >= needed && scan.matched.len() == live.len() {
                return Ok(scan);
            }
            app_offset = app_offset.saturating_add(received);
        }
    }

    fn live_sessions(&self, cwd: Option<&str>) -> Result<BTreeMap<String, String>, AcpError> {
        Ok(self
            .lock_state()?
            .sessions
            .iter()
            .filter_map(|(session_id, slot)| {
                let harness = slot.harness()?;
                (!cwd.is_some_and(|filter| !same_path(filter, &harness.cwd)))
                    .then(|| (session_id.clone(), harness.cwd.clone()))
            })
            .collect())
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        let mode = crate::session::Mode::parse(mode_id)
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

    pub(crate) async fn history(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<AcpHistoryPage, AcpError> {
        if !(1..=MAX_HISTORY_PAGE_SIZE).contains(&limit) {
            return Err(AcpError::InvalidParams(format!(
                "history limit must be between 1 and {MAX_HISTORY_PAGE_SIZE}"
            )));
        }
        let harness = self.session_harness(session_id)?;
        let mut service = harness.service.lock().await;
        let entries = history_page(&mut service, session_id, offset, limit)?;
        Ok(AcpHistoryPage {
            next_offset: (entries.len() == limit).then_some(offset.saturating_add(entries.len())),
            entries,
        })
    }

    /// Replays a saved session as ACP updates so a reconnecting client can
    /// rebuild the transcript.
    pub async fn replay_history(
        &self,
        session_id: &str,
        mut emit: impl FnMut(AcpSessionUpdate) -> Result<(), AcpError>,
    ) -> Result<(), AcpError> {
        let mut offset = 0;
        loop {
            let page = self
                .history(session_id, offset, MAX_HISTORY_PAGE_SIZE)
                .await?;
            for (index, entry) in page.entries.into_iter().enumerate() {
                for update in history_entry_updates(&entry, offset.saturating_add(index))? {
                    emit(AcpSessionUpdate {
                        session_id: session_id.to_owned(),
                        update,
                    })?;
                }
            }
            let Some(next) = page.next_offset else {
                return Ok(());
            };
            if next <= offset {
                return Err(AcpError::InvalidResponse(
                    "history cursor did not advance".to_owned(),
                ));
            }
            offset = next;
        }
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let harness = self.session_harness(session_id)?;
        let phase = harness.phase()?;
        if phase == ActivePhase::Idle {
            return Ok(());
        }
        harness.request_cancel();
        if let ActivePhase::Running(turn_id) = phase {
            harness
                .service
                .lock()
                .await
                .interrupt(session_id, &turn_id)?;
        }
        Ok(())
    }

    pub async fn close_session(&self, session_id: &str) -> Result<(), AcpError> {
        self.require_initialized()?;
        let Some(harness) = self.lock_state()?.begin_close(session_id)? else {
            return Ok(());
        };
        harness.request_cancel();
        let mut service = harness.service.lock().await;
        // A concurrent disconnect may have finished this session while the
        // service lock was contended.
        if self.lock_state()?.is_closed(session_id) {
            return Ok(());
        }
        let result = stop_session(&mut service, &harness).await;
        self.lock_state()?
            .finish_close(session_id, harness.clone(), result.is_ok());
        result
    }

    pub async fn disconnect(&self) -> Result<(), AcpError> {
        let sessions = self.lock_state()?.take_live_sessions();
        let mut first_error = None;
        for (session_id, harness) in sessions {
            harness.request_cancel();
            let mut service = harness.service.lock().await;
            if self.lock_state()?.is_closed(&session_id) {
                continue;
            }
            match stop_session(&mut service, &harness).await {
                Ok(()) => self.lock_state()?.tombstone(session_id),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
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

    fn new_service(
        &self,
        working_directory: &str,
        additional_directories: &[String],
    ) -> Result<HeadlessService<D>, AcpError> {
        let (capabilities, client_info) = self.lock_state()?.client_context();
        let mut server = AppServer::default()
            .using_session_tool_factory(Arc::new(AcpClientToolFactory {
                client: self.client.clone(),
                capabilities: capabilities.clone(),
                timeout: self.client_tool_timeout,
            }))
            .using_client_telemetry(Arc::clone(&self.telemetry));
        if let Some(release4) = self.production_release4()? {
            server = server.using_release4_service(release4);
        }
        if let Some(session_root) = &self.session_root {
            let release3 = Release3Service::for_runtime_session_root(
                session_root,
                Path::new(working_directory),
            )
            .with_allowed_roots(additional_directories.iter().map(PathBuf::from).collect());
            // The editor session starts here, so an older configuration file is
            // brought forward before the first read. A failure to write is
            // carried by the configuration snapshot, not raised.
            release3
                .migrate_configuration()
                .map_err(|error| AcpError::Configuration(error.to_string()))?;
            server = server.using_release3_service(release3);
        }
        Ok(
            HeadlessService::new_interactive_shared_with_server_and_client(
                self.driver.clone(),
                server,
                ClientInfo {
                    name: client_info
                        .as_ref()
                        .map_or_else(|| "vibe_acp_client".to_owned(), |info| info.name.clone()),
                    version: client_info
                        .as_ref()
                        .map_or_else(|| "unknown".to_owned(), |info| info.version.clone()),
                    title: client_info.and_then(|info| info.title),
                    entrypoint: ClientEntrypoint::Acp,
                    terminal_emulator: TerminalEmulator::Unknown,
                },
                ClientCapabilities {
                    callback_kinds: self
                        .client
                        .is_some()
                        .then_some(CallbackKind::Approval)
                        .into_iter()
                        .collect(),
                    client_tools: declared_client_tools(&capabilities),
                    // The bridge renders every notification the server sends, so
                    // it mutes none of them.
                    disabled_notifications: Vec::new(),
                },
            )?,
        )
    }

    /// Cloud services are resolved lazily so sessions start without a
    /// provider credential.
    fn production_release4(&self) -> Result<Option<Release4Service>, AcpError> {
        if !self.production_cloud {
            return Ok(None);
        }
        let mut cached = self.release4.lock().map_err(|_| AcpError::StatePoisoned)?;
        if let Some(service) = cached.as_ref() {
            return Ok(Some(service.clone()));
        }
        // The session root sits inside the vibe home, so the global dotenv file
        // is resolvable here and a key kept there starts the cloud services the
        // same way an exported one does.
        let dotenv = self
            .session_root
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(DotenvValues::default, DotenvValues::global);
        let Some(api_key) = dotenv
            .variable(&self.credential_environment)
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let config = VibeCodeCloudConfig::from_credential(api_key)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        let service = Release4Service::production(config)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        *cached = Some(service.clone());
        Ok(Some(service))
    }

    /// Starts a session on `service` and checks the canonical layer honored the
    /// requested identity.
    fn attach_session(
        &self,
        mut service: HeadlessService<D>,
        session_id: &str,
        options: SessionOptions,
        operation: &str,
    ) -> Result<Arc<AcpHarness<D>>, AcpError> {
        let attached_id = service.start_session(&options)?;
        if attached_id != session_id {
            return Err(AcpError::InvalidResponse(format!(
                "{operation} session `{session_id}` was attached as `{attached_id}`"
            )));
        }
        Ok(Arc::new(AcpHarness::adopt(service, session_id)?))
    }

    fn publish_session(
        &self,
        service: HeadlessService<D>,
        session_id: &str,
    ) -> Result<AcpSession, AcpError> {
        let harness = Arc::new(AcpHarness::adopt(service, session_id)?);
        let settings = harness.settings()?;
        self.lock_state()?
            .insert_active(session_id.to_owned(), harness);
        Ok(session_response(session_id.to_owned(), &settings))
    }

    /// Serves the `telemetry/send` extension notification.
    ///
    /// Reference `AcpAgent.ext_notification` (`vibe/acp/agent.py:1002-1031`):
    /// the payload is validated, a session the agent does not hold drops the
    /// event, two names are routed and every other one is ignored with a
    /// warning naming it. The rating carries the active model alias and
    /// correlates with the last request, which is what the reference reads off
    /// its own configuration and telemetry client.
    pub async fn telemetry_notification(&self, params: &Value) -> Result<(), AcpError> {
        let notification = serde_json::from_value::<AcpTelemetryNotification>(params.clone())
            .map_err(|error| {
                AcpError::InvalidParams(format!("invalid ACP telemetry notification: {error}"))
            })?;
        let Some(harness) = self.lock_state()?.active(&notification.session_id) else {
            return Ok(());
        };
        let (properties, correlate) = match notification.event.as_str() {
            AT_MENTION_INSERTED_EVENT => (notification.properties, false),
            USER_RATING_FEEDBACK_EVENT => {
                let rating = notification
                    .properties
                    .get("rating")
                    .cloned()
                    .unwrap_or_else(|| json!(0));
                let model = self
                    .active_model_alias(&harness, &notification.session_id)
                    .await;
                (
                    [
                        ("rating".to_owned(), rating),
                        ("model".to_owned(), json!(model)),
                    ]
                    .into_iter()
                    .collect(),
                    true,
                )
            }
            event => {
                log(
                    LogLevel::Warning,
                    &format!("Ignoring unsupported ACP telemetry event: {event}"),
                );
                return Ok(());
            }
        };
        harness.service.lock().await.public_call(
            "telemetry/record",
            json!({
                "sessionId": notification.session_id,
                "name": notification.event,
                "properties": properties,
                "correlateLastRequest": correlate,
            }),
        )?;
        Ok(())
    }

    /// The alias the session's configuration publishes for the model a turn
    /// would run on, which is what the reference reads as
    /// `config.current.active_model.alias`. A configuration that answers none
    /// reports the same `unknown` an absent label reports elsewhere.
    async fn active_model_alias(&self, harness: &AcpHarness<D>, session_id: &str) -> String {
        harness
            .service
            .lock()
            .await
            .public_call("config/read", json!({"sessionId": session_id}))
            .ok()
            .and_then(|result| {
                result
                    .get("config")?
                    .get("activeModel")?
                    .get("alias")?
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .filter(|alias| !alias.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    }

    pub(crate) fn session_harness(&self, session_id: &str) -> Result<Arc<AcpHarness<D>>, AcpError> {
        self.require_initialized()?;
        self.lock_state()?
            .active(session_id)
            .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))
    }

    fn require_initialized(&self) -> Result<(), AcpError> {
        if self.lock_state()?.initialized {
            Ok(())
        } else {
            Err(AcpError::NotInitialized)
        }
    }

    pub(crate) fn lock_state(&self) -> Result<MutexGuard<'_, AgentState<D>>, AcpError> {
        self.state.lock().map_err(|_| AcpError::StatePoisoned)
    }

    fn live_session_settings(&self, session_id: &str) -> Result<Option<SessionSettings>, AcpError> {
        self.lock_state()?
            .active(session_id)
            .map(|harness| harness.settings())
            .transpose()
    }

    /// Resolves the history index a fork should branch from, accepting both
    /// replayed identifiers and IDs minted for live turns.
    fn resolve_user_message_anchor(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<usize, AcpError> {
        if let Some(anchor) = parse_history_user_message_id(message_id) {
            return Ok(anchor);
        }
        if let Some(harness) = self.lock_state()?.active(session_id)
            && let Some(anchor) = harness.user_message_anchor(message_id)?
        {
            return Ok(anchor);
        }
        Err(AcpError::InvalidParams(format!(
            "cannot fork from unknown user messageId `{message_id}`"
        )))
    }
}

struct SavedScan {
    saved: Vec<Value>,
    matched: BTreeSet<String>,
    complete: bool,
}

/// Interrupts any running turn, closes the canonical session, and stops the
/// service. Stops at the first failure so a half-closed session is reported
/// rather than silently tombstoned.
async fn stop_session<D>(
    service: &mut HeadlessService<D>,
    harness: &AcpHarness<D>,
) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    if let Some(turn_id) = harness.running_turn_id()? {
        service.interrupt(&harness.session_id, &turn_id)?;
    }
    service.close_session(&harness.session_id).await?;
    service.shutdown()?;
    Ok(())
}

fn current_directory() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}
