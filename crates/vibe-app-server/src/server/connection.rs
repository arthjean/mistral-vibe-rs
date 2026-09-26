//! One client connection, and the requests it dispatches.
//!
//! [`AppServer`] holds the sessions; a connection holds what is true of one
//! client: which sessions it attached, what capabilities it declared, and which
//! server-to-client requests are still outstanding. Every routed method is
//! answered here, against the server above.

mod review_request;
mod rewind;
mod saved;
mod session;
mod trust;
mod turn;

use super::*;

pub struct ServerConnection {
    pub(super) server: AppServer,
    pub(super) state: ConnectionState,
    pub(super) transport: TransportKind,
    pub(super) capabilities: ClientCapabilities,
    pub(super) attached_sessions: BTreeSet<String>,
    /// The registry key of the session this connection opened first, which
    /// answers the methods the reference routes to a connection's root.
    pub(super) root: Option<String>,
    /// How the client said it was launched, which decides whether its
    /// sessions name themselves in the background.
    pub(super) entrypoint: ClientEntrypoint,
    pub(super) pending_server_requests: HashMap<RequestId, CallbackRoute>,
}

impl ServerConnection {
    #[must_use]
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    pub fn dispatch(&mut self, bytes: &[u8]) -> DispatchBatch {
        let frame = match decode_frame(bytes) {
            Ok(frame) => frame,
            Err(_) => {
                self.state = ConnectionState::Closed;
                return DispatchBatch {
                    outbound: Vec::new(),
                    deferred: Vec::new(),
                    close_after_flush: true,
                };
            }
        };
        let mut batch = match frame {
            Envelope::Request(request) => self.handle_request(request),
            Envelope::Notification(notification) => self.handle_notification(notification),
            Envelope::Success(response) => self.handle_server_success(response),
            Envelope::Error(response) => self.handle_server_error(response),
        };
        batch.outbound.retain(|frame| self.delivers(frame));
        batch
    }

    /// Whether a frame may be delivered to this client.
    ///
    /// A client can mute notification names during `initialize`, and the server
    /// honors the list with one exception: a sequenced event carries the
    /// per-session `eventId` the client's projection counts on, so dropping one
    /// would open a gap it reads as a fault. Muting a non-event notification
    /// touches no watermark, so the sequence stays contiguous either way.
    ///
    /// Every other frame passes: responses and server requests are answers the
    /// client asked for, not a stream it can silence.
    #[must_use]
    pub fn delivers(&self, frame: &[u8]) -> bool {
        if self.capabilities.disabled_notifications.is_empty() {
            return true;
        }
        let Ok(Envelope::Notification(notification)) = decode_frame(frame) else {
            return true;
        };
        if notification.params.contains_key("eventId") {
            return true;
        }
        !self
            .capabilities
            .disabled_notifications
            .contains(&notification.method)
    }

    pub fn request_callback(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        prompt: impl Into<String>,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        let prompt = prompt.into();
        self.deliver_callback(session_id, turn_id, kind, |server| {
            server.request_callback(session_id, turn_id, kind, prompt)
        })
    }

    pub fn request_callback_with_detail(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        title: impl Into<String>,
        detail: Value,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        let title = title.into();
        self.deliver_callback(session_id, turn_id, kind, |server| {
            server.request_callback_with_detail(session_id, turn_id, kind, title, detail)
        })
    }

    /// The frames a completed attachment puts on the wire.
    ///
    /// A newly attached client is handed the whole session as a sequenced
    /// `session/snapshot`, then any callback still open is replayed to it: the
    /// question was delivered to whoever was attached before, so without the
    /// replay the new client would wait on a turn it cannot unblock.
    pub(crate) fn attachment_frames(&mut self, session_id: &str) -> Vec<Vec<u8>> {
        let Ok(mut sessions) = self.server.lock_sessions() else {
            return Vec::new();
        };
        let Some(session) = sessions.get_mut(session_id) else {
            return Vec::new();
        };
        let mut frames = vec![session_snapshot_frame(session)];
        let open = session
            .pending_callback
            .clone()
            .zip(session.active_turn.clone())
            .map(|(callback, turn_id)| (session.id.clone(), callback, turn_id));
        drop(sessions);
        let Some((canonical_session_id, callback, turn_id)) = open else {
            return frames;
        };
        let supported = match callback.kind {
            EngineCallbackKind::Approval => CallbackKind::Approval,
            EngineCallbackKind::UserInput => CallbackKind::UserInput,
            // A kind this connection cannot answer is not replayed to it: the
            // reference refuses to raise one rather than emit a callback the
            // client has no way to close.
            EngineCallbackKind::ConnectorAuth => return frames,
        };
        if !self.capabilities.callback_kinds.contains(&supported) {
            return frames;
        }
        let sequence = self.server.next_callback.fetch_add(1, Ordering::Relaxed);
        let request_id = RequestId::Integer(i64::try_from(sequence).unwrap_or(i64::MAX));
        self.pending_server_requests.insert(
            request_id.clone(),
            CallbackRoute {
                session_id: canonical_session_id,
                turn_id,
                callback_id: callback.id.clone(),
                answered: false,
            },
        );
        frames.push(encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id,
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback.entry))]),
        })));
        frames
    }

    /// Mints a callback through `request` and routes the client's answer back
    /// to the turn that is waiting for it.
    fn deliver_callback(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        request: impl FnOnce(&AppServer) -> Result<(String, Vec<Vec<u8>>), ServerError>,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        if self.state != ConnectionState::Ready {
            return Err(ServerError::NotInitialized);
        }
        let supported = match kind {
            EngineCallbackKind::Approval => CallbackKind::Approval,
            EngineCallbackKind::UserInput => CallbackKind::UserInput,
            EngineCallbackKind::ConnectorAuth => {
                return Err(ServerError::UnsupportedClientCallbackKind(kind));
            }
        };
        if !self.capabilities.callback_kinds.contains(&supported) {
            return Err(ServerError::UnsupportedClientCallbackKind(kind));
        }
        let (callback_id, frames) = request(&self.server)?;
        // The delivery itself is the last frame: the status update precedes it.
        let delivery = frames.last().ok_or(ServerError::InvalidCallbackRequest)?;
        let request_id = match decode_frame(delivery)? {
            Envelope::Request(request) => request.id,
            _ => return Err(ServerError::InvalidCallbackRequest),
        };
        self.pending_server_requests.insert(
            request_id,
            CallbackRoute {
                session_id: session_id.to_owned(),
                turn_id: turn_id.to_owned(),
                callback_id: callback_id.clone(),
                answered: false,
            },
        );
        Ok((callback_id, frames))
    }

    fn handle_server_success(&mut self, response: SuccessResponse) -> DispatchBatch {
        if self.server.client_tools.resolve(
            &response.id,
            Ok(Value::Object(response.result.clone().into_iter().collect())),
        ) {
            return DispatchBatch::empty();
        }
        let Some(route) = self.pending_server_requests.remove(&response.id) else {
            return self.close_for_protocol_error();
        };
        let callback_id_matches = response.result.get("callbackId").and_then(Value::as_str)
            == Some(route.callback_id.as_str());
        let accepted = response
            .result
            .get("accepted")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !callback_id_matches {
            return self.close_for_protocol_error();
        }
        if accepted || route.answered {
            DispatchBatch::empty()
        } else {
            match self
                .server
                .reject_callback(&route, "Client did not accept callback delivery")
            {
                Ok((status, deferred)) => DispatchBatch {
                    outbound: rejection_frames(status),
                    deferred,
                    close_after_flush: false,
                },
                Err(_) => self.close_for_protocol_error(),
            }
        }
    }

    fn handle_server_error(&mut self, response: ErrorResponse) -> DispatchBatch {
        if self
            .server
            .client_tools
            .resolve(&response.id, Err(response.error.message.clone()))
        {
            return DispatchBatch::empty();
        }
        let Some(route) = self.pending_server_requests.remove(&response.id) else {
            return self.close_for_protocol_error();
        };
        if route.answered {
            return DispatchBatch::empty();
        }
        match self.server.reject_callback(&route, &response.error.message) {
            Ok((status, deferred)) => DispatchBatch {
                outbound: rejection_frames(status),
                deferred,
                close_after_flush: false,
            },
            Err(_) => self.close_for_protocol_error(),
        }
    }

    fn close_for_protocol_error(&mut self) -> DispatchBatch {
        self.state = ConnectionState::Closed;
        DispatchBatch {
            outbound: Vec::new(),
            deferred: Vec::new(),
            close_after_flush: true,
        }
    }

    pub fn attach_session(&mut self, session_id: &str) -> Result<(), ServerError> {
        if self.state != ConnectionState::Ready {
            return Err(ServerError::NotInitialized);
        }
        let mut sessions = self.server.lock_sessions()?;
        let key = sessions
            .key(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
            .to_owned();
        if self.attached_sessions.contains(&key) {
            return Ok(());
        }
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.attachments == 0
            && let Some(backend) = &self.server.resource_backend
        {
            let generation = session.resource_generation.checked_add(1).ok_or_else(|| {
                ServerError::Resource("resource session generation was exhausted".to_owned())
            })?;
            backend
                .open_session(ResourceSession {
                    session_id: session.id.clone(),
                    generation,
                    working_directory: session.working_directory.clone(),
                    project_trusted: session.intent.trusted,
                    policy: session.policy.clone(),
                    tools: session.tools.clone(),
                })
                .map_err(|error| ServerError::Resource(error.to_string()))?;
            session.resource_generation = generation;
        }
        session.attachments = session.attachments.saturating_add(1);
        self.attached_sessions.insert(key);
        Ok(())
    }

    pub fn detach_session(&mut self, session_id: &str) -> Result<(), ServerError> {
        let mut sessions = self.server.lock_sessions()?;
        let key = sessions
            .key(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
            .to_owned();
        if self.attached_sessions.remove(&key)
            && let Some(session) = sessions.get_mut(&key)
        {
            session.attachments = session.attachments.saturating_sub(1);
        }
        Ok(())
    }

    pub fn close(&mut self) {
        let mut released = Vec::new();
        if let Ok(mut sessions) = self.server.lock_sessions() {
            for session_id in &self.attached_sessions {
                if let Some(session) = sessions.get_mut(session_id) {
                    session.attachments = session.attachments.saturating_sub(1);
                    if session.attachments == 0 && session.status != SessionStatus::Closed {
                        released.push(session.id.clone());
                    }
                }
            }
        }
        // A connection that goes away lets go of what it held, as the
        // reference's backend shutdown does: the lease, and the terminal's
        // pointer to the session it leaves.
        for session_id in released {
            self.server.release_lease(&session_id);
            let _ = self
                .server
                .workspace
                .close_saved_session(&session_id, now_millis());
        }
        self.root = None;
        self.attached_sessions.clear();
        self.pending_server_requests.clear();
        // A tool parked on a delegation would otherwise wait out its deadline
        // for a client that is already gone.
        self.server.client_tools.detach();
        self.state = ConnectionState::Closed;
    }

    /// The delegation registry a transport puts its write side on, so a tool
    /// running off the read loop can still reach the client.
    #[must_use]
    pub fn client_tools(&self) -> Arc<ClientToolBridge> {
        self.server.client_tools.clone()
    }

    pub(crate) fn attached_session_ids(&self) -> Vec<String> {
        self.attached_sessions.iter().cloned().collect()
    }

    fn handle_request(&mut self, request: ServerRequest) -> DispatchBatch {
        if request.method == INITIALIZE_METHOD {
            return self.initialize(request);
        }
        if request.method == SHUTDOWN_METHOD {
            return self.shutdown(request);
        }
        if self.state != ConnectionState::Ready {
            return error_batch(
                request.id,
                ProtocolErrorCode::NotInitialized,
                "Connection is not initialized",
            );
        }
        if let Some(method) = internal_method(self.transport, &request.method) {
            let request = ServerRequest {
                method: method.to_owned(),
                ..request
            };
            return self.workspace_request(request);
        }
        if !is_dispatchable_method(&request.method) {
            return error_batch(
                request.id,
                ProtocolErrorCode::MethodNotFound,
                "Unknown app-server method",
            );
        }
        match request.method.as_str() {
            "session/start" => self.session_start(request),
            "session/list"
            | "session/read"
            | "session/history/get"
            | "session/history/list"
            | "session/turns/list"
            | "session/rename"
            | "session/title/update"
            | "session/pin"
            | "session/relocate"
            | "session/log/read"
            | "session/resume"
            | "session/continue"
            | "session/fork"
            | "session/delete"
            | "session/history/clear" => self.saved_session_request(request),
            "session/close" => self.session_close(request),
            "session/compact/start" => self.session_compact_start(request),
            "session/settings/update" => self.session_settings_update(request),
            "session/overrides/write" => self.session_overrides_write(request),
            "session/rewind/read" => self.session_rewind_read(request),
            "session/rewind" => self.session_rewind(request),
            "turn/start" => self.turn_start(request),
            "turn/steer" => self.turn_steer(request),
            "turn/interrupt" => self.turn_interrupt(request),
            "session/context/inject" => self.context_inject(request),
            "callback/respond" => self.callback_respond(request),
            "workspace/trust/status"
            | "workspace/trust/decision"
            | "workspace/trust/untrustedConfig" => self.trust_request(request),
            method if crate::resources::mcp_catalog::handles(method) => {
                self.mcp_catalog_request(request)
            }
            method if review::is_review_method(method) => self.review_request(request),
            method if RESOURCE_METHODS.contains(&method) => self.resource_request(request),
            method if WORKSPACE_METHODS.contains(&method) => self.workspace_request(request),
            method if PROJECTS_METHODS.contains(&method) => self.projects_request(request),
            _ => error_batch(
                request.id,
                ProtocolErrorCode::MethodNotFound,
                "Method is not implemented in this release",
            ),
        }
    }

    fn handle_notification(&mut self, notification: Notification) -> DispatchBatch {
        match notification.method.as_str() {
            INITIALIZED_NOTIFICATION if self.state == ConnectionState::AwaitingInitialized => {
                self.state = ConnectionState::Ready;
                DispatchBatch::empty()
            }
            INITIALIZED_NOTIFICATION => {
                self.state = ConnectionState::Closed;
                DispatchBatch {
                    outbound: Vec::new(),
                    deferred: Vec::new(),
                    close_after_flush: true,
                }
            }
            EXIT_NOTIFICATION if self.state == ConnectionState::ShuttingDown => {
                self.state = ConnectionState::Closed;
                DispatchBatch {
                    outbound: Vec::new(),
                    deferred: Vec::new(),
                    close_after_flush: true,
                }
            }
            _ => DispatchBatch::empty(),
        }
    }

    fn initialize(&mut self, request: ServerRequest) -> DispatchBatch {
        if self.state != ConnectionState::New {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidRequest,
                "initialize may only be called once",
            );
        }
        let parsed = from_params::<InitializeParams>(&request.params);
        let params = match parsed {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.capabilities = params.capabilities;
        self.entrypoint = params.client_info.entrypoint.clone();
        // Sessions started on this connection publish their tools against what
        // the handshake just declared, so the delegation is recorded before the
        // first `session/start` can read it.
        self.server
            .client_tools
            .declare(&self.capabilities.client_tools);
        self.state = ConnectionState::AwaitingInitialized;
        let response = InitializeResponse {
            server_info: ServerInfo {
                name: "vibe-app-server".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            protocol_version: ProtocolVersion::V1,
            capabilities: ServerCapabilities {
                methods: advertised_methods(),
                callback_kinds: vec![CallbackKind::Approval, CallbackKind::UserInput],
                transports: vec![self.transport],
            },
        };
        success_batch(
            request.id,
            object(serde_json::to_value(response).unwrap_or_else(|_| json!({}))),
        )
    }

    fn shutdown(&mut self, request: ServerRequest) -> DispatchBatch {
        if self.state != ConnectionState::Ready {
            return error_batch(
                request.id,
                ProtocolErrorCode::NotInitialized,
                "Connection is not initialized",
            );
        }
        self.state = ConnectionState::ShuttingDown;
        success_batch(request.id, BTreeMap::new())
    }

    fn workspace_request(&mut self, request: ServerRequest) -> DispatchBatch {
        session_management::dispatch(self, request)
    }

    fn projects_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.dispatch_projects(request))
    }

    fn dispatch_projects(
        &mut self,
        mut request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(session_id) = &session_id {
            if let Some(batch) = self.attachment_error(request.id.clone(), session_id) {
                return Ok(batch);
            }
            let sessions = self.server.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| session_missing("Session was not found"))?;
            if matches!(
                request.method.as_str(),
                "loops/create" | "loops/delete" | "loops/clear"
            ) && session.active_turn.is_some()
            {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "Scheduled loops can only change while the session is idle",
                ));
            }
            if matches!(
                request.method.as_str(),
                "vibeCode/projects/open" | "vibeCode/teleport/start"
            ) {
                request.params.insert(
                    "workingDirectory".to_owned(),
                    json!(session.working_directory),
                );
            }
        }
        if self
            .server
            .projects
            .requires_deferred_dispatch(&request.method)
        {
            return Ok(DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::CloudRequest {
                    request_id: request.id,
                    method: request.method,
                    params: request.params,
                }],
                close_after_flush: false,
            });
        }
        let dispatch = self
            .server
            .projects
            .dispatch(&request.method, &request.params)?;
        Ok(projects_dispatch_batch(request.id, dispatch))
    }

    /// Reference `MCPCatalogService.dispatch`: the parameters are validated
    /// and the target resolved here, against the sessions this connection
    /// holds, and the call itself runs deferred.
    fn mcp_catalog_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = request
            .params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let call = match crate::resources::mcp_catalog::parse(&request.method, &params) {
            Ok(call) => call,
            Err(error) => return catalog_error_batch(request.id, error),
        };
        let target = {
            let sessions = match self.server.lock_sessions() {
                Ok(sessions) => sessions,
                Err(error) => return internal_error_batch(request.id, &error),
            };
            let attached = |session_id: &str| {
                sessions
                    .key(session_id)
                    .is_some_and(|key| self.attached_sessions.contains(key))
            };
            crate::resources::mcp_catalog::target(
                &call,
                &attached,
                !self.attached_sessions.is_empty(),
            )
        };
        match target {
            Ok(target) => DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::McpCatalog {
                    request_id: request.id,
                    call,
                    target,
                    publish: !self.attached_sessions.is_empty(),
                }],
                close_after_flush: false,
            },
            Err(error) => catalog_error_batch(request.id, error),
        }
    }

    fn resource_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.dispatch_resource(request))
    }

    fn dispatch_resource(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ProtocolFault::invalid_params("sessionId must be a non-empty string"))?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &session_id) {
            return Ok(batch);
        }
        // The three live-state reads are composed here rather than inside the
        // resource service: each one needs the configuration, the catalogs or
        // the session's own accounting, and the service holds none of them.
        if let Some(batch) = self.live_state_request(&request, &session_id) {
            return Ok(batch);
        }
        if request.method == "telemetry/record" {
            return self.record_telemetry(request);
        }
        if request.method.starts_with("feedback/") {
            return Ok(self.feedback_request(request, &session_id));
        }
        let session_active = {
            let sessions = self.server.lock_sessions()?;
            sessions
                .get(&session_id)
                .is_some_and(|session| session.active_turn.is_some())
        };
        if BACKEND_RESOURCE_METHODS.contains(&request.method.as_str())
            && self.server.resource_backend.is_some()
        {
            let command =
                ResourceBackendCommand::parse(&request.method, &request.params, session_active)?;
            return Ok(DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::ResourceRequest {
                    request_id: request.id,
                    session_id,
                    command,
                }],
                close_after_flush: false,
            });
        }
        let result = self
            .server
            .resources
            .lock()
            .map_err(|_| ProtocolFault::internal("Resource state lock is poisoned"))?
            .dispatch(&request.method, &request.params, session_active);
        Ok(resource_result_batch(
            request.id,
            &self.server,
            &session_id,
            &request.method,
            result,
        ))
    }

    /// Answers the three methods that report live session state.
    ///
    /// `runtime/read` composes what a client renders a session from,
    /// `stats/read` publishes the same accounting the sequenced statistics
    /// notification carries, and `account/read` classifies the configured
    /// credential. `None` means the request is not one of them.
    fn live_state_request(
        &self,
        request: &ServerRequest,
        session_id: &str,
    ) -> Option<DispatchBatch> {
        let result = match request.method.as_str() {
            "runtime/read" => {
                let Some(runtime) = self.server.runtime_snapshot(session_id) else {
                    return Some(error_batch(
                        request.id.clone(),
                        ProtocolErrorCode::NotFound,
                        "Session was not found",
                    ));
                };
                let ready = match self.server.resources.lock() {
                    Ok(resources) => resources.is_ready(),
                    Err(_) => false,
                };
                result_map([
                    ("runtime", runtime),
                    ("sessionLog", self.server.session_log_summary(session_id)),
                    ("ready", json!(ready)),
                ])
            }
            "stats/read" => {
                let (stats, context_window) = match self.server.lock_sessions() {
                    Ok(sessions) => {
                        let session = sessions.get(session_id);
                        (
                            public_stats(session),
                            session.map_or(0, |session| session.context_window),
                        )
                    }
                    Err(error) => {
                        return Some(internal_error_batch(request.id.clone(), &error));
                    }
                };
                let stats = priced(stats, self.server.workspace.active_model_pricing());
                result_map([("stats", stats), ("contextWindow", json!(context_window))])
            }
            // The plan comes from the console, so the answer waits on I/O.
            "account/read" => {
                return Some(DispatchBatch {
                    outbound: Vec::new(),
                    deferred: vec![DeferredWork::CloudRequest {
                        request_id: request.id.clone(),
                        method: request.method.clone(),
                        params: request.params.clone(),
                    }],
                    close_after_flush: false,
                });
            }
            _ => return None,
        };
        Some(success_batch(request.id.clone(), result))
    }

    /// Records a client-reported event against the attached session.
    ///
    /// The reference forwards the event to the agent loop's telemetry client,
    /// which ships it to the datalake under the open-properties envelope
    /// (`vibe/app_server/_resources.py:488-499`). This port now publishes that
    /// envelope too, so the event travels through the sink an adapter
    /// installed, carrying the client's own name and its properties unmodified.
    /// It is also written to the log file an operator reads back through
    /// `diagnostics/logs/read`, and both are dropped when `enable_telemetry` is
    /// off, which is the decision the reference delegates to the same key.
    fn record_telemetry(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TelemetryRecordParams>(&request.params)?;
        if self.server.workspace.telemetry_enabled() {
            let properties = params
                .properties
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let message = format!(
                "client event {} for session {} (correlateLastRequest={}, properties: {properties})",
                params.name, params.session_id, params.correlate_last_request
            );
            self.server
                .resources
                .lock()
                .map_err(|_| ProtocolFault::internal("Resource state lock is poisoned"))?
                .record_log(LogLevel::Info, &message);
            self.server.client_telemetry.record_client_event(
                &params.name,
                params.properties.into_iter().collect(),
                Some(&params.session_id),
                params.correlate_last_request,
            );
        }
        Ok(success_batch(request.id, BTreeMap::new()))
    }

    /// Reference `_dispatch_feedback`: whether to ask for a rating, from the
    /// session's conversation and the timestamps in the shared cache, and the
    /// record of what the user did with the prompt.
    fn feedback_request(&self, request: ServerRequest, session_id: &str) -> DispatchBatch {
        let cache = vibe_core::feedback::FeedbackCache::new(self.server.workspace.vibe_home());
        let now = i64::try_from(now_millis() / 1_000).unwrap_or(i64::MAX);
        if request.method == "feedback/record" {
            let action = request
                .params
                .get("action")
                .and_then(Value::as_str)
                .and_then(vibe_core::feedback::FeedbackAction::parse);
            let Some(action) = action else {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::InvalidParams,
                    "action must be asked, given, or snoozed",
                );
            };
            cache.record(action, now);
            return DispatchBatch {
                outbound: vec![success_bytes(request.id, BTreeMap::new())],
                deferred: Vec::new(),
                close_after_flush: false,
            };
        }
        let pending = request
            .params
            .get("pendingUserMessages")
            .and_then(Value::as_u64)
            .and_then(|pending| usize::try_from(pending).ok())
            .unwrap_or(0);
        let user_messages = match self.server.lock_sessions() {
            Ok(sessions) => sessions.get(session_id).map_or(0, count_user_messages),
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let is_mistral = self
            .server
            .workspace
            .layered_config()
            .load()
            .is_ok_and(|snapshot| {
                vibe_core::telemetry::is_active_model_mistral(&snapshot.effective)
            });
        let show = cache.should_show(
            self.server.client_telemetry.is_active(),
            is_mistral,
            user_messages.saturating_add(pending),
            now,
            vibe_core::feedback::uniform_roll(),
        );
        DispatchBatch {
            outbound: vec![success_bytes(
                request.id,
                result_map([
                    ("show", json!(show)),
                    (
                        "snoozeDurationSeconds",
                        json!(vibe_core::feedback::FEEDBACK_SNOOZED_COOLDOWN_SECONDS),
                    ),
                ]),
            )],
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }

    fn attachment_error(&self, request_id: RequestId, session_id: &str) -> Option<DispatchBatch> {
        let sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return Some(internal_error_batch(request_id, &error)),
        };
        let attached = sessions
            .key(session_id)
            .is_some_and(|key| self.attached_sessions.contains(key));
        (!attached).then(|| {
            error_batch(
                request_id,
                ProtocolErrorCode::Forbidden,
                "Session is not attached to this connection",
            )
        })
    }
}

impl Drop for ServerConnection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reference counts `agent_loop.messages` from the user that were not
/// injected; the live history holds them once a turn has run, the saved
/// transcript before that.
fn count_user_messages(session: &super::SessionRuntime) -> usize {
    if let Some(snapshot) = &session.snapshot {
        return snapshot
            .history
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Message {
                        metadata,
                        role: PublicMessageRole::User,
                        source,
                        ..
                    } if *source != Some(PublicMessageSource::Harness)
                        || metadata.turn_id.is_none()
                )
            })
            .count();
    }
    session.persisted.as_ref().map_or(0, |persisted| {
        persisted
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    vibe_core::events::ModelMessage::User {
                        injected: false,
                        ..
                    }
                )
            })
            .count()
    })
}
