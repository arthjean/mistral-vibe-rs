//! A client that drives an [`AppServer`] in the same process.
//!
//! There is no transport: a call is dispatched straight against a connection and
//! the frames it produces are decoded back into results. The observers that
//! carry engine events into the two update channels live here too, because they
//! are what a caller reads a turn through when nothing is on the wire.

use super::*;

pub(super) struct ProgrammaticEventObserver {
    pub(super) reducer: Mutex<ProjectionReducer>,
    pub(super) emitted: Mutex<BTreeMap<String, Value>>,
    pub(super) sender: tokio::sync::mpsc::Sender<ProgrammaticUpdate>,
    pub(super) next_update_id: AtomicU64,
}

impl EventObserver for ProgrammaticEventObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let mut reducer = lock(&self.reducer);
        if reducer.state().watermark == 0
            && reducer.state().turn_id.is_none()
            && let Some(turn_id) = &event.turn_id
        {
            *reducer = ProjectionReducer::for_turn(&event.session_id, turn_id);
        }
        if reducer.apply(event).map_err(|error| error.to_string())? == ApplyOutcome::Duplicate {
            return Ok(());
        }
        forward_stats(&self.sender, event)?;
        let mut emitted = lock(&self.emitted);
        for entry in &reducer.state().history {
            if !entry.is_completed() {
                continue;
            }
            let encoded = serde_json::to_value(entry).map_err(|error| error.to_string())?;
            let changed = emitted
                .get(&entry.metadata().id)
                .is_none_or(|previous| previous != &encoded);
            if changed {
                emitted.insert(entry.metadata().id.clone(), encoded);
                let event_id = self.next_update_id.fetch_add(1, Ordering::Relaxed);
                self.sender
                    .try_send(ProgrammaticUpdate::HistoryEntry {
                        event_id,
                        emitted_at: event.emitted_at,
                        entry: Box::new(entry.clone()),
                    })
                    .map_err(|error| {
                        format!("programmatic update queue is unavailable: {error}")
                    })?;
            }
        }
        Ok(())
    }
}

/// Usage carries no history, so it is forwarded straight to the live observer
/// instead of traveling through the projection.
pub(super) fn forward_stats(
    sender: &tokio::sync::mpsc::Sender<ProgrammaticUpdate>,
    event: &EventEnvelope,
) -> Result<(), String> {
    let vibe_core::events::EngineEvent::Stats {
        context_tokens,
        input_tokens,
        output_tokens,
        ..
    } = event.event
    else {
        return Ok(());
    };
    sender
        .try_send(ProgrammaticUpdate::Stats {
            context_tokens,
            input_tokens,
            output_tokens,
        })
        .map_err(|error| format!("usage update queue is unavailable: {error}"))
}

pub(super) fn public_history_entry_identity(entry: &PublicHistoryEntry) -> String {
    let metadata = entry.metadata();
    format!(
        "{}:{}",
        metadata.turn_id.as_deref().unwrap_or("session"),
        metadata.id
    )
}

pub(super) struct ServerProjectionObserver {
    pub(super) server: AppServer,
    pub(super) session_id: String,
    pub(super) turn_id: String,
    pub(super) reducer: Mutex<ProjectionReducer>,
    pub(super) emitted: Mutex<BTreeMap<String, Value>>,
    pub(super) sender: tokio::sync::mpsc::Sender<ProgrammaticUpdate>,
}

impl EventObserver for ServerProjectionObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let (snapshot, changed) = {
            let mut reducer = lock(&self.reducer);
            if reducer.apply(event).map_err(|error| error.to_string())? == ApplyOutcome::Duplicate {
                return Ok(());
            }
            let mut emitted = lock(&self.emitted);
            let mut changed = Vec::new();
            for entry in &reducer.state().history {
                let encoded = serde_json::to_value(entry).map_err(|error| error.to_string())?;
                let identity = public_history_entry_identity(entry);
                if emitted
                    .get(&identity)
                    .is_none_or(|previous| previous != &encoded)
                {
                    emitted.insert(identity, encoded);
                    changed.push(entry.clone());
                }
            }
            (reducer.state().clone(), changed)
        };
        // The session's accounting is kept by the server, as the transport's
        // live projection keeps it, before the observer hears of the usage.
        if matches!(
            event.event,
            vibe_core::events::EngineEvent::ToolResult { .. }
                | vibe_core::events::EngineEvent::ToolCallAbandoned { .. }
        ) {
            let mut settled = vibe_core::engine::ToolCallTally::default();
            settled.observe(&event.event);
            self.server
                .record_turn_tool_calls(&self.session_id, &self.turn_id, settled, false)
                .map_err(|error| error.to_string())?;
        }
        if let vibe_core::events::EngineEvent::Stats {
            input_tokens,
            output_tokens,
            cached_tokens,
            ..
        } = event.event
        {
            self.server
                .record_turn_stats(
                    &self.session_id,
                    &self.turn_id,
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                )
                .map_err(|error| error.to_string())?;
            let published = self
                .server
                .check_turn_stats(&self.session_id, &self.turn_id)
                .map_err(|error| error.to_string())?;
            if published.is_some() {
                forward_stats(&self.sender, event)?;
            }
        }

        let mut event_id = self
            .server
            .apply_live_projection(&self.session_id, &self.turn_id, snapshot.clone())
            .map_err(|error| error.to_string())?;
        if changed.is_empty() {
            self.sender
                .try_send(ProgrammaticUpdate::Watermark {
                    event_id,
                    emitted_at: event.emitted_at,
                })
                .map_err(|error| format!("interactive update queue is unavailable: {error}"))?;
            return Ok(());
        }
        for (index, entry) in changed.into_iter().enumerate() {
            if index > 0 {
                event_id = self
                    .server
                    .apply_live_projection(&self.session_id, &self.turn_id, snapshot.clone())
                    .map_err(|error| error.to_string())?;
            }
            self.sender
                .try_send(ProgrammaticUpdate::HistoryEntry {
                    event_id,
                    emitted_at: event.emitted_at,
                    entry: Box::new(entry),
                })
                .map_err(|error| format!("interactive update queue is unavailable: {error}"))?;
        }
        Ok(())
    }
}

/// The problems an MCP configuration reported, out of the frames it published.
///
/// Discovery failures cross the wire as `warning`, so this reads the same
/// vocabulary a reference client reads rather than a shape local to this port.
pub(super) fn decode_mcp_warnings(frames: &[Vec<u8>]) -> Result<Vec<String>, ClientError> {
    let mut diagnostics = Vec::new();
    for frame in frames {
        let notification = match decode_frame(frame)
            .map_err(|error| ClientError::InvalidResponse(error.to_string()))?
        {
            Envelope::Notification(notification) => notification,
            _ => {
                return Err(ClientError::InvalidResponse(
                    "MCP initialization returned an unexpected response".to_owned(),
                ));
            }
        };
        match notification.method.as_str() {
            "warning" => {
                let message = notification
                    .params
                    .get("warning")
                    .and_then(|warning| warning.get("message"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ClientError::InvalidResponse("a warning carries no message".to_owned())
                    })?;
                if !diagnostics.iter().any(|seen| seen == message) {
                    diagnostics.push(message.to_owned());
                }
            }
            "runtime/updated" => {}
            other => {
                return Err(ClientError::InvalidResponse(format!(
                    "MCP initialization published {other}"
                )));
            }
        }
    }
    Ok(diagnostics)
}

pub struct InProcessClient {
    pub(super) server: AppServer,
    pub(super) connection: ServerConnection,
    pub(super) next_request: i64,
    pub(super) pending_mcp: BTreeMap<String, Vec<McpServerConfig>>,
}

/// Hears a notification a call publishes while it is still running.
pub type LiveNotificationListener = Arc<dyn Fn(PublicNotification) + Send + Sync>;

pub struct PendingPublicCall {
    server: AppServer,
    request_id: RequestId,
    method: String,
    outbound: Vec<Vec<u8>>,
    deferred: Vec<DeferredWork>,
    listener: Option<LiveNotificationListener>,
    /// The driver that holds what a legacy session hands it for the next
    /// turn: the result of a manual command it did not save.
    driver: Option<Arc<dyn TurnDriver>>,
    /// Where a unified session gives the answer it owes once its work ends.
    later: Option<tokio::sync::oneshot::Receiver<Vec<u8>>>,
}

impl PendingPublicCall {
    /// A call a unified session's connection took.
    pub(super) fn unified(
        server: &AppServer,
        method: &str,
        dispatched: super::unified::Dispatched,
    ) -> Self {
        Self {
            server: server.clone(),
            request_id: dispatched.request_id,
            method: method.to_owned(),
            outbound: dispatched.outbound,
            deferred: dispatched.deferred,
            listener: None,
            driver: None,
            later: dispatched.later,
        }
    }

    /// The same call, with what it publishes mid-flight also handed to
    /// `listener` as it happens, which is how a sign-in's URL reaches a user
    /// before the sign-in answers.
    #[must_use]
    pub fn with_listener(mut self, listener: LiveNotificationListener) -> Self {
        self.listener = Some(listener);
        self
    }

    pub(super) fn with_driver(mut self, driver: Arc<dyn TurnDriver>) -> Self {
        self.driver = Some(driver);
        self
    }

    pub async fn complete(mut self) -> Result<PublicDispatch, ClientError> {
        // What a call publishes before it answers comes ahead of its answer.
        let early = Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let live: crate::server::FrameSink = {
            let early = early.clone();
            let listener = self.listener.clone();
            Arc::new(move |frame: Vec<u8>| {
                if let Some(listener) = &listener
                    && let Ok(Envelope::Notification(notification)) = decode_frame(&frame)
                {
                    listener(PublicNotification {
                        method: notification.method,
                        params: notification.params,
                    });
                }
                lock(&early).push(frame);
            })
        };
        for work in self.deferred {
            let mut completed = match self.server.execute_deferred(work, live.clone()) {
                Execution::Done => DispatchBatch::default(),
                Execution::Pending(pending) => pending.await,
                Execution::Host(_) => {
                    return Err(ClientError::InvalidResponse(format!(
                        "unsupported deferred work returned by `{}`",
                        self.method
                    )));
                }
            };
            // A manual command of a session that saves nothing hands its
            // result to the driver before the command is answered.
            for work in std::mem::take(&mut completed.deferred) {
                let DeferredWork::InjectContext {
                    session_id,
                    content,
                    as_message,
                    inject_invoked_skill,
                } = work
                else {
                    return Err(ClientError::InvalidResponse(format!(
                        "deferred work returned nested work for `{}`",
                        self.method
                    )));
                };
                let driver = self.driver.as_ref().ok_or_else(|| {
                    ClientError::InvalidResponse(format!(
                        "`{}` handed context to a driver this call does not hold",
                        self.method
                    ))
                })?;
                driver.inject_context(&session_id, &content, as_message, inject_invoked_skill)?;
            }
            if completed.close_after_flush {
                return Err(ClientError::InvalidResponse(format!(
                    "deferred work returned nested work for `{}`",
                    self.method
                )));
            }
            self.outbound.append(&mut lock(&early));
            self.outbound.extend(completed.outbound);
        }
        if let Some(later) = self.later {
            let frame = later.await.map_err(|_| {
                ClientError::InvalidResponse(format!("`{}` was never answered", self.method))
            })?;
            self.outbound.push(frame);
        }
        decode_public_dispatch(self.outbound, &self.request_id, &self.method)
    }
}

impl InProcessClient {
    pub fn connect() -> Result<Self, ClientError> {
        Self::connect_with_server(AppServer::default())
    }

    pub fn connect_with_server(server: AppServer) -> Result<Self, ClientError> {
        Self::connect_with_server_and_client(
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

    pub fn connect_with_server_and_client(
        server: AppServer,
        client_info: ClientInfo,
        capabilities: ClientCapabilities,
    ) -> Result<Self, ClientError> {
        let mut client = Self {
            connection: server.connect(TransportKind::InProcess),
            server,
            next_request: 1,
            pending_mcp: BTreeMap::new(),
        };
        client.initialize(client_info, capabilities)?;
        Ok(client)
    }

    pub fn start_session(&mut self, options: &SessionOptions) -> Result<String, ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "session/start",
            serde_json::to_value(options).map_err(ClientError::Json)?,
        )?;
        let batch = self.connection.dispatch(&request);
        if batch.close_after_flush {
            return Err(ClientError::InvalidResponse(
                "session start unexpectedly closed the connection".to_owned(),
            ));
        }
        let result = response_result(response_frame(batch.outbound)?, &request_id)?;
        let session_id = result
            .get("state")
            .and_then(|state| state.pointer("/session/id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ClientError::InvalidResponse("missing sessionId".to_owned()))?;
        for work in batch.deferred {
            match work {
                DeferredWork::ConfigureMcp {
                    session_id: deferred_session,
                    configs,
                } if deferred_session == session_id => {
                    self.pending_mcp.insert(deferred_session, configs);
                }
                _ => {
                    return Err(ClientError::InvalidResponse(
                        "session start returned unexpected deferred work".to_owned(),
                    ));
                }
            }
        }
        Ok(session_id)
    }

    /// Whether a session this client opened titles itself in the background.
    pub(crate) fn session_titles_itself(&self, session_id: &str) -> bool {
        self.server.session_titles_itself(session_id)
    }

    /// The configuration and session service the server behind this client
    /// composes over.
    #[must_use]
    pub fn workspace_service(&self) -> crate::workspace::WorkspaceService {
        self.server.workspace_service()
    }

    pub async fn configure_pending_mcp(&mut self, session_id: &str) -> Result<(), ClientError> {
        self.configure_pending_mcp_with_diagnostics(session_id)
            .await
            .map(drop)
    }

    pub(super) async fn configure_pending_mcp_with_diagnostics(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<String>, ClientError> {
        // The session's connector catalog is resolved with its integrations,
        // as the reference resolves it while it builds the session. A catalog
        // that cannot be had leaves the session without one.
        let _ = self.server.open_session_connectors(session_id).await;
        let Some(configs) = self.pending_mcp.get(session_id).cloned() else {
            return Ok(Vec::new());
        };
        let frames = self.server.configure_mcp_servers(session_id, configs).await;
        let diagnostics = decode_mcp_warnings(&frames)?;
        self.pending_mcp.remove(session_id);
        Ok(diagnostics)
    }

    pub fn session(&mut self, session_id: &str) -> Result<SessionView, ClientError> {
        self.call("session/read", json!({"sessionId": session_id}))?;
        self.server.session(session_id).map_err(ClientError::Server)
    }

    pub fn reserve_turn_request(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
    ) -> Result<TurnReservation, ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
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
        let batch = self.connection.dispatch(&request);
        let result = response_result(response_frame(batch.outbound)?, &request_id)?;
        let turn_id = result
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidResponse("missing turnId".to_owned()))?
            .to_owned();
        // The answer has to have deferred exactly the run it just reported, so
        // the reservation is built from the deferred work rather than from the
        // request: the two would otherwise be able to disagree.
        let mut deferred = batch.deferred.into_iter();
        let reserved = deferred.next().filter(|work| {
            matches!(
                work,
                DeferredWork::RunTurn {
                    session_id: reserved_session,
                    turn_id: reserved_turn,
                    prompt: reserved_prompt,
                    ..
                } if reserved_session == session_id
                    && *reserved_turn == turn_id
                    && *reserved_prompt == turn.prompt
            )
        });
        let (Some(work), None) = (reserved, deferred.next()) else {
            return Err(ClientError::InvalidResponse(
                "turn reservation omitted deferred work".to_owned(),
            ));
        };
        Ok(self.server.reserve_turn(work)?)
    }

    pub(super) fn reserve_due_loop(
        &mut self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledTurn>, ClientError> {
        let Some(scheduled) = self.server.reserve_due_loop(session_id, now_seconds)? else {
            return Ok(None);
        };
        Ok(Some(ScheduledTurn {
            loop_id: scheduled.fire.loop_id,
            reservation: self.server.reserve_turn(scheduled.work)?,
            notice: PublicNotification {
                method: scheduled.fire.notice.method,
                params: scheduled.fire.notice.params,
            },
        }))
    }

    pub fn finish_turn(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError> {
        let public_stop_reason = outcome
            .stop_reason
            .is_limit()
            .then_some(vibe_core::events::PublicTurnStopReason::Limit);
        self.server.complete_turn_with_details(
            &reservation.session_id,
            &reservation.turn_id,
            outcome.snapshot.clone(),
            public_stop_reason,
            public_turn_error(&outcome.stop_reason),
        )?;
        ProgrammaticTurn::settled(&reservation.turn_id, outcome)
    }

    pub fn fail_turn(
        &mut self,
        reservation: &TurnReservation,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<(), ClientError> {
        self.fail_turn_with(reservation, super::public_turn_failure(code, message))
    }

    /// Ends a reserved turn as failed with `error` as it is published.
    pub fn fail_turn_with(
        &mut self,
        reservation: &TurnReservation,
        error: PublicError,
    ) -> Result<(), ClientError> {
        self.server
            .fail_turn_with(&reservation.session_id, &reservation.turn_id, error)?;
        Ok(())
    }

    /// Sends `session/context/inject` for an idle session and returns the text
    /// the driver is to hold for the next turn, once the canonical session has
    /// recorded it. A session saved with the context already holds it, and the
    /// driver then has nothing to hold.
    pub fn inject_context(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<Option<(String, bool, bool)>, ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "session/context/inject",
            json!({
                "sessionId": session_id,
                "input": [{"type": "text", "text": text}],
            }),
        )?;
        let batch = self.connection.dispatch(&request);
        response_result(response_frame(batch.outbound)?, &request_id)?;
        match batch.deferred.as_slice() {
            [
                DeferredWork::InjectContext {
                    content,
                    as_message,
                    inject_invoked_skill,
                    ..
                },
            ] => Ok(Some((content.clone(), *as_message, *inject_invoked_skill))),
            [] => Ok(None),
            _ => Err(ClientError::InvalidResponse(
                "context injection did not schedule delivery".to_owned(),
            )),
        }
    }

    pub fn interrupt(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "turn/interrupt",
            json!({"sessionId": session_id, "expectedTurnId": turn_id}),
        )?;
        let batch = self.connection.dispatch(&request);
        response_result(response_frame(batch.outbound)?, &request_id)?;
        if batch.deferred
            != vec![DeferredWork::InterruptTurn {
                session_id: session_id.to_owned(),
                turn_id: turn_id.to_owned(),
            }]
        {
            return Err(ClientError::InvalidResponse(
                "interrupt did not schedule cancellation".to_owned(),
            ));
        }
        Ok(())
    }

    /// Sends `turn/steer` for the running turn and returns the text the
    /// driver is to deliver, once the canonical turn has accepted it.
    pub fn steer(
        &mut self,
        session_id: &str,
        turn_id: &str,
        input: &[PublicContentBlock],
        client_user_message_id: Option<&str>,
    ) -> Result<(String, bool), ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "turn/steer",
            json!({
                "sessionId": session_id,
                "expectedTurnId": turn_id,
                "message": input,
                "clientUserMessageId": client_user_message_id,
            }),
        )?;
        let batch = self.connection.dispatch(&request);
        response_result(response_frame(batch.outbound)?, &request_id)?;
        match batch.deferred.as_slice() {
            [
                DeferredWork::SteerTurn {
                    content,
                    inject_invoked_skill,
                    ..
                },
            ] => Ok((content.clone(), *inject_invoked_skill)),
            _ => Err(ClientError::InvalidResponse(
                "steer did not schedule delivery".to_owned(),
            )),
        }
    }

    pub async fn close_session(
        &mut self,
        session_id: &str,
    ) -> Result<Option<(String, String)>, ClientError> {
        self.pending_mcp.remove(session_id);
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "session/close",
            json!({"sessionId": session_id}),
        )?;
        let batch = self.connection.dispatch(&request);
        if !batch.close_after_flush {
            return Err(ClientError::InvalidResponse(
                "session close did not terminate the connection".to_owned(),
            ));
        }
        response_result(response_frame(batch.outbound)?, &request_id)?;
        let mut interrupt = None;
        for work in batch.deferred {
            match work {
                DeferredWork::InterruptTurn {
                    session_id,
                    turn_id,
                } if interrupt.is_none() => {
                    interrupt = Some((session_id, turn_id));
                }
                DeferredWork::CloseResources {
                    session_id,
                    generation,
                } => {
                    self.server
                        .close_resource_session(&session_id, generation)
                        .await?;
                }
                _ => {
                    return Err(ClientError::InvalidResponse(
                        "session close returned unexpected deferred work".to_owned(),
                    ));
                }
            }
        }
        Ok(interrupt)
    }

    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        if self.connection.state() == crate::server::ConnectionState::Closed {
            return Ok(());
        }
        self.call("shutdown", json!({}))?;
        let notification = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": {}
        }))
        .map_err(ClientError::Json)?;
        let batch = self.connection.dispatch(&notification);
        if !batch.close_after_flush {
            return Err(ClientError::InvalidResponse(
                "shutdown did not close connection".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.call(method, params)
    }

    pub async fn public_call_async(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PublicDispatch, ClientError> {
        self.begin_public_call(method, params)?.complete().await
    }

    pub fn begin_public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PendingPublicCall, ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(request_id.clone(), method, params)?;
        let batch = self.connection.dispatch(&request);
        if batch.close_after_flush {
            return Err(ClientError::InvalidResponse(format!(
                "`{method}` unexpectedly closed the connection"
            )));
        }
        Ok(PendingPublicCall {
            server: self.server.clone(),
            request_id,
            method: method.to_owned(),
            outbound: batch.outbound,
            deferred: batch.deferred,
            listener: None,
            driver: None,
            later: None,
        })
    }

    pub(super) fn reserve_compaction(
        &mut self,
        session_id: &str,
        extra_instructions: &str,
    ) -> Result<(RequestId, String, String), ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "session/compact/start",
            json!({
                "sessionId": session_id,
                "extraInstructions": extra_instructions,
            }),
        )?;
        let batch = self.connection.dispatch(&request);
        if batch.close_after_flush || !batch.outbound.is_empty() {
            return Err(ClientError::InvalidResponse(
                "compaction was not deferred".to_owned(),
            ));
        }
        match batch.deferred.as_slice() {
            [
                DeferredWork::CompactSession {
                    request_id: deferred_id,
                    session_id,
                    working_directory,
                    ..
                },
            ] if deferred_id == &request_id => {
                Ok((request_id, session_id.clone(), working_directory.clone()))
            }
            _ => Err(ClientError::InvalidResponse(
                "compaction omitted deferred work".to_owned(),
            )),
        }
    }

    pub(super) fn finish_compaction(
        &mut self,
        request_id: RequestId,
        session_id: &str,
        result: Result<SessionCompaction, DriverError>,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let batch = match result {
            Ok(compaction) => self.server.complete_manual_compaction(
                request_id.clone(),
                session_id,
                &compaction.summary,
                compaction.hydrated,
            ),
            Err(error) => self.server.fail_manual_compaction(
                request_id.clone(),
                session_id,
                &error.to_string(),
            ),
        };
        let response = batch
            .outbound
            .into_iter()
            .find(|bytes| {
                decode_frame(bytes).is_ok_and(|envelope| {
                    matches!(
                        envelope,
                        Envelope::Success(SuccessResponse { ref id, .. })
                            | Envelope::Error(ErrorResponse { ref id, .. })
                            if id == &request_id
                    )
                })
            })
            .ok_or_else(|| {
                ClientError::InvalidResponse("compaction response is missing".to_owned())
            })?;
        response_result(response, &request_id)
    }

    fn initialize(
        &mut self,
        client_info: ClientInfo,
        capabilities: ClientCapabilities,
    ) -> Result<(), ClientError> {
        self.call(
            "initialize",
            json!({
                "clientInfo": client_info,
                "capabilities": capabilities,
            }),
        )?;
        let initialized = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .map_err(ClientError::Json)?;
        let batch = self.connection.dispatch(&initialized);
        if !batch.outbound.is_empty() || batch.close_after_flush {
            return Err(ClientError::InvalidResponse(
                "initialized notification was rejected".to_owned(),
            ));
        }
        Ok(())
    }

    fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(request_id.clone(), method, params)?;
        let batch = self.connection.dispatch(&request);
        if batch.close_after_flush || !batch.deferred.is_empty() {
            return Err(ClientError::InvalidResponse(format!(
                "unexpected dispatch behavior for `{method}`"
            )));
        }
        response_result(response_frame(batch.outbound)?, &request_id)
    }

    pub(super) fn take_request_id(&mut self) -> RequestId {
        let id = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        RequestId::Integer(id)
    }
}
