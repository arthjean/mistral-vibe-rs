//! The programmatic surface: one process driving sessions without a client.
//!
//! [`HeadlessService`] owns an in-process client and a driver, and turns the
//! calls a program makes into the same dispatch a connected editor would
//! produce. Everything a person would answer interactively is answered here
//! through the interactive tools instead, or refused.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use vibe_core::engine::TurnOutcome;

use super::in_process::{ServerProjectionObserver, public_history_entry_identity};
use super::interactive::*;
use super::*;

pub struct HeadlessService<D> {
    pub(super) client: InProcessClient,
    pub(super) driver: Arc<D>,
    pub(super) interactive_callbacks:
        Option<tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>>,
    pub(super) interactive_backlog: VecDeque<InteractiveCallbackRequest>,
    pub(super) pending_interactive_callbacks: HashMap<String, PendingInteractiveCallback>,
    /// The sessions the Unified Harness serves, when the server selected it.
    pub(super) unified: Option<super::unified::UnifiedLink>,
}

/// Runs a reserved turn to its end, off the service: the driver runs a turn
/// of a legacy session, and a unified session runs its own, which is waited
/// on.
pub struct TurnRunner<D> {
    driver: Arc<D>,
    unified: Option<Arc<super::unified::Shared>>,
}

impl<D> Clone for TurnRunner<D> {
    fn clone(&self) -> Self {
        Self {
            driver: Arc::clone(&self.driver),
            unified: self.unified.clone(),
        }
    }
}

impl<D> TurnRunner<D>
where
    D: TurnDriver,
{
    /// The turn `reservation` holds, run to its end.
    ///
    /// # Errors
    ///
    /// The turn failed, or its driver did.
    pub async fn run(
        &self,
        reservation: &TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> Result<TurnOutcome, DriverError> {
        if let Some(shared) = self
            .unified
            .as_ref()
            .filter(|shared| shared.serves(&reservation.session_id))
        {
            return super::unified::settled_turn(
                Arc::clone(shared),
                reservation.session_id.clone(),
                reservation.turn_id.clone(),
            )
            .await;
        }
        self.driver.run_observed(reservation, observer).await
    }
}

/// An observer for a turn nothing engine-side reports on.
struct Unobserved;

impl EventObserver for Unobserved {
    fn observe(&self, _event: &vibe_core::events::EventEnvelope) -> Result<(), String> {
        Ok(())
    }
}

impl<D> HeadlessService<D>
where
    D: TurnDriver,
{
    pub fn new(driver: D) -> Result<Self, ClientError> {
        Self::new_shared(Arc::new(driver))
    }

    pub fn new_shared(driver: Arc<D>) -> Result<Self, ClientError> {
        Self::new_shared_with_server(driver, AppServer::default())
    }

    pub fn new_shared_with_server(driver: Arc<D>, server: AppServer) -> Result<Self, ClientError> {
        Self::new_shared_with_server_and_client(
            driver,
            server,
            ClientInfo {
                name: "vibe-programmatic".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                title: None,
                entrypoint: ClientEntrypoint::Programmatic,
                terminal_emulator: TerminalEmulator::Unknown,
            },
            ClientCapabilities::default(),
        )
    }

    pub fn new_shared_with_server_and_client(
        driver: Arc<D>,
        server: AppServer,
        client_info: ClientInfo,
        capabilities: ClientCapabilities,
    ) -> Result<Self, ClientError> {
        let server = server.defaulting_secondary_provider(driver.summary_provider());
        let unified = server
            .harness_selection()
            .use_unified
            .then(|| super::unified::UnifiedLink::new(client_info.clone(), capabilities.clone()));
        Ok(Self {
            client: InProcessClient::connect_with_server_and_client(
                server,
                client_info,
                capabilities,
            )?,
            driver,
            interactive_callbacks: None,
            interactive_backlog: VecDeque::new(),
            pending_interactive_callbacks: HashMap::new(),
            unified,
        })
    }

    pub fn new_interactive_shared_with_server(
        driver: Arc<D>,
        server: AppServer,
    ) -> Result<Self, ClientError> {
        Self::new_interactive_shared_with_server_and_client(
            driver,
            server,
            ClientInfo {
                name: "vibe-cli".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                title: Some("Mistral Vibe".to_owned()),
                entrypoint: ClientEntrypoint::Cli,
                terminal_emulator: TerminalEmulator::Unknown,
            },
            ClientCapabilities {
                callback_kinds: vec![ClientCallbackKind::Approval, ClientCallbackKind::UserInput],
                ..ClientCapabilities::default()
            },
        )
    }

    pub fn new_interactive_shared_with_server_and_client(
        driver: Arc<D>,
        server: AppServer,
        client_info: ClientInfo,
        mut capabilities: ClientCapabilities,
    ) -> Result<Self, ClientError> {
        if !capabilities
            .callback_kinds
            .contains(&ClientCallbackKind::Approval)
        {
            capabilities
                .callback_kinds
                .push(ClientCallbackKind::Approval);
        }
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(MAX_INTERACTIVE_CALLBACKS);
        let plan_directory = driver.plan_directory();
        let server = server
            .defaulting_secondary_provider(driver.summary_provider())
            .using_surface_extension(
                Arc::new(InteractiveApprovalFactory {
                    sender: sender.clone(),
                }),
                Arc::new(InteractiveSessionToolFactory {
                    sender: CallbackChannel::Shared(sender),
                    plan_directory,
                }),
            );
        let unified = server
            .harness_selection()
            .use_unified
            .then(|| super::unified::UnifiedLink::new(client_info.clone(), capabilities.clone()));
        Ok(Self {
            client: InProcessClient::connect_with_server_and_client(
                server,
                client_info,
                capabilities,
            )?,
            driver,
            interactive_callbacks: Some(receiver),
            interactive_backlog: VecDeque::new(),
            pending_interactive_callbacks: HashMap::new(),
            unified,
        })
    }

    pub fn start_session(&mut self, options: &SessionOptions) -> Result<String, ClientError> {
        if let Some(link) = self.unified.as_mut() {
            let (session_id, mcp) = link.start(&self.client.server, options)?;
            if !mcp.is_empty() {
                self.client.pending_mcp.insert(session_id.clone(), mcp);
            }
            return Ok(session_id);
        }
        let session_id = self.client.start_session(options)?;
        self.probe_for_titles(&session_id);
        Ok(session_id)
    }

    /// A root session that titles itself learns whether a fast model serves
    /// its titles before it is handed back, as the reference's session open
    /// waits on the probe.
    fn probe_for_titles(&self, session_id: &str) {
        if self.client.session_titles_itself(session_id) {
            self.driver.probe_utility_models();
        }
    }

    /// The configuration and session service this session composes over.
    #[must_use]
    pub fn workspace_service(&self) -> crate::workspace::WorkspaceService {
        self.client.workspace_service()
    }

    pub async fn initialize_pending_mcp(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<String>, ClientError> {
        self.client
            .configure_pending_mcp_with_diagnostics(session_id)
            .await
    }

    fn fail_interactive_callbacks(
        &mut self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        message: &str,
    ) {
        let matches = |candidate_session: &str, candidate_turn: Option<&str>| {
            session_id.is_none_or(|expected| expected == candidate_session)
                && turn_id.is_none_or(|expected| candidate_turn == Some(expected))
        };
        let matches_queued = |candidate_session: &str| {
            session_id.is_none_or(|expected| expected == candidate_session)
        };
        let pending_ids = self
            .pending_interactive_callbacks
            .iter()
            .filter(|(_, pending)| matches(&pending.session_id, Some(pending.turn_id.as_str())))
            .map(|(callback_id, _)| callback_id.clone())
            .collect::<Vec<_>>();
        for callback_id in pending_ids {
            fail_pending_interactive_callback(
                self.pending_interactive_callbacks.remove(&callback_id),
                message,
            );
        }

        let mut retained = VecDeque::new();
        while let Some(request) = self.interactive_backlog.pop_front() {
            if matches_queued(interactive_request_session_id(&request)) {
                reject_interactive_request(request, message);
            } else {
                retain_interactive_request(&mut retained, request);
            }
        }
        if let Some(receiver) = self.interactive_callbacks.as_mut() {
            while let Ok(request) = receiver.try_recv() {
                if matches_queued(interactive_request_session_id(&request)) {
                    reject_interactive_request(request, message);
                } else {
                    retain_interactive_request(&mut retained, request);
                }
            }
        }
        self.interactive_backlog = retained;
    }

    /// The delegation this service's server sends `clientTool/*` requests
    /// through. Attaching a write side to it is what lets an adapter answer
    /// them for its own client.
    #[must_use]
    pub fn client_tools(&self) -> Arc<crate::client_tools::ClientToolBridge> {
        self.client.server.client_tools.clone()
    }

    #[must_use]
    pub fn driver(&self) -> Arc<D> {
        self.driver.clone()
    }

    /// What runs a reserved turn to its end, off this service.
    #[must_use]
    pub fn turn_runner(&self) -> TurnRunner<D> {
        TurnRunner {
            driver: Arc::clone(&self.driver),
            unified: self.unified.as_ref().map(|link| Arc::clone(&link.shared)),
        }
    }

    /// Whether the Unified Harness serves `session_id`.
    #[must_use]
    pub fn is_unified(&self, session_id: &str) -> bool {
        self.unified
            .as_ref()
            .is_some_and(|link| link.holds(session_id))
    }

    /// The unified session a call's parameters name, when they name one.
    fn unified_target(&self, method: &str, params: &Value) -> Option<String> {
        self.unified.as_ref()?;
        let named = params.get("sessionId").and_then(Value::as_str);
        match named {
            Some(session_id) if self.is_unified(session_id) => Some(session_id.to_owned()),
            // A session-optional read reaches the connection of the session
            // this client opened, as the reference's single host answers it.
            None if !matches!(method, "session/resume" | "session/continue") => self
                .unified
                .as_ref()
                .and_then(super::unified::UnifiedLink::any_session),
            _ => None,
        }
    }

    /// The accounting the Unified Harness last published for `session_id`,
    /// with its context window.
    #[must_use]
    pub fn unified_accounting(&self, session_id: &str) -> Option<(Value, u64)> {
        self.unified.as_ref()?.accounting(session_id)
    }

    /// The state the Unified Harness last published for `session_id`.
    #[must_use]
    pub fn unified_state(&self, session_id: &str) -> Option<Value> {
        self.unified.as_ref()?.state(session_id)
    }

    /// The child sessions the Unified Harness last published for `session_id`.
    #[must_use]
    pub fn unified_children(&self, session_id: &str) -> Option<Value> {
        self.unified
            .as_ref()?
            .state_field(session_id, "childSessions")
    }

    pub fn session(&mut self, session_id: &str) -> Result<SessionView, ClientError> {
        if let Some(link) = self.unified.as_ref().filter(|link| link.holds(session_id)) {
            let history = link.history(session_id)?;
            let mut view = self.client.server.session(session_id)?;
            view.snapshot = Some(vibe_core::events::ProjectionSnapshot {
                session_id: session_id.to_owned(),
                turn_id: None,
                handoff_cause: None,
                watermark: 0,
                lifecycle: vibe_core::events::LifecycleState::Idle,
                title: None,
                history,
            });
            return Ok(view);
        }
        self.client.session(session_id)
    }

    /// Opens a saved session on the Unified Harness with `method`, a
    /// `session/resume` or a `session/continue`.
    fn open_unified(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let Some(link) = self.unified.as_mut() else {
            return Err(ClientError::InvalidResponse(
                "the Unified Harness is not selected".to_owned(),
            ));
        };
        let (session_id, result, deferred) = link.open(&self.client.server, method, params)?;
        for work in deferred {
            if let DeferredWork::ConfigureMcp { configs, .. } = work {
                self.client.pending_mcp.insert(session_id.clone(), configs);
            }
        }
        Ok(result)
    }

    /// Dispatches `method` on a unified session's connection, its answer
    /// required now.
    fn unified_call(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let link = self.unified.as_mut().ok_or_else(|| {
            ClientError::InvalidResponse("the Unified Harness is not selected".to_owned())
        })?;
        link.dispatch(&self.client.server, session_id, method, params)?
            .now()
    }

    pub fn public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        if self.unified.is_some() && matches!(method, "session/resume" | "session/continue") {
            return self.open_unified(method, params);
        }
        if let Some(session_id) = self.unified_target(method, &params) {
            return self.unified_call(&session_id, method, params);
        }
        let requested = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let result = self.client.public_call(method, params)?;
        // A resumed or continued session opens a root of its own, which the
        // reference builds, probe included; a fork copies its source and
        // does not probe (`vibe/app_server/_runtime.py:1073`).
        if matches!(
            method,
            "session/resume"
                | "session/continue"
                | "internal/session/resume"
                | "internal/session/continue"
        ) && let Some(session_id) = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(requested)
        {
            self.probe_for_titles(&session_id);
        }
        Ok(result)
    }

    pub async fn public_call_async(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PublicDispatch, ClientError> {
        if self.unified.is_some() && matches!(method, "session/resume" | "session/continue") {
            let result = self.open_unified(method, params)?;
            return Ok(PublicDispatch {
                result,
                notifications: Vec::new(),
            });
        }
        self.begin_public_call(method, params)?.complete().await
    }

    pub fn begin_public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PendingPublicCall, ClientError> {
        if let Some(session_id) = self.unified_target(method, &params)
            && let Some(link) = self.unified.as_mut()
        {
            let dispatched = link.dispatch(&self.client.server, &session_id, method, params)?;
            return Ok(PendingPublicCall::unified(
                &self.client.server,
                method,
                dispatched,
            ));
        }
        self.client.begin_public_call(method, params)
    }

    pub fn interactive_update_channel_after(
        &self,
        session_id: &str,
        turn_id: &str,
        _event_id: u64,
    ) -> Result<
        (
            Arc<dyn EventObserver>,
            tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
        ),
        ClientError,
    > {
        if let Some(link) = self.unified.as_ref().filter(|link| link.holds(session_id)) {
            return Ok((Arc::new(Unobserved), link.updates(turn_id)?));
        }
        let seed = self
            .client
            .server
            .live_projection_seed(session_id, turn_id)?;
        // The turn is starting: its user message is a step of the session's
        // accounting, which the transport records when it announces the turn.
        self.client.server.turn_started(session_id, turn_id)?;
        let mut reducer = ProjectionReducer::for_turn(session_id, turn_id);
        reducer
            .restore(seed.clone(), 0)
            .map_err(|error| ClientError::InvalidResponse(error.to_string()))?;
        let emitted = seed
            .history
            .iter()
            .map(|entry| {
                serde_json::to_value(entry)
                    .map(|encoded| (public_history_entry_identity(entry), encoded))
                    .map_err(ClientError::Json)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<ProgrammaticUpdate>(MAX_PROGRAMMATIC_UPDATES);
        Ok((
            Arc::new(ServerProjectionObserver {
                server: self.client.server.clone(),
                session_id: session_id.to_owned(),
                turn_id: turn_id.to_owned(),
                reducer: Mutex::new(reducer),
                emitted: Mutex::new(emitted),
                sender,
            }),
            receiver,
        ))
    }

    /// Reference `_schedule_title_generation_events`: an observer that starts
    /// the background title each completed model step makes due. A client
    /// running a turn on its own composes it with the observer it reads.
    pub fn title_scheduler(&self) -> Arc<dyn EventObserver>
    where
        D: 'static,
    {
        Arc::new(TitleScheduler {
            server: self.client.server.clone(),
            driver: Arc::clone(&self.driver),
        })
    }

    pub fn request_callback_with_detail(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        title: impl Into<String>,
        detail: Value,
    ) -> Result<(String, PublicHistoryEntry), ClientError> {
        let (callback_id, frames) = self
            .client
            .connection
            .request_callback_with_detail(session_id, turn_id, kind, title, detail)?;
        // The delivery is the last frame; the status update before it is for
        // the wire, not for this in-process caller.
        let request_bytes = frames
            .last()
            .cloned()
            .ok_or_else(|| ClientError::InvalidResponse("callback was not delivered".to_owned()))?;
        let request = match decode_frame(&request_bytes).map_err(ServerError::Protocol)? {
            Envelope::Request(request) if request.method == "callback/call" => request,
            _ => {
                return Err(ClientError::InvalidResponse(
                    "callback delivery was not a callback/call request".to_owned(),
                ));
            }
        };
        let callback = request
            .params
            .get("callback")
            .cloned()
            .ok_or_else(|| {
                ClientError::InvalidResponse("callback delivery omitted callback".to_owned())
            })
            .and_then(|value| {
                serde_json::from_value::<PublicHistoryEntry>(value).map_err(ClientError::Json)
            })?;
        let acknowledgment = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": request.id,
            "result": {
                "callbackId": callback_id,
                "accepted": true,
            },
        }))
        .map_err(ClientError::Json)?;
        let batch = self.client.connection.dispatch(&acknowledgment);
        if batch.close_after_flush || !batch.outbound.is_empty() || !batch.deferred.is_empty() {
            return Err(ClientError::InvalidResponse(
                "callback delivery acknowledgment was rejected".to_owned(),
            ));
        }
        Ok((callback_id, callback))
    }

    pub fn drain_callbacks(&mut self) -> Result<Vec<PublicHistoryEntry>, ClientError> {
        let unified = match self.unified.as_ref() {
            Some(link) => link.drain_callbacks()?,
            None => Vec::new(),
        };
        let mut entries = self.drain_interactive_callbacks()?;
        entries.extend(unified);
        Ok(entries)
    }

    fn drain_interactive_callbacks(&mut self) -> Result<Vec<PublicHistoryEntry>, ClientError> {
        let mut requests = std::mem::take(&mut self.interactive_backlog);
        if let Some(receiver) = self.interactive_callbacks.as_mut() {
            while let Ok(request) = receiver.try_recv() {
                requests.push_back(request);
            }
        }
        let request_count = requests.len();
        let mut entries = Vec::with_capacity(request_count);
        for _ in 0..request_count {
            let Some(request) = requests.pop_front() else {
                break;
            };
            // A clearing occupies no callback slot: it names the running turn
            // and hands it a control, which is why it settles here rather than
            // queueing behind whatever callback is open.
            if request.is_turn_control() {
                settle_turn_control(&self.client.server, &*self.driver, request);
                continue;
            }
            let (session_id, title, detail, kind) = match &request {
                InteractiveCallbackRequest::Approval {
                    session_id,
                    request,
                    ..
                } => {
                    let working_directory = self
                        .client
                        .server
                        .session(session_id)
                        .ok()
                        .map(|session| std::path::PathBuf::from(session.working_directory));
                    let remote = self
                        .client
                        .server
                        .tool_registry(session_id)
                        .ok()
                        .and_then(|tools| tools.remote_origin(&request.tool));
                    (
                        session_id.clone(),
                        format!("Approve {}?", request.tool),
                        approval_callback_detail(
                            request,
                            working_directory.as_deref(),
                            remote.as_ref(),
                        ),
                        EngineCallbackKind::Approval,
                    )
                }
                InteractiveCallbackRequest::Tool {
                    session_id,
                    title,
                    detail,
                    ..
                } => (
                    session_id.clone(),
                    title.clone(),
                    detail.clone(),
                    EngineCallbackKind::UserInput,
                ),
                _ => continue,
            };
            if self
                .pending_interactive_callbacks
                .values()
                .any(|pending| pending.session_id == session_id)
            {
                retain_interactive_request(&mut self.interactive_backlog, request);
                continue;
            }
            let session = match self.client.server.session(&session_id) {
                Ok(session) => session,
                Err(_) => {
                    reject_interactive_request(request, "session is no longer available");
                    continue;
                }
            };
            let turn_id = match session.active_turn {
                Some(turn_id) => turn_id,
                None => {
                    reject_interactive_request(request, "turn is no longer active");
                    continue;
                }
            };
            if session.pending_callback.is_some() {
                retain_interactive_request(&mut self.interactive_backlog, request);
                continue;
            }
            match self.request_callback_with_detail(&session_id, &turn_id, kind, title, detail) {
                Ok((callback_id, entry)) => {
                    let response = match request {
                        InteractiveCallbackRequest::Approval { response, .. } => {
                            InteractiveCallbackResponse::Approval(response)
                        }
                        InteractiveCallbackRequest::Tool { response, .. } => {
                            InteractiveCallbackResponse::Tool(response)
                        }
                        _ => continue,
                    };
                    if self
                        .pending_interactive_callbacks
                        .contains_key(&callback_id)
                    {
                        fail_interactive_response(response, "callback ID was reused");
                        return Err(ClientError::InvalidResponse(
                            "callback identifier was reused".to_owned(),
                        ));
                    }
                    self.pending_interactive_callbacks.insert(
                        callback_id,
                        PendingInteractiveCallback {
                            session_id,
                            turn_id,
                            response,
                        },
                    );
                    entries.push(entry);
                }
                Err(error) => {
                    reject_interactive_request(request, &error.to_string());
                }
            }
        }
        Ok(entries)
    }

    pub fn respond_callback(
        &mut self,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.settle_callback(params, None)
    }

    /// Settles a callback with `params` like [`Self::respond_callback`], but
    /// fails the tool that raised it with `message`. Reference
    /// `TurnController.reject_callback`: an answer the client gave that does
    /// not fit the question.
    pub fn reject_callback(
        &mut self,
        params: Value,
        message: &str,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.settle_callback(params, Some(message))
    }

    fn settle_callback(
        &mut self,
        params: Value,
        rejection: Option<&str>,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let callback_id = params
            .get("callbackId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::InvalidResponse("callback response omitted callbackId".to_owned())
            })?
            .to_owned();
        let output = params.get("output").cloned().ok_or_else(|| {
            ClientError::InvalidResponse("callback response omitted output".to_owned())
        })?;
        // Reference `AppServerSession.respond_to_callback` and
        // `reject_callback`: a unified session takes the answer as a
        // `callback/result`.
        if let Some(session_id) = self
            .unified
            .as_ref()
            .and_then(|link| link.callback_session(&callback_id))
        {
            let result = match rejection {
                Some(message) => json!({"callbackId": callback_id, "error": {"message": message}}),
                None => json!({"callbackId": callback_id, "output": output}),
            };
            return self.unified_call(
                &session_id,
                "callback/result",
                json!({"sessionId": session_id, "result": result}),
            );
        }
        let request_id = self.client.take_request_id();
        let request = request_bytes(request_id.clone(), "callback/respond", params)?;
        let batch = self.client.connection.dispatch(&request);
        if batch.close_after_flush {
            return Err(ClientError::InvalidResponse(
                "callback response closed the connection".to_owned(),
            ));
        }
        let result = response_result(response_frame(batch.outbound)?, &request_id)?;
        for work in batch.deferred {
            let driver_result = match work {
                DeferredWork::ResolveCallback {
                    session_id,
                    turn_id,
                    callback_id: deferred_callback_id,
                    accepted,
                    value,
                } if deferred_callback_id == callback_id => self.driver.resolve_callback(
                    &session_id,
                    &turn_id,
                    &deferred_callback_id,
                    accepted,
                    value.as_deref(),
                ),
                DeferredWork::InterruptTurn {
                    session_id,
                    turn_id,
                } => self.driver.interrupt(&session_id, &turn_id),
                _ => {
                    fail_pending_interactive_callback(
                        self.pending_interactive_callbacks.remove(&callback_id),
                        "callback response returned unexpected deferred work",
                    );
                    return Err(ClientError::InvalidResponse(
                        "callback response returned unexpected deferred work".to_owned(),
                    ));
                }
            };
            if let Err(error) = driver_result {
                if matches!(error, DriverError::UnsupportedControl("callback/respond")) {
                    continue;
                }
                fail_pending_interactive_callback(
                    self.pending_interactive_callbacks.remove(&callback_id),
                    &error.to_string(),
                );
                return Err(ClientError::Driver(error));
            }
        }

        if let Some(pending) = self.pending_interactive_callbacks.remove(&callback_id) {
            match pending.response {
                InteractiveCallbackResponse::Approval(response) => {
                    let decision = approval_decision_from_output(&output)?;
                    let _ = response.send(decision);
                }
                InteractiveCallbackResponse::Tool(response) => {
                    let _ = response
                        .send(rejection.map_or(Ok(output), |message| Err(message.to_owned())));
                }
            }
        }
        Ok(result)
    }

    pub async fn compact(
        &mut self,
        session_id: &str,
        extra_instructions: &str,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        if self.is_unified(session_id)
            && let Some(link) = self.unified.as_mut()
        {
            return link
                .dispatch(
                    &self.client.server,
                    session_id,
                    "session/compact",
                    json!({"sessionId": session_id, "extraInstructions": extra_instructions}),
                )?
                .result()
                .await;
        }
        let (request_id, canonical_session_id, working_directory) = self
            .client
            .reserve_compaction(session_id, extra_instructions)?;
        let result = self
            .driver
            .compact(
                &canonical_session_id,
                &working_directory,
                extra_instructions,
            )
            .await;
        self.client
            .finish_compaction(request_id, &canonical_session_id, result)
    }

    /// Reference `_teleport` (`vibe/cli/programmatic.py`): the picker opens
    /// for Teleport, which the server gates, a repository with no linked
    /// project is refused, and the run's events are read as they arrive, every
    /// push question answered with `approve_push`, until the run completes or
    /// fails.
    pub async fn teleport(
        &mut self,
        session_id: &str,
        prompt: &str,
        approve_push: bool,
    ) -> Result<Vec<ProgrammaticTeleportEvent>, ClientError> {
        let opened = self
            .public_call_async(
                "vibeCode/projects/open",
                json!({
                    "sessionId": session_id,
                    "purpose": "teleport",
                    "prompt": (!prompt.is_empty()).then_some(prompt),
                }),
            )
            .await?;
        let picker_id = opened
            .result
            .get("pickerId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::InvalidResponse("project picker omitted pickerId".to_owned())
            })?;
        let project_id = opened
            .result
            .get("resolvedProjectId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::InvalidResponse(
                    "no Vibe Code project is linked to this working directory".to_owned(),
                )
            })?;
        let operation_id = vibe_core::session_id::uuid_v4();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let wanted = operation_id.clone();
        let listener: LiveNotificationListener =
            Arc::new(move |notification: PublicNotification| {
                let Ok(events) = teleport_events(std::slice::from_ref(&notification)) else {
                    return;
                };
                for event in events {
                    if event.operation_id() == wanted {
                        let _ = sender.send(event);
                    }
                }
            });
        self.begin_public_call(
            "vibeCode/teleport/start",
            json!({
                "sessionId": session_id,
                "pickerId": picker_id,
                "projectId": project_id,
                "operationId": operation_id,
                "prompt": (!prompt.is_empty()).then_some(prompt),
            }),
        )?
        .with_listener(listener)
        .complete()
        .await?;
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            let terminal = matches!(
                event,
                ProgrammaticTeleportEvent::Complete { .. }
                    | ProgrammaticTeleportEvent::Failed { .. }
            );
            let push = matches!(event, ProgrammaticTeleportEvent::PushRequired { .. });
            events.push(event);
            if push {
                self.public_call_async(
                    "vibeCode/teleport/push/respond",
                    json!({
                        "sessionId": session_id,
                        "operationId": operation_id,
                        "approved": approve_push,
                    }),
                )
                .await?;
            }
            if terminal {
                break;
            }
        }
        Ok(events)
    }

    pub async fn reserve_prompt(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
    ) -> Result<TurnReservation, ClientError> {
        let images = provider_images(&turn.input).await?;
        self.reserve_prepared_prompt(session_id, turn, images).await
    }

    pub async fn reserve_prepared_prompt(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
        images: PreparedImages,
    ) -> Result<TurnReservation, ClientError> {
        validate_prepared_images(turn, &images)?;
        self.client.configure_pending_mcp(session_id).await?;
        if self.is_unified(session_id) {
            return self.reserve_unified(session_id, turn);
        }
        let mut reservation = self.client.reserve_turn_request(session_id, turn)?;
        reservation.prepared_images = Some(images);
        Ok(reservation)
    }

    /// Reference `AppServerSession.act` up to its first event: `turn/start`
    /// on a unified session, which runs the turn on its own.
    fn reserve_unified(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
    ) -> Result<TurnReservation, ClientError> {
        let result = self.unified_call(
            session_id,
            "turn/start",
            json!({
                "sessionId": session_id,
                "message": turn.input,
                "injected": turn.injected,
                "clientUserMessageId": turn.client_user_message_id,
                "autoTitle": turn.auto_title,
                "userDisplayContent": turn.user_display_content,
                "mentionStats": turn.mention_stats,
            }),
        )?;
        let turn_id = result
            .get("turn")
            .and_then(|started| started.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidResponse("missing turnId".to_owned()))?
            .to_owned();
        if let Some(link) = self.unified.as_ref() {
            link.listen(&turn_id)?;
        }
        Ok(self
            .client
            .server
            .unified_reservation(session_id, &turn_id, turn)?)
    }

    pub async fn reserve_due_loop(
        &mut self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledTurn>, ClientError> {
        // A unified session fires its loops on its own.
        if self.is_unified(session_id) {
            return Ok(None);
        }
        self.client.configure_pending_mcp(session_id).await?;
        let Some(mut scheduled) = self.client.reserve_due_loop(session_id, now_seconds)? else {
            return Ok(None);
        };
        match provider_images(&scheduled.reservation.input).await {
            Ok(images) => scheduled.reservation.prepared_images = Some(images),
            Err(error) => {
                self.client.fail_turn(
                    &scheduled.reservation,
                    &error.to_string(),
                    turn_error_code(&error),
                )?;
                return Err(ClientError::Driver(error));
            }
        }
        Ok(Some(scheduled))
    }

    pub fn finish_scheduled_loop(
        &mut self,
        loop_id: &str,
        completed_at_seconds: u64,
    ) -> Result<(), ClientError> {
        self.client
            .server
            .finish_scheduled_loop(loop_id, completed_at_seconds)
            .map_err(ClientError::Server)
    }

    /// Ends a reserved turn as failed, publishing `code` as the reason a client
    /// branches on. [`turn_error_code`] classifies a driver failure into it.
    pub fn fail_reserved(
        &mut self,
        reservation: &TurnReservation,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<(), ClientError> {
        self.fail_reserved_with(reservation, super::public_turn_failure(code, message))
    }

    /// Ends a reserved turn as failed with `error` as it is published, which
    /// [`public_driver_error`] builds out of a driver failure.
    pub fn fail_reserved_with(
        &mut self,
        reservation: &TurnReservation,
        error: PublicError,
    ) -> Result<(), ClientError> {
        // A unified session settles its own turns; one the client gave up on
        // before it ended is interrupted.
        if self.is_unified(&reservation.session_id) {
            let _ = self.interrupt(&reservation.session_id, &reservation.turn_id);
            return Ok(());
        }
        let message = error.message.clone();
        let result = self.client.fail_turn_with(reservation, error);
        self.fail_interactive_callbacks(
            Some(&reservation.session_id),
            Some(&reservation.turn_id),
            &message,
        );
        result
    }

    pub async fn prompt(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<ProgrammaticTurn, ClientError>
    where
        D: 'static,
    {
        self.client.configure_pending_mcp(session_id).await?;
        let reservation = self.client.reserve_turn(session_id, prompt)?;
        let observer = self.title_scheduler();
        match self.driver.run_observed(&reservation, observer).await {
            Ok(outcome) => self.finish_reserved(&reservation, outcome),
            Err(error) => {
                self.fail_reserved_with(&reservation, public_driver_error(&error))?;
                Err(ClientError::Driver(error))
            }
        }
    }

    pub async fn prompt_observed(
        &mut self,
        session_id: &str,
        prompt: &str,
        observer: Arc<dyn EventObserver>,
    ) -> Result<ProgrammaticTurn, ClientError>
    where
        D: 'static,
    {
        self.client.configure_pending_mcp(session_id).await?;
        let reservation = self.client.reserve_turn(session_id, prompt)?;
        let observer = Arc::new(vibe_core::engine::CompositeEventObserver::new(
            observer,
            self.title_scheduler(),
        ));
        match self.driver.run_observed(&reservation, observer).await {
            Ok(outcome) => self.finish_reserved(&reservation, outcome),
            Err(error) => {
                self.fail_reserved_with(&reservation, public_driver_error(&error))?;
                Err(ClientError::Driver(error))
            }
        }
    }

    pub fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<InterruptOutcome, ClientError> {
        if self.is_unified(session_id) {
            self.unified_call(
                session_id,
                "turn/interrupt",
                json!({"sessionId": session_id, "expectedTurnId": turn_id}),
            )?;
            return Ok(InterruptOutcome::Complete);
        }
        self.driver.interrupt(session_id, turn_id)?;
        if let Err(canonical_error) = self.client.interrupt(session_id, turn_id) {
            return Ok(InterruptOutcome::DriverOnly { canonical_error });
        }
        self.fail_interactive_callbacks(Some(session_id), Some(turn_id), "turn was interrupted");
        Ok(InterruptOutcome::Complete)
    }

    /// Stops the turn that runs `turn_id` where it runs: the driver of a
    /// legacy session, which settles the canonical turn itself, and the
    /// unified session that runs its own.
    ///
    /// # Errors
    ///
    /// The driver or the session refused.
    pub fn interrupt_driver(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError> {
        if self.is_unified(session_id) {
            return self.interrupt(session_id, turn_id).map(drop);
        }
        Ok(self.driver.interrupt(session_id, turn_id)?)
    }

    /// Delivers `input` into the running turn: the canonical turn accepts it
    /// first, so a stale or finished turn refuses it before the driver sees it.
    pub fn steer(
        &mut self,
        session_id: &str,
        turn_id: &str,
        input: &[PublicContentBlock],
        client_user_message_id: Option<&str>,
    ) -> Result<(), ClientError> {
        if self.is_unified(session_id) {
            self.unified_call(
                session_id,
                "turn/steer",
                json!({
                    "sessionId": session_id,
                    "expectedTurnId": turn_id,
                    "message": input,
                    "clientUserMessageId": client_user_message_id,
                }),
            )?;
            return Ok(());
        }
        let (content, inject_invoked_skill) =
            self.client
                .steer(session_id, turn_id, input, client_user_message_id)?;
        self.driver
            .steer(session_id, turn_id, &content, inject_invoked_skill)?;
        Ok(())
    }

    /// Hands `text` to the model as context for the next turn of an idle
    /// session, without a user message of its own.
    pub fn inject_context(&mut self, session_id: &str, text: &str) -> Result<(), ClientError> {
        if self.is_unified(session_id) {
            self.unified_call(
                session_id,
                "session/context/inject",
                json!({"sessionId": session_id, "input": [{"type": "text", "text": text}]}),
            )?;
            return Ok(());
        }
        if let Some((content, as_message, inject_invoked_skill)) =
            self.client.inject_context(session_id, text)?
        {
            self.driver
                .inject_context(session_id, &content, as_message, inject_invoked_skill)?;
        }
        Ok(())
    }

    pub async fn close_session(&mut self, session_id: &str) -> Result<(), ClientError> {
        if self.is_unified(session_id)
            && let Some(link) = self.unified.as_mut()
        {
            link.close(&self.client.server, session_id).await;
            return Ok(());
        }
        let interrupt = self.client.close_session(session_id).await?;
        self.fail_interactive_callbacks(Some(session_id), None, "session was closed");
        if let Some((canonical_session_id, turn_id)) = interrupt {
            if canonical_session_id != session_id {
                self.fail_interactive_callbacks(
                    Some(&canonical_session_id),
                    None,
                    "session was closed",
                );
            }
            self.driver.interrupt(&canonical_session_id, &turn_id)?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        let result = self.client.shutdown();
        if result.is_ok() {
            self.fail_interactive_callbacks(None, None, "service was shut down");
        }
        result
    }

    pub fn finish_reserved(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError> {
        if self.is_unified(&reservation.session_id) {
            return Ok(super::unified::settled_programmatic_turn(
                reservation,
                outcome,
            ));
        }
        let result = self.client.finish_turn(reservation, outcome);
        self.fail_interactive_callbacks(
            Some(&reservation.session_id),
            Some(&reservation.turn_id),
            "turn completed before the callback was resolved",
        );
        result
    }
}

/// Starts the background title a completed model step makes due, beside the
/// rest of the turn. The title lands in the store and the session's state; a
/// process with no runtime to run it on makes the next step due instead.
struct TitleScheduler<D> {
    server: AppServer,
    driver: Arc<D>,
}

impl<D> EventObserver for TitleScheduler<D>
where
    D: TurnDriver + 'static,
{
    fn observe(&self, event: &vibe_core::events::EventEnvelope) -> Result<(), String> {
        let vibe_core::events::EngineEvent::StepCompleted { turn_completing } = event.event else {
            return Ok(());
        };
        let Some(periodic) = self.driver.title_model_is_fast() else {
            return Ok(());
        };
        let Some((job, _)) = self
            .server
            .title_job(&event.session_id, periodic, turn_completing)
        else {
            return Ok(());
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.server.land_title(job, None);
            return Ok(());
        };
        let title = self.driver.generate_title(
            job.session_id.clone(),
            job.messages.clone(),
            job.previous_title.clone(),
            self.server.client_telemetry(),
        );
        let server = self.server.clone();
        runtime.spawn(async move {
            let title = title.await;
            server.land_title(job, title);
        });
        Ok(())
    }
}
