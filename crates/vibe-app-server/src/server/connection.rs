//! One client connection, and the requests it dispatches.
//!
//! [`AppServer`] holds the sessions; a connection holds what is true of one
//! client: which sessions it attached, what capabilities it declared, and which
//! server-to-client requests are still outstanding. Every routed method is
//! answered here, against the server above.

mod callbacks;
mod plugins;
mod queue;
mod resources;
mod review_request;
pub(in crate::server) mod rewind;
mod route;
mod saved;
pub(in crate::server) use saved::NO_ROOT;
pub(in crate::server) use saved::reopen_start_params as reopen_params;
mod session;
mod trust;
mod turn;
pub(crate) use turn::bump_session;

use super::*;
use crate::params::object_of;
use crate::wire_validation;

pub(crate) use route::SKILLS_MUTATIONS;
use route::is_connector_method;
#[cfg(test)]
pub(crate) use route::{Route, route};

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
    /// The name the client gave itself, which a Teleport summary reports as
    /// its source.
    pub(super) client_name: Option<String>,
    /// What the handshake declared about the client, which request metadata
    /// reports (reference `_build_launch_context_from_services`).
    pub(super) launch: Option<vibe_core::telemetry::LaunchContext>,
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
            Envelope::Request(request) => {
                // A stdio client's extra keys are reported in the order it
                // wrote them, which the envelope's sorted map has lost.
                let ordered = (self.transport == TransportKind::Stdio)
                    .then(|| wire_validation::ordered_params(bytes))
                    .flatten();
                self.handle_request(request, ordered)
            }
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

    pub fn attach_session(&mut self, session_id: &str) -> Result<(), ServerError> {
        if self.state != ConnectionState::Ready {
            return Err(ServerError::NotInitialized);
        }
        let mut sessions = self.server.lock_sessions();
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
        let mut sessions = self.server.lock_sessions();
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
        {
            let mut sessions = self.server.lock_sessions();
            for session_id in &self.attached_sessions {
                if let Some(session) = sessions.get_mut(session_id) {
                    session.attachments = session.attachments.saturating_sub(1);
                    if session.attachments == 0 && session.status != SessionStatus::Closed {
                        released.push(session::Retired::of(session));
                    }
                }
            }
        }
        // A connection that goes away lets go of what it held, as the
        // reference's backend shutdown does: the lease, the worktree it stood
        // in, and the terminal's pointer to the session it leaves.
        for retired in released {
            let session_id = &retired.session_id;
            // Reference `VibeCodeController.close`: a push still waiting is
            // answered no and every run stops, so nothing ships after the
            // session is gone.
            self.server.spawn_vibe_code_reset(session_id);
            self.release_retired(&retired);
            let _ = self
                .server
                .workspace
                .close_saved_session(session_id, now_millis());
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

    fn handle_request(
        &mut self,
        request: ServerRequest,
        ordered: Option<wire_validation::Json>,
    ) -> DispatchBatch {
        if request.method == INITIALIZE_METHOD {
            return self.initialize(request, ordered);
        }
        if self.transport == TransportKind::InProcess && request.method == SHUTDOWN_METHOD {
            return self.shutdown(request);
        }
        if self.state != ConnectionState::Ready {
            return error_batch(
                request.id,
                ProtocolErrorCode::NotInitialized,
                "initialize must be the first request",
            );
        }
        if let Some(method) = internal_method(self.transport, &request.method) {
            let request = ServerRequest {
                method: method.to_owned(),
                ..request
            };
            return self.workspace_request(request);
        }
        if self.transport == TransportKind::InProcess {
            if !self.server.harness_selection().use_unified {
                return self.port_request(request);
            }
            // Reference `LocalHarness`: the terminal and editor clients reach
            // a unified session over the same routing a stdio client takes.
            // The start keeps this port's shape, which also carries the
            // model and mode a launch picked; a method the reference does not
            // declare is one of this port's own.
            if request.method == "session/start" {
                let id = request.id.clone();
                return answered(id, self.unified_session_start(request));
            }
            if !is_server_method(&request.method) {
                return self.port_request(request);
            }
            return self.reference_request(request, None);
        }
        if !is_server_method(&request.method) {
            let id = request.id.clone();
            return route::method_not_found(&request.method).into_batch(id);
        }
        self.reference_request(request, ordered)
    }

    /// Answers a request with this port's own handlers.
    ///
    /// In-process clients reach every method this way, including the shapes
    /// only this port speaks; a stdio request reaches it once the reference
    /// routing has validated it ([`Self::reference_request`]).
    fn port_request(&mut self, request: ServerRequest) -> DispatchBatch {
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
            "session/compact" | "session/compact/start" => self.session_compact_start(request),
            "session/settings/update" => self.session_settings_update(request),
            "session/overrides/write" => self.session_overrides_write(request),
            "session/rewind/read" => self.session_rewind_read(request),
            "session/rewind" => self.session_rewind(request),
            "turn/start" => self.turn_start(request),
            "turn/steer" => self.turn_steer(request),
            "turn/interrupt" => self.turn_interrupt(request),
            "session/turn/enqueue"
            | "session/turn/queue/read"
            | "session/turn/queue/remove"
            | "session/turn/queue/replace"
            | "session/turn/queue/resume" => self.turn_queue_request(request),
            "session/context/inject" => self.context_inject(request),
            "plugin_catalog/read" | "plugins/read" | "plugin/info" | "plugin/reload" => {
                self.plugin_request(request)
            }
            "callback/respond" => self.callback_respond(request),
            "workspace/trust/status"
            | "workspace/trust/decision"
            | "workspace/trust/untrustedConfig" => self.trust_request(request),
            method if crate::resources::mcp_catalog::handles(method) => {
                self.mcp_catalog_request(request)
            }
            method if review::is_review_method(method) => self.review_request(request),
            method if is_connector_method(method) => self.connector_request(request),
            "narration/summarize" => self.narration_request(request),
            // Only a unified session serves the provider view, which the
            // routing hands it before this; the legacy backend declines the
            // whole namespace (reference `_dispatch` in
            // `vibe/app_server/_handler.py`).
            "providerAuth/read" => error_batch(
                request.id,
                ProtocolErrorCode::NotImplemented,
                &format!("Provider auth is not supported: {}", request.method),
            ),
            method if RESOURCE_METHODS.contains(&method) => self.resource_request(request),
            method if WORKSPACE_METHODS.contains(&method) => self.workspace_request(request),
            method if PROJECTS_METHODS.contains(&method) => self.projects_request(request),
            method => {
                let id = request.id.clone();
                route::method_not_found(method).into_batch(id)
            }
        }
    }

    /// One `connector_catalog/*` or `connectors/*` call, answered off the
    /// request loop because the catalog and the authorization pages are
    /// fetched. The attached session rides along, as over stdio, since a call
    /// naming none may still act on it.
    fn connector_request(&self, request: ServerRequest) -> DispatchBatch {
        let mut params = request.params;
        if let Some(root) = self.root_id() {
            params.insert(CONNECTION_ROOT_PARAM.to_owned(), json!(root));
        }
        DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::CloudRequest {
                request_id: request.id,
                method: request.method,
                params,
            }],
            close_after_flush: false,
        }
    }

    /// A notification the connection does not expect ends it, as the
    /// reference's `_handle_notification` ends the connection it arrived on.
    fn handle_notification(&mut self, notification: Notification) -> DispatchBatch {
        match notification.method.as_str() {
            INITIALIZED_NOTIFICATION if self.state == ConnectionState::AwaitingInitialized => {
                self.state = ConnectionState::Ready;
                DispatchBatch::empty()
            }
            // This port's in-process clients end a connection with `shutdown`
            // and `exit`, and send nothing else the server has to refuse; the
            // reference protocol has neither, so over stdio `exit` is one
            // more notification the server does not know.
            EXIT_NOTIFICATION
                if self.transport == TransportKind::InProcess
                    && self.state == ConnectionState::ShuttingDown =>
            {
                self.close_for_protocol_error()
            }
            method
                if self.transport == TransportKind::InProcess
                    && method != INITIALIZED_NOTIFICATION =>
            {
                DispatchBatch::empty()
            }
            _ => self.close_for_protocol_error(),
        }
    }

    fn initialize(
        &mut self,
        request: ServerRequest,
        ordered: Option<wire_validation::Json>,
    ) -> DispatchBatch {
        if self.state != ConnectionState::New {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidRequest,
                "initialize may only be called once",
            );
        }
        let params = ordered.unwrap_or_else(|| {
            wire_validation::Json::from_value(&Value::Object(object_of(&request.params)))
        });
        let validated = match wire_validation::validate_lifecycle(INITIALIZE_METHOD, &params) {
            Ok(validated) => validated,
            Err(issues) => {
                return self
                    .rejected(INITIALIZE_METHOD, &issues)
                    .into_batch(request.id);
            }
        };
        let parsed = serde_json::from_value::<InitializeParams>(validated).map_err(|error| {
            ParamsRejection::at_root(format!("Invalid request parameters: {error}"))
        });
        let params = match parsed {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.capabilities = params.capabilities;
        self.entrypoint = params.client_info.entrypoint.clone();
        self.client_name = Some(params.client_info.name.clone());
        self.launch = Some(vibe_core::telemetry::LaunchContext {
            agent_entrypoint: wire_name(&params.client_info.entrypoint),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            client_name: params.client_info.name.clone(),
            client_version: params.client_info.version.clone(),
            terminal_emulator: Some(wire_name(&params.client_info.terminal_emulator)),
        });
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
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if session_id.is_none() && request.method.starts_with("vibeCode/") {
            crate::params::required_string(&request.params, "sessionId")
                .map_err(ProjectsServiceError::from)?;
        }
        if let Some(session_id) = &session_id {
            if let Some(batch) = self.attachment_error(request.id.clone(), session_id) {
                return Ok(batch);
            }
            let sessions = self.server.lock_sessions();
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
            if request.method.starts_with("vibeCode/") {
                let launch = ClientLaunch {
                    entrypoint: wire_name(&self.entrypoint),
                    client_name: self.client_name.clone(),
                };
                return Ok(DispatchBatch {
                    outbound: Vec::new(),
                    deferred: vec![DeferredWork::VibeCode {
                        request_id: request.id,
                        session_id: session.id.clone(),
                        method: request.method,
                        params: request.params,
                        launch,
                    }],
                    close_after_flush: false,
                });
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

    fn attachment_error(&self, request_id: RequestId, session_id: &str) -> Option<DispatchBatch> {
        let sessions = self.server.lock_sessions();
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

/// The name a handshake enum travels under.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}
