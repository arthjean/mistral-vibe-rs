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
    pub(super) completed_only: bool,
    pub(super) next_update_id: AtomicU64,
}

impl EventObserver for ProgrammaticEventObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let mut reducer = self
            .reducer
            .lock()
            .map_err(|_| "programmatic projection lock is poisoned".to_owned())?;
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
        let mut emitted = self
            .emitted
            .lock()
            .map_err(|_| "programmatic emission lock is poisoned".to_owned())?;
        for entry in &reducer.state().history {
            if self.completed_only && !entry.is_completed() {
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
            let mut reducer = self
                .reducer
                .lock()
                .map_err(|_| "interactive projection lock is poisoned".to_owned())?;
            if reducer.apply(event).map_err(|error| error.to_string())? == ApplyOutcome::Duplicate {
                return Ok(());
            }
            let mut emitted = self
                .emitted
                .lock()
                .map_err(|_| "interactive emission lock is poisoned".to_owned())?;
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
        forward_stats(&self.sender, event)?;

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

pub struct PendingPublicCall {
    server: AppServer,
    request_id: RequestId,
    method: String,
    outbound: Vec<Vec<u8>>,
    deferred: Vec<DeferredWork>,
}

impl PendingPublicCall {
    pub async fn complete(mut self) -> Result<PublicDispatch, ClientError> {
        for work in self.deferred {
            let completed = match work {
                DeferredWork::ResourceRequest {
                    request_id,
                    session_id,
                    command,
                } => {
                    self.server
                        .execute_resource_request(request_id, session_id, command)
                        .await
                }
                DeferredWork::CloudRequest {
                    request_id,
                    method,
                    params,
                } => {
                    self.server
                        .execute_cloud_request(request_id, method, params)
                        .await
                }
                _ => {
                    return Err(ClientError::InvalidResponse(format!(
                        "unsupported deferred work returned by `{}`",
                        self.method
                    )));
                }
            };
            if completed.close_after_flush || !completed.deferred.is_empty() {
                return Err(ClientError::InvalidResponse(format!(
                    "deferred work returned nested work for `{}`",
                    self.method
                )));
            }
            self.outbound.extend(completed.outbound);
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

    /// The configuration and session service the server behind this client
    /// composes over.
    #[must_use]
    pub fn release3_service(&self) -> crate::release3::Release3Service {
        self.server.release3_service()
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

    pub fn reserve_turn(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<TurnReservation, ClientError> {
        self.reserve_turn_request(session_id, &TurnRequest::text(prompt))
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
                "input": turn.input,
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
        let final_assistant = outcome
            .snapshot
            .history
            .iter()
            .rev()
            .find_map(|entry| match entry {
                PublicHistoryEntry::Message {
                    role: vibe_core::events::PublicMessageRole::Assistant,
                    content,
                    ..
                } => {
                    let text = content
                        .iter()
                        .filter_map(|block| match block {
                            PublicContentBlock::Text { text } => Some(text.as_str()),
                            PublicContentBlock::Image { .. }
                            | PublicContentBlock::Resource { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
            .unwrap_or_default();
        let public_stop_reason = matches!(
            outcome.stop_reason,
            PublicTurnStopReason::MaxSteps
                | PublicTurnStopReason::TokenLimit
                | PublicTurnStopReason::PriceLimit
        )
        .then_some(vibe_core::events::PublicTurnStopReason::Limit);
        self.server.complete_turn_with_details(
            &reservation.session_id,
            &reservation.turn_id,
            outcome.snapshot.clone(),
            public_stop_reason,
            public_turn_error(&outcome.stop_reason),
        )?;
        let events = outcome
            .events
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ClientError::Json)?;
        Ok(ProgrammaticTurn {
            session_id: outcome.session_id,
            turn_id: reservation.turn_id.clone(),
            final_assistant,
            history: outcome.snapshot.history,
            events,
            usage: PublicUsage {
                input_tokens: outcome.usage.input_tokens,
                output_tokens: outcome.usage.output_tokens,
                price_micros: outcome.price_micros,
            },
            context_tokens: outcome.context_tokens,
            steps: outcome.steps,
            checkpoints: outcome.checkpoints,
            stop_reason: outcome.stop_reason,
            teleport_events: Vec::new(),
        })
    }

    pub fn fail_turn(
        &mut self,
        reservation: &TurnReservation,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<(), ClientError> {
        self.server
            .fail_turn(&reservation.session_id, &reservation.turn_id, message, code)?;
        Ok(())
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

    pub fn public_call_with_notifications(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PublicDispatch, ClientError> {
        let request_id = self.take_request_id();
        let request = request_bytes(request_id.clone(), method, params)?;
        let batch = self.connection.dispatch(&request);
        if batch.close_after_flush || !batch.deferred.is_empty() {
            return Err(ClientError::InvalidResponse(format!(
                "unexpected dispatch behavior for `{method}`"
            )));
        }
        decode_public_dispatch(batch.outbound, &request_id, method)
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
        })
    }

    pub(super) fn reserve_compaction(
        &mut self,
        session_id: &str,
        extra_instructions: &str,
    ) -> Result<(RequestId, String), ClientError> {
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
                    ..
                },
            ] if deferred_id == &request_id => Ok((request_id, session_id.clone())),
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
                &compaction.new_session_id,
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
