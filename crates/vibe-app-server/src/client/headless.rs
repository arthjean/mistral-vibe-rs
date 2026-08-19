//! The programmatic surface: one process driving sessions without a client.
//!
//! [`HeadlessService`] owns an in-process client and a driver, and turns the
//! calls a program makes into the same dispatch a connected editor would
//! produce. Everything a person would answer interactively is answered here
//! through the interactive tools instead, or refused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
        let server = server.using_surface_extension(
            Arc::new(InteractiveApprovalFactory {
                sender: sender.clone(),
            }),
            Arc::new(InteractiveSessionToolFactory {
                sender,
                plan_directory,
            }),
        );
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
        })
    }

    pub fn start_session(&mut self, options: &SessionOptions) -> Result<String, ClientError> {
        self.client.start_session(options)
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

    #[must_use]
    pub fn driver(&self) -> Arc<D> {
        self.driver.clone()
    }

    pub fn session(&mut self, session_id: &str) -> Result<SessionView, ClientError> {
        self.client.session(session_id)
    }

    pub fn public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.client.public_call(method, params)
    }

    pub fn public_call_with_notifications(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PublicDispatch, ClientError> {
        self.client.public_call_with_notifications(method, params)
    }

    pub async fn public_call_async(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PublicDispatch, ClientError> {
        self.client.public_call_async(method, params).await
    }

    pub fn begin_public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PendingPublicCall, ClientError> {
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
        let seed = self
            .client
            .server
            .live_projection_seed(session_id, turn_id)?;
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

    /// Hands a tool's clearing request to the turn it names, and answers the
    /// tool with what the driver said.
    fn dispatch_context_clearing(&self, request: InteractiveCallbackRequest) {
        let InteractiveCallbackRequest::ClearContext {
            session_id,
            continuation,
            plan_file_path,
            response,
        } = request
        else {
            return;
        };
        let outcome = self
            .client
            .server
            .session(&session_id)
            .map_err(|error| error.to_string())
            .and_then(|session| {
                session
                    .active_turn
                    .ok_or_else(|| "turn is no longer active".to_owned())
            })
            .and_then(|turn_id| {
                self.driver
                    .clear_context(
                        &session_id,
                        &turn_id,
                        &continuation,
                        plan_file_path.as_deref(),
                    )
                    .map_err(|error| error.to_string())
            });
        let _ = response.send(outcome);
    }

    pub fn drain_callbacks(&mut self) -> Result<Vec<PublicHistoryEntry>, ClientError> {
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
            if matches!(request, InteractiveCallbackRequest::ClearContext { .. }) {
                self.dispatch_context_clearing(request);
                continue;
            }
            let (session_id, title, detail, kind) = match &request {
                InteractiveCallbackRequest::Approval {
                    session_id,
                    request,
                    ..
                } => (
                    session_id.clone(),
                    format!("Approve {}?", request.tool),
                    approval_callback_detail(request),
                    EngineCallbackKind::Approval,
                ),
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
                InteractiveCallbackRequest::ClearContext { .. } => continue,
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
                        InteractiveCallbackRequest::ClearContext { .. } => continue,
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
                    let _ = response.send(Ok(output));
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
        let (request_id, canonical_session_id) = self
            .client
            .reserve_compaction(session_id, extra_instructions)?;
        let result = self
            .driver
            .compact(&canonical_session_id, extra_instructions)
            .await;
        self.client
            .finish_compaction(request_id, &canonical_session_id, result)
    }

    pub async fn teleport(
        &mut self,
        session_id: &str,
        working_directory: &str,
        summary: &str,
        approve_push: bool,
    ) -> Result<Vec<ProgrammaticTeleportEvent>, ClientError> {
        let opened = self
            .public_call_async(
                "vibeCode/projects/open",
                json!({
                    "sessionId": session_id,
                    "workingDirectory": working_directory,
                    "purpose": "teleport",
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
        let operation_id = unique_cloud_operation_id();
        let started = self
            .public_call_async(
                "vibeCode/teleport/start",
                json!({
                    "sessionId": session_id,
                    "pickerId": picker_id,
                    "projectId": project_id,
                    "operationId": operation_id,
                    "workingDirectory": working_directory,
                    "prompt": summary,
                }),
            )
            .await?;
        let mut events = teleport_events(&started.notifications)?;
        if events
            .iter()
            .any(|event| matches!(event, ProgrammaticTeleportEvent::PushRequired { .. }))
        {
            let responded = self
                .public_call_async(
                    "vibeCode/teleport/push/respond",
                    json!({
                        "sessionId": session_id,
                        "operationId": operation_id,
                        "approved": approve_push,
                    }),
                )
                .await?;
            events.extend(teleport_events(&responded.notifications)?);
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
        let mut reservation = self.client.reserve_turn_request(session_id, turn)?;
        reservation.prepared_images = Some(images);
        Ok(reservation)
    }

    pub async fn reserve_due_loop(
        &mut self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledTurn>, ClientError> {
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

    pub fn finish_reserved(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError> {
        let result = self.client.finish_turn(reservation, outcome);
        self.fail_interactive_callbacks(
            Some(&reservation.session_id),
            Some(&reservation.turn_id),
            "turn completed before the callback was resolved",
        );
        result
    }

    /// Ends a reserved turn as failed, publishing `code` as the reason a client
    /// branches on. [`turn_error_code`] classifies a driver failure into it.
    pub fn fail_reserved(
        &mut self,
        reservation: &TurnReservation,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<(), ClientError> {
        let result = self.client.fail_turn(reservation, message, code);
        self.fail_interactive_callbacks(
            Some(&reservation.session_id),
            Some(&reservation.turn_id),
            message,
        );
        result
    }

    pub async fn prompt(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<ProgrammaticTurn, ClientError> {
        self.client.configure_pending_mcp(session_id).await?;
        let reservation = self.client.reserve_turn(session_id, prompt)?;
        match self.driver.run(&reservation).await {
            Ok(outcome) => self.finish_reserved(&reservation, outcome),
            Err(error) => {
                self.fail_reserved(&reservation, &error.to_string(), turn_error_code(&error))?;
                Err(ClientError::Driver(error))
            }
        }
    }

    pub async fn prompt_observed(
        &mut self,
        session_id: &str,
        prompt: &str,
        observer: Arc<dyn EventObserver>,
    ) -> Result<ProgrammaticTurn, ClientError> {
        self.client.configure_pending_mcp(session_id).await?;
        let reservation = self.client.reserve_turn(session_id, prompt)?;
        match self.driver.run_observed(&reservation, observer).await {
            Ok(outcome) => self.finish_reserved(&reservation, outcome),
            Err(error) => {
                self.fail_reserved(&reservation, &error.to_string(), turn_error_code(&error))?;
                Err(ClientError::Driver(error))
            }
        }
    }

    pub fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<InterruptOutcome, ClientError> {
        self.driver.interrupt(session_id, turn_id)?;
        if let Err(canonical_error) = self.client.interrupt(session_id, turn_id) {
            return Ok(InterruptOutcome::DriverOnly { canonical_error });
        }
        self.fail_interactive_callbacks(Some(session_id), Some(turn_id), "turn was interrupted");
        Ok(InterruptOutcome::Complete)
    }

    pub async fn close_session(&mut self, session_id: &str) -> Result<(), ClientError> {
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
}

fn unique_cloud_operation_id() -> String {
    let timestamp = crate::host::now_millis();
    let sequence = NEXT_CLOUD_OPERATION.fetch_add(1, Ordering::Relaxed);
    format!(
        "teleport-programmatic-{}-{timestamp}-{sequence}",
        std::process::id()
    )
}
