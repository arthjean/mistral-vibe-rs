use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::events::{
    CallbackKind as EngineCallbackKind, LifecycleState, ProjectionSnapshot, PublicContentBlock,
    PublicEntryGenerationStatus, PublicEntryMetadata, PublicError, PublicHistoryEntry,
    PublicMessageRole, PublicMessageSource, PublicTurn, PublicTurnStatus,
};
use vibe_protocol::{
    CallbackKind, ClientCapabilities, Envelope, ErrorResponse, InitializeParams,
    InitializeResponse, JsonRpcVersion, Notification, ProtocolError, ProtocolErrorCode,
    ProtocolVersion, RequestId, ServerCapabilities, ServerInfo, ServerRequest, SuccessResponse,
    TransportKind, decode_frame, encode_frame, validate_server_method,
};

const INITIALIZE_METHOD: &str = "initialize";
const INITIALIZED_NOTIFICATION: &str = "initialized";
const SHUTDOWN_METHOD: &str = "shutdown";
const EXIT_NOTIFICATION: &str = "exit";
const IMPLEMENTED_METHODS: [&str; 8] = [
    "callback/respond",
    "session/close",
    "session/context/inject",
    "session/read",
    "session/start",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    New,
    AwaitingInitialized,
    Ready,
    ShuttingDown,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredWork {
    RunTurn {
        session_id: String,
        turn_id: String,
        prompt: String,
        input: Vec<PublicContentBlock>,
        client_user_message_id: Option<String>,
        auto_title: Option<String>,
        user_display_content: Option<Value>,
        mention_stats: Option<Value>,
    },
    InterruptTurn {
        session_id: String,
        turn_id: String,
    },
    SteerTurn {
        session_id: String,
        turn_id: String,
        content: String,
    },
    InjectContext {
        session_id: String,
        content: String,
        as_message: bool,
    },
    ResolveCallback {
        session_id: String,
        turn_id: String,
        callback_id: String,
        accepted: bool,
        value: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBatch {
    pub outbound: Vec<Vec<u8>>,
    pub deferred: Vec<DeferredWork>,
    pub close_after_flush: bool,
}

impl DispatchBatch {
    fn empty() -> Self {
        Self {
            outbound: Vec::new(),
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppServer {
    sessions: Arc<Mutex<BTreeMap<String, SessionRuntime>>>,
    next_session: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
    next_callback: Arc<AtomicU64>,
    next_entry: Arc<AtomicU64>,
}

impl Default for AppServer {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            next_session: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            next_callback: Arc::new(AtomicU64::new(1)),
            next_entry: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl AppServer {
    #[must_use]
    pub fn connect(&self, transport: TransportKind) -> ServerConnection {
        ServerConnection {
            server: self.clone(),
            state: ConnectionState::New,
            transport,
            capabilities: ClientCapabilities::default(),
            attached_sessions: BTreeSet::new(),
            pending_server_requests: HashMap::new(),
        }
    }

    pub fn complete_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
    ) -> Result<Vec<u8>, ServerError> {
        self.complete_turn_with_stop_reason(session_id, turn_id, snapshot, None)
    }

    pub fn turn_started(&self, session_id: &str, turn_id: &str) -> Result<Vec<u8>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = session_mut_by_id_or_alias(&mut sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let turn = session
            .latest_turn
            .as_ref()
            .filter(|turn| turn.id == turn_id)
            .ok_or_else(|| ServerError::StaleTurn(turn_id.to_owned()))?;
        let turn = turn.clone();
        let event_id = next_event_id(session);
        encode_notification(
            "turn/started",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session.id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        )
    }

    pub fn complete_turn_with_stop_reason(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
        stop_reason: Option<vibe_core::events::PublicTurnStopReason>,
    ) -> Result<Vec<u8>, ServerError> {
        self.complete_turn_with_details(session_id, turn_id, snapshot, stop_reason, None)
    }

    pub fn complete_turn_with_details(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
        stop_reason: Option<vibe_core::events::PublicTurnStopReason>,
        error: Option<PublicError>,
    ) -> Result<Vec<u8>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let source_key = session_key_by_id_or_alias(&sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        let session = sessions
            .get(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let was_closed = session.status == SessionStatus::Closed;
        let started_at = session.active_turn_started_at.unwrap_or_default();
        if !matches!(
            snapshot.lifecycle,
            LifecycleState::Completed | LifecycleState::Cancelled | LifecycleState::Failed
        ) {
            return Err(ServerError::NonTerminalCompletion(snapshot.lifecycle));
        }
        let target_session_id = snapshot.session_id.clone();
        if target_session_id != source_key && sessions.contains_key(&target_session_id) {
            return Err(ServerError::SessionConflict(target_session_id));
        }
        let status = match snapshot.lifecycle {
            LifecycleState::Completed => PublicTurnStatus::Completed,
            LifecycleState::Cancelled => PublicTurnStatus::Interrupted,
            LifecycleState::Failed => PublicTurnStatus::Failed,
            _ => return Err(ServerError::NonTerminalCompletion(snapshot.lifecycle)),
        };
        let completed_at = now_millis();
        let turn = PublicTurn {
            id: turn_id.to_owned(),
            session_id: target_session_id.clone(),
            status,
            started_at,
            completed_at: Some(completed_at),
            error: (status == PublicTurnStatus::Failed).then(|| {
                error.unwrap_or_else(|| PublicError {
                    message: "Turn failed".to_owned(),
                    code: Some("turn_failed".to_owned()),
                    details: Value::Null,
                })
            }),
            stop_reason,
        };
        let mut session = sessions
            .remove(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        session.aliases.insert(session_id.to_owned());
        session.id.clone_from(&target_session_id);
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.status = if was_closed {
            SessionStatus::Closed
        } else {
            match snapshot.lifecycle {
                LifecycleState::Completed => SessionStatus::Idle,
                LifecycleState::Cancelled => SessionStatus::Cancelled,
                LifecycleState::Failed => SessionStatus::Failed,
                _ => SessionStatus::Idle,
            }
        };
        session.snapshot = Some(snapshot.clone());
        session.latest_turn = Some(turn.clone());
        session.updated_at = completed_at;
        let event_id = next_event_id(&mut session);
        sessions.insert(target_session_id.clone(), session);
        encode_notification(
            "turn/completed",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(target_session_id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        )
    }

    pub fn fail_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        message: &str,
    ) -> Result<Vec<u8>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = session_mut_by_id_or_alias(&mut sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let was_closed = session.status == SessionStatus::Closed;
        let started_at = session.active_turn_started_at.unwrap_or_default();
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.pending_callback = None;
        session.status = if was_closed {
            SessionStatus::Closed
        } else {
            SessionStatus::Failed
        };
        let turn = PublicTurn {
            id: turn_id.to_owned(),
            session_id: session_id.to_owned(),
            status: PublicTurnStatus::Failed,
            started_at,
            completed_at: Some(now_millis()),
            error: Some(PublicError {
                message: message.to_owned(),
                code: Some("turn_failed".to_owned()),
                details: Value::Null,
            }),
            stop_reason: None,
        };
        session.latest_turn = Some(turn.clone());
        session.updated_at = turn.completed_at.unwrap_or(started_at);
        let event_id = next_event_id(session);
        encode_notification(
            "turn/completed",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session_id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        )
    }

    pub fn request_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        prompt: impl Into<String>,
    ) -> Result<(String, Vec<u8>), ServerError> {
        if kind == EngineCallbackKind::ConnectorAuth {
            return Err(ServerError::UnsupportedCallbackKind(kind));
        }
        let prompt = prompt.into();
        let mut sessions = self.lock_sessions()?;
        let session = session_mut_by_id_or_alias(&mut sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        if session.pending_callback.is_some() {
            return Err(ServerError::CallbackConflict);
        }
        let callback_sequence = self.next_callback.fetch_add(1, Ordering::Relaxed);
        let callback_id = format!("callback-{callback_sequence}");
        let timestamp = now_millis();
        let detail = match kind {
            EngineCallbackKind::Approval => json!({
                "kind": "approval",
                "effect": {
                    "kind": "tool",
                    "toolName": "callback",
                    "input": null,
                    "display": {
                        "summary": prompt,
                        "content": null,
                        "suffix": "",
                        "verb": "",
                        "message": null,
                        "settledVerb": "",
                        "settledMessage": null,
                        "statusText": "",
                    },
                },
                "requiredPermissions": [],
                "choices": [
                    "approve",
                    "approve_for_session",
                    "approve_permanently",
                    "deny",
                    "cancel_turn",
                ],
                "relatedEntryId": null,
            }),
            EngineCallbackKind::UserInput => json!({
                "kind": "user_input",
                "request": {
                    "questions": [{
                        "question": prompt,
                        "header": "",
                        "options": [
                            {"label": "Yes", "description": ""},
                            {"label": "No", "description": ""},
                        ],
                        "multiSelect": false,
                        "hideOther": false,
                    }],
                    "footerNote": null,
                },
                "relatedEntryId": null,
            }),
            EngineCallbackKind::ConnectorAuth => {
                return Err(ServerError::UnsupportedCallbackKind(kind));
            }
        };
        let callback = PublicHistoryEntry::Callback {
            metadata: PublicEntryMetadata {
                id: format!("callback:{callback_id}"),
                session_id: session_id.to_owned(),
                turn_id: Some(turn_id.to_owned()),
                created_at: timestamp,
                updated_at: timestamp,
                generation_status: PublicEntryGenerationStatus::InProgress,
                related_entry_id: None,
            },
            callback_id: callback_id.clone(),
            title: prompt,
            detail,
            state: vibe_core::events::PublicCallbackState::Open,
        };
        session.status = SessionStatus::WaitingCallback;
        session.updated_at = timestamp;
        session.pending_callback = Some(PendingCallback {
            id: callback_id.clone(),
            kind,
            entry: callback.clone(),
        });
        let request = encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: RequestId::Integer(i64::try_from(callback_sequence).unwrap_or(i64::MAX)),
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback))]),
        }))?;
        Ok((callback_id, request))
    }

    pub(crate) fn sequence_event(&self, session_id: &str) -> Result<u64, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = session_mut_by_id_or_alias(&mut sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        Ok(next_event_id(session))
    }

    pub(crate) fn handoff_active_turn(
        &self,
        old_session_id: &str,
        new_session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
        summary_length: usize,
        emitted_at: u64,
    ) -> Result<Vec<u8>, ServerError> {
        if snapshot.session_id != new_session_id {
            return Err(ServerError::SessionConflict(new_session_id.to_owned()));
        }
        let mut sessions = self.lock_sessions()?;
        let source_key = session_key_by_id_or_alias(&sessions, old_session_id)
            .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?;
        if source_key != new_session_id && sessions.contains_key(new_session_id) {
            return Err(ServerError::SessionConflict(new_session_id.to_owned()));
        }
        let mut session = sessions
            .remove(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            sessions.insert(source_key, session);
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        session.aliases.insert(old_session_id.to_owned());
        session.id = new_session_id.to_owned();
        session.snapshot = Some(snapshot);
        session.updated_at = now_millis();
        session.event_watermark = 0;
        if let Some(turn) = session.latest_turn.as_mut() {
            turn.session_id = new_session_id.to_owned();
        }
        let event_id = next_event_id(&mut session);
        let state = public_session_state(&session);
        sessions.insert(new_session_id.to_owned(), session);
        encode_notification(
            "session/compacted",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(new_session_id)),
                ("oldSessionId", json!(old_session_id)),
                ("state", state),
                ("sessionLog", json!({"enabled": false})),
                ("summaryLength", json!(summary_length)),
                ("emittedAt", json!(emitted_at)),
            ]),
        )
    }

    fn reject_callback(
        &self,
        route: &CallbackRoute,
        reason: &str,
    ) -> Result<Vec<DeferredWork>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = session_mut_by_id_or_alias(&mut sessions, &route.session_id)
            .ok_or_else(|| ServerError::SessionNotFound(route.session_id.clone()))?;
        if session.active_turn.as_deref() != Some(&route.turn_id) {
            return Err(ServerError::StaleTurn(route.turn_id.clone()));
        }
        let Some(callback) = &session.pending_callback else {
            return Ok(Vec::new());
        };
        if callback.id != route.callback_id {
            return Err(ServerError::CallbackConflict);
        }
        session.pending_callback = None;
        session.status = SessionStatus::Cancelled;
        session.updated_at = now_millis();
        Ok(vec![
            DeferredWork::ResolveCallback {
                session_id: route.session_id.clone(),
                turn_id: route.turn_id.clone(),
                callback_id: route.callback_id.clone(),
                accepted: false,
                value: Some(reason.to_owned()),
            },
            DeferredWork::InterruptTurn {
                session_id: route.session_id.clone(),
                turn_id: route.turn_id.clone(),
            },
        ])
    }

    pub fn session(&self, session_id: &str) -> Result<SessionView, ServerError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(session_id)
            .map(SessionView::from)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, SessionRuntime>>, ServerError> {
        self.sessions.lock().map_err(|_| ServerError::StatePoisoned)
    }
}

pub struct ServerConnection {
    server: AppServer,
    state: ConnectionState,
    transport: TransportKind,
    capabilities: ClientCapabilities,
    attached_sessions: BTreeSet<String>,
    pending_server_requests: HashMap<RequestId, CallbackRoute>,
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
        match frame {
            Envelope::Request(request) => self.handle_request(request),
            Envelope::Notification(notification) => self.handle_notification(notification),
            Envelope::Success(response) => self.handle_server_success(response),
            Envelope::Error(response) => self.handle_server_error(response),
        }
    }

    pub fn request_callback(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        prompt: impl Into<String>,
    ) -> Result<(String, Vec<u8>), ServerError> {
        if self.state != ConnectionState::Ready {
            return Err(ServerError::NotInitialized);
        }
        let supported = match kind {
            EngineCallbackKind::Approval => self
                .capabilities
                .callback_kinds
                .contains(&CallbackKind::Approval),
            EngineCallbackKind::UserInput => self
                .capabilities
                .callback_kinds
                .contains(&CallbackKind::UserInput),
            EngineCallbackKind::ConnectorAuth => false,
        };
        if !supported {
            return Err(ServerError::UnsupportedClientCallbackKind(kind));
        }
        let (callback_id, bytes) = self
            .server
            .request_callback(session_id, turn_id, kind, prompt)?;
        let request_id = match decode_frame(&bytes)? {
            Envelope::Request(request) => request.id,
            _ => return Err(ServerError::InvalidCallbackRequest),
        };
        self.pending_server_requests.insert(
            request_id,
            CallbackRoute {
                session_id: session_id.to_owned(),
                turn_id: turn_id.to_owned(),
                callback_id: callback_id.clone(),
            },
        );
        Ok((callback_id, bytes))
    }

    fn handle_server_success(&mut self, response: SuccessResponse) -> DispatchBatch {
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
        if accepted {
            DispatchBatch::empty()
        } else {
            match self
                .server
                .reject_callback(&route, "Client did not accept callback delivery")
            {
                Ok(deferred) => DispatchBatch {
                    outbound: Vec::new(),
                    deferred,
                    close_after_flush: false,
                },
                Err(_) => self.close_for_protocol_error(),
            }
        }
    }

    fn handle_server_error(&mut self, response: ErrorResponse) -> DispatchBatch {
        let Some(route) = self.pending_server_requests.remove(&response.id) else {
            return self.close_for_protocol_error();
        };
        match self.server.reject_callback(&route, &response.error.message) {
            Ok(deferred) => DispatchBatch {
                outbound: Vec::new(),
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
        let session = session_mut_by_id_or_alias(&mut sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if attached_key(session, &self.attached_sessions).is_none() {
            session.attachments = session.attachments.saturating_add(1);
            self.attached_sessions.insert(session.id.clone());
        }
        Ok(())
    }

    pub fn detach_session(&mut self, session_id: &str) -> Result<(), ServerError> {
        let mut sessions = self.server.lock_sessions()?;
        let session = session_mut_by_id_or_alias(&mut sessions, session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if let Some(key) = attached_key(session, &self.attached_sessions) {
            self.attached_sessions.remove(&key);
            session.attachments = session.attachments.saturating_sub(1);
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if let Ok(mut sessions) = self.server.lock_sessions() {
            for session_id in &self.attached_sessions {
                if let Some(session) = session_mut_by_id_or_alias(&mut sessions, session_id) {
                    session.attachments = session.attachments.saturating_sub(1);
                }
            }
        }
        self.attached_sessions.clear();
        self.pending_server_requests.clear();
        self.state = ConnectionState::Closed;
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
        if validate_server_method(&request.method).is_err() {
            return error_batch(
                request.id,
                ProtocolErrorCode::MethodNotFound,
                "Unknown app-server method",
            );
        }
        match request.method.as_str() {
            "session/start" => self.session_start(request),
            "session/read" => self.session_read(request),
            "session/close" => self.session_close(request),
            "turn/start" => self.turn_start(request),
            "turn/steer" => self.turn_steer(request),
            "turn/interrupt" => self.turn_interrupt(request),
            "session/context/inject" => self.context_inject(request),
            "callback/respond" => self.callback_respond(request),
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
                self.close();
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
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        self.capabilities = params.capabilities;
        self.state = ConnectionState::AwaitingInitialized;
        let response = InitializeResponse {
            server_info: ServerInfo {
                name: "vibe-app-server".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            protocol_version: ProtocolVersion::V1,
            capabilities: ServerCapabilities {
                methods: IMPLEMENTED_METHODS
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
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

    fn session_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionStartParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        let _session_open_options = (params.headless, params.history_limit);
        if !(1..=500).contains(&params.history_limit) {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "historyLimit must be between 1 and 500",
            );
        }
        let max_price_micros = match params.max_price {
            Some(price) => match price_dollars_to_micros(price) {
                Some(value) => params.max_price_micros.or(Some(value)),
                None => {
                    return error_batch(
                        request.id,
                        ProtocolErrorCode::InvalidParams,
                        "maxPrice must be a finite non-negative number",
                    );
                }
            },
            None => params.max_price_micros,
        };
        let session_id = params.session_id.unwrap_or_else(|| {
            generated_session_id(self.server.next_session.fetch_add(1, Ordering::Relaxed))
        });
        let created_at = now_millis();
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        if sessions.contains_key(&session_id) {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Session already exists",
            );
        }
        sessions.insert(
            session_id.clone(),
            SessionRuntime {
                id: session_id.clone(),
                working_directory: params.working_directory.unwrap_or_else(|| ".".to_owned()),
                intent: SessionIntent {
                    add_directories: params.add_directories,
                    trusted: params.trusted,
                    agent: params.agent,
                    tool_filters: params.tool_filters,
                    enabled_tools: params.enabled_tools.unwrap_or_default(),
                    disabled_tools: params.disabled_tools,
                    mcp_servers: params.mcp_servers,
                    max_turns: params.max_turns,
                    max_tokens: params.max_tokens,
                    max_price_micros,
                    auto_approve: params.auto_approve,
                    resume: params.resume,
                    continue_session: params.continue_session,
                },
                status: SessionStatus::Idle,
                active_turn: None,
                active_turn_started_at: None,
                pending_callback: None,
                resolved_callbacks: BTreeMap::new(),
                context: Vec::new(),
                steering: Vec::new(),
                snapshot: None,
                attachments: 1,
                aliases: BTreeSet::new(),
                created_at,
                updated_at: created_at,
                latest_turn: None,
                event_watermark: 0,
            },
        );
        self.attached_sessions.insert(session_id.clone());
        let state = sessions
            .get(&session_id)
            .map(public_session_state)
            .unwrap_or(Value::Null);
        success_batch(request.id, result_map([("state", state)]))
    }

    fn session_read(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        match self.server.session(&params.session_id) {
            Ok(_) => match self.server.lock_sessions() {
                Ok(sessions) => match sessions.get(&params.session_id) {
                    Some(session) => success_batch(
                        request.id,
                        result_map([("state", public_session_state(session))]),
                    ),
                    None => {
                        error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found")
                    }
                },
                Err(error) => internal_error_batch(request.id, &error),
            },
            Err(ServerError::SessionNotFound(_)) => {
                error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found")
            }
            Err(error) => internal_error_batch(request.id, &error),
        }
    }

    fn session_close(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        let active_turn = session.active_turn.clone();
        session.status = SessionStatus::Closed;
        session.updated_at = now_millis();
        session.pending_callback = None;
        if let Some(key) = attached_key(session, &self.attached_sessions) {
            self.attached_sessions.remove(&key);
            session.attachments = session.attachments.saturating_sub(1);
        }
        self.state = ConnectionState::Closed;
        DispatchBatch {
            outbound: success_bytes(request.id, BTreeMap::new())
                .into_iter()
                .collect(),
            deferred: active_turn
                .map(|turn_id| DeferredWork::InterruptTurn {
                    session_id: params.session_id,
                    turn_id,
                })
                .into_iter()
                .collect(),
            close_after_flush: true,
        }
    }

    fn turn_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<TurnStartParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        let prompt = content_text(&params.input);
        if prompt.trim().is_empty() {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "Prompt must not be empty",
            );
        }
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        if session.active_turn.is_some() || session.status == SessionStatus::Closed {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Session cannot start another turn",
            );
        }
        let turn_sequence = self.server.next_turn.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!("turn-{turn_sequence}");
        session.active_turn = Some(turn_id.clone());
        let started_at = now_millis();
        session.active_turn_started_at = Some(started_at);
        session.status = SessionStatus::Running;
        let turn = PublicTurn {
            id: turn_id.clone(),
            session_id: params.session_id,
            status: PublicTurnStatus::InProgress,
            started_at,
            completed_at: None,
            error: None,
            stop_reason: None,
        };
        session.latest_turn = Some(turn.clone());
        session.updated_at = started_at;
        DispatchBatch {
            outbound: success_bytes(request.id, result_map([("turn", json!(turn))]))
                .into_iter()
                .collect(),
            deferred: vec![DeferredWork::RunTurn {
                session_id: session.id.clone(),
                turn_id,
                prompt,
                input: params.input,
                client_user_message_id: params.client_user_message_id,
                auto_title: params.auto_title,
                user_display_content: params.user_display_content,
                mention_stats: params.mention_stats,
            }],
            close_after_flush: false,
        }
    }

    fn turn_steer(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<TurnSteerParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        let session_id = params.session_id.clone();
        let turn_id = params.expected_turn_id.clone();
        let content = content_text(&params.input);
        let _steering_metadata = (
            &params.client_user_message_id,
            params.inject_invoked_skill,
            &params.mention_stats,
        );
        let expected_turn_id = params.expected_turn_id.clone();
        let lookup_turn_id = expected_turn_id.clone();
        self.mutate_active_turn(request.id, &params.session_id, &lookup_turn_id, |session| {
            if session.status != SessionStatus::Running {
                return Err((ProtocolErrorCode::NotSteerable, "Turn is not steerable"));
            }
            session.steering.push(content.clone());
            session.updated_at = now_millis();
            Ok((
                result_map([("turnId", json!(turn_id))]),
                vec![DeferredWork::SteerTurn {
                    session_id,
                    turn_id: expected_turn_id,
                    content,
                }],
            ))
        })
    }

    fn context_inject(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<ContextInjectParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let content = content_text(&params.input);
        let _injection_metadata = (params.inject_invoked_skill, &params.mention_stats);
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        if session.active_turn.is_some() || session.status != SessionStatus::Idle {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Use turn/steer while a turn is active",
            );
        }
        let sequence = self.server.next_entry.fetch_add(1, Ordering::Relaxed);
        let timestamp = now_millis();
        session.context.push(content.clone());
        session.updated_at = timestamp;
        let metadata = PublicEntryMetadata {
            id: params
                .client_user_message_id
                .unwrap_or_else(|| format!("entry-{sequence}")),
            session_id: params.session_id.clone(),
            turn_id: Some(format!("injection:{sequence}")),
            created_at: timestamp,
            updated_at: timestamp,
            generation_status: PublicEntryGenerationStatus::Completed,
            related_entry_id: None,
        };
        let entry = if params.as_message {
            PublicHistoryEntry::Message {
                metadata,
                role: PublicMessageRole::User,
                content: params.input,
                source: Some(PublicMessageSource::Harness),
                user_display_content: None,
            }
        } else {
            PublicHistoryEntry::Checkpoint {
                metadata,
                kind: "context_injected".to_owned(),
                message: None,
                details: json!({"content": content}),
            }
        };
        DispatchBatch {
            outbound: success_bytes(request.id, result_map([("entries", json!([entry]))]))
                .into_iter()
                .collect(),
            deferred: vec![DeferredWork::InjectContext {
                session_id: params.session_id,
                content,
                as_message: params.as_message,
            }],
            close_after_flush: false,
        }
    }

    fn turn_interrupt(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<TurnParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        if session.active_turn.as_deref() != Some(&params.expected_turn_id) {
            return error_batch(request.id, ProtocolErrorCode::StaleTurn, "Turn is stale");
        }
        session.status = SessionStatus::Cancelled;
        session.updated_at = now_millis();
        session.pending_callback = None;
        DispatchBatch {
            outbound: success_bytes(request.id, result_map([("interrupted", json!(true))]))
                .into_iter()
                .collect(),
            deferred: vec![DeferredWork::InterruptTurn {
                session_id: params.session_id,
                turn_id: params.expected_turn_id,
            }],
            close_after_flush: false,
        }
    }

    fn callback_respond(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<CallbackResponseParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        let Some(turn_id) = session.active_turn.clone() else {
            return error_batch(request.id, ProtocolErrorCode::StaleTurn, "Turn is stale");
        };
        let Some(kind) = callback_output_kind(&params.output) else {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "Callback output kind is unsupported",
            );
        };
        let accepted = callback_output_accepted(&params.output);
        let value = serde_json::to_string(&params.output).ok();
        let Some(callback) = &session.pending_callback else {
            if let Some(resolved) = session.resolved_callbacks.get(&params.callback_id) {
                if resolved.kind == kind && resolved.output == params.output {
                    return success_batch(request.id, result_map([("status", json!("duplicate"))]));
                }
                return error_batch(
                    request.id,
                    ProtocolErrorCode::Conflict,
                    "Callback already has a different answer",
                );
            }
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Callback is not pending",
            );
        };
        if callback.id != params.callback_id {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Callback ID does not match",
            );
        }
        if callback.kind != kind {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "Callback kind does not match",
            );
        }
        session.pending_callback = None;
        session.resolved_callbacks.insert(
            params.callback_id.clone(),
            ResolvedCallback {
                kind,
                output: params.output.clone(),
            },
        );
        session.status = if accepted {
            SessionStatus::Running
        } else {
            SessionStatus::Cancelled
        };
        session.updated_at = now_millis();
        let result = result_map([("status", json!("accepted"))]);
        let mut deferred = vec![DeferredWork::ResolveCallback {
            session_id: params.session_id.clone(),
            turn_id: turn_id.clone(),
            callback_id: params.callback_id,
            accepted,
            value,
        }];
        if !accepted {
            deferred.push(DeferredWork::InterruptTurn {
                session_id: params.session_id,
                turn_id,
            });
        }
        DispatchBatch {
            outbound: success_bytes(request.id, result).into_iter().collect(),
            deferred,
            close_after_flush: false,
        }
    }

    fn mutate_active_turn(
        &mut self,
        request_id: RequestId,
        session_id: &str,
        turn_id: &str,
        mutation: impl FnOnce(
            &mut SessionRuntime,
        ) -> Result<
            (BTreeMap<String, Value>, Vec<DeferredWork>),
            (ProtocolErrorCode, &'static str),
        >,
    ) -> DispatchBatch {
        if let Some(batch) = self.attachment_error(request_id.clone(), session_id) {
            return batch;
        }
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request_id, &error),
        };
        let Some(session) = sessions.get_mut(session_id) else {
            return error_batch(request_id, ProtocolErrorCode::NotFound, "Session not found");
        };
        if session.active_turn.as_deref() != Some(turn_id) {
            return error_batch(request_id, ProtocolErrorCode::StaleTurn, "Turn is stale");
        }
        match mutation(session) {
            Ok((result, deferred)) => DispatchBatch {
                outbound: success_bytes(request_id, result).into_iter().collect(),
                deferred,
                close_after_flush: false,
            },
            Err((code, message)) => error_batch(request_id, code, message),
        }
    }

    fn attachment_error(&self, request_id: RequestId, session_id: &str) -> Option<DispatchBatch> {
        let sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return Some(internal_error_batch(request_id, &error)),
        };
        let attached = session_by_id_or_alias(&sessions, session_id)
            .is_some_and(|session| attached_key(session, &self.attached_sessions).is_some());
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    WaitingCallback,
    Cancelled,
    Failed,
    Closed,
}

#[derive(Debug, Clone)]
struct SessionRuntime {
    id: String,
    working_directory: String,
    intent: SessionIntent,
    status: SessionStatus,
    active_turn: Option<String>,
    active_turn_started_at: Option<u64>,
    pending_callback: Option<PendingCallback>,
    resolved_callbacks: BTreeMap<String, ResolvedCallback>,
    context: Vec<String>,
    steering: Vec<String>,
    snapshot: Option<ProjectionSnapshot>,
    attachments: u32,
    aliases: BTreeSet<String>,
    created_at: u64,
    updated_at: u64,
    latest_turn: Option<PublicTurn>,
    event_watermark: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCallback {
    id: String,
    kind: EngineCallbackKind,
    entry: PublicHistoryEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCallback {
    kind: EngineCallbackKind,
    output: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallbackRoute {
    session_id: String,
    turn_id: String,
    callback_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub working_directory: String,
    pub intent: SessionIntent,
    pub status: SessionStatus,
    pub active_turn: Option<String>,
    pub pending_callback: Option<String>,
    pub context: Vec<String>,
    pub steering: Vec<String>,
    pub snapshot: Option<ProjectionSnapshot>,
    pub attachments: u32,
}

impl From<&SessionRuntime> for SessionView {
    fn from(session: &SessionRuntime) -> Self {
        Self {
            id: session.id.clone(),
            working_directory: session.working_directory.clone(),
            intent: session.intent.clone(),
            status: session.status,
            active_turn: session.active_turn.clone(),
            pending_callback: session
                .pending_callback
                .as_ref()
                .map(|callback| callback.id.clone()),
            context: session.context.clone(),
            steering: session.steering.clone(),
            snapshot: session.snapshot.clone(),
            attachments: session.attachments,
        }
    }
}

fn public_session_state(session: &SessionRuntime) -> Value {
    let history = session
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.history.clone())
        .unwrap_or_default();
    let status = match session.status {
        SessionStatus::Idle | SessionStatus::Cancelled => json!({"type": "idle"}),
        SessionStatus::Running => json!({
            "type": "running",
            "activeTurnId": session.active_turn,
        }),
        SessionStatus::WaitingCallback => json!({
            "type": "blocked",
            "activeTurnId": session.active_turn,
            "callbackId": session.pending_callback.as_ref().map(|callback| &callback.id),
            "reason": "Waiting for callback",
        }),
        SessionStatus::Failed => json!({
            "type": "failed",
            "message": "Turn failed",
        }),
        SessionStatus::Closed => json!({"type": "archived"}),
    };
    let preview = history
        .iter()
        .rev()
        .find_map(|entry| match entry {
            PublicHistoryEntry::Message { content, .. } => Some(content_text(content)),
            _ => None,
        })
        .unwrap_or_default();
    json!({
        "format": "vibe.public-session-state/v1",
        "eventId": session.event_watermark,
        "session": {
            "id": session.id,
            "rootSessionId": session.aliases.first().unwrap_or(&session.id),
            "parentSessionId": null,
            "title": session.snapshot.as_ref().and_then(|snapshot| snapshot.title.as_ref()),
            "preview": preview,
            "status": status,
            "createdAt": session.created_at,
            "updatedAt": session.updated_at,
            "cwd": session.working_directory,
            "workspaceRoots": session.intent.add_directories,
            "model": null,
            "agent": null,
            "tokenUsage": null,
        },
        "history": {
            "entries": history,
            "cursor": {"before": null, "after": null},
            "range": "latest",
        },
        "activeCallbacks": session
            .pending_callback
            .iter()
            .map(|callback| callback.entry.clone())
            .collect::<Vec<_>>(),
        "latestTurn": session.latest_turn,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionStartParams {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, rename = "cwd", alias = "workingDirectory")]
    working_directory: Option<String>,
    #[serde(default, rename = "workspaceRoots", alias = "addDirectories")]
    add_directories: Vec<String>,
    #[serde(default, rename = "trustWorkspace", alias = "trusted")]
    trusted: bool,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tool_filters: Vec<String>,
    #[serde(default)]
    enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    disabled_tools: Vec<String>,
    #[serde(default)]
    mcp_servers: Vec<Value>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default, rename = "maxSessionTokens", alias = "maxTokens")]
    max_tokens: Option<u64>,
    #[serde(default)]
    max_price_micros: Option<u64>,
    #[serde(default)]
    max_price: Option<f64>,
    #[serde(default)]
    auto_approve: bool,
    #[serde(default)]
    resume: Option<String>,
    #[serde(default, rename = "continue")]
    continue_session: bool,
    #[serde(default)]
    headless: bool,
    #[serde(default = "default_history_limit")]
    history_limit: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIntent {
    pub add_directories: Vec<String>,
    pub trusted: bool,
    pub agent: Option<String>,
    pub tool_filters: Vec<String>,
    pub enabled_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub mcp_servers: Vec<Value>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_price_micros: Option<u64>,
    pub auto_approve: bool,
    pub resume: Option<String>,
    #[serde(rename = "continue")]
    pub continue_session: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TurnStartParams {
    session_id: String,
    input: Vec<PublicContentBlock>,
    #[serde(default)]
    client_user_message_id: Option<String>,
    #[serde(default)]
    auto_title: Option<String>,
    #[serde(default)]
    user_display_content: Option<Value>,
    #[serde(default)]
    mention_stats: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TurnParams {
    session_id: String,
    expected_turn_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TurnSteerParams {
    session_id: String,
    expected_turn_id: String,
    input: Vec<PublicContentBlock>,
    #[serde(default)]
    client_user_message_id: Option<String>,
    #[serde(default = "default_true")]
    inject_invoked_skill: bool,
    #[serde(default)]
    mention_stats: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ContextInjectParams {
    session_id: String,
    input: Vec<PublicContentBlock>,
    #[serde(default)]
    as_message: bool,
    #[serde(default)]
    inject_invoked_skill: bool,
    #[serde(default)]
    client_user_message_id: Option<String>,
    #[serde(default)]
    mention_stats: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CallbackResponseParams {
    session_id: String,
    callback_id: String,
    output: Value,
}

fn content_text(input: &[PublicContentBlock]) -> String {
    input
        .iter()
        .filter_map(|block| match block {
            PublicContentBlock::Text { text } => Some(text.as_str()),
            PublicContentBlock::Image { .. } | PublicContentBlock::Resource { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

const fn default_true() -> bool {
    true
}

fn callback_output_kind(output: &Value) -> Option<EngineCallbackKind> {
    match output.get("type").and_then(Value::as_str) {
        Some("approval") => Some(EngineCallbackKind::Approval),
        Some("user_input") => Some(EngineCallbackKind::UserInput),
        _ => None,
    }
}

fn callback_output_accepted(output: &Value) -> bool {
    output
        .pointer("/decision/type")
        .and_then(Value::as_str)
        .is_none_or(|decision| !matches!(decision, "deny" | "cancel_turn"))
}

fn price_dollars_to_micros(price: f64) -> Option<u64> {
    (price.is_finite() && price >= 0.0 && price <= u64::MAX as f64 / 1_000_000.0)
        .then(|| (price * 1_000_000.0).round() as u64)
}

const fn default_history_limit() -> u16 {
    200
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("connection is not initialized")]
    NotInitialized,
    #[error("session `{0}` was not found")]
    SessionNotFound(String),
    #[error("session `{0}` already exists")]
    SessionConflict(String),
    #[error("turn `{0}` is stale")]
    StaleTurn(String),
    #[error("turn completion is not terminal: {0:?}")]
    NonTerminalCompletion(LifecycleState),
    #[error("another callback is already pending")]
    CallbackConflict,
    #[error("callback kind `{0:?}` is not supported")]
    UnsupportedCallbackKind(EngineCallbackKind),
    #[error("client does not support callback kind `{0:?}`")]
    UnsupportedClientCallbackKind(EngineCallbackKind),
    #[error("callback request did not encode as a server request")]
    InvalidCallbackRequest,
    #[error("server state lock is poisoned")]
    StatePoisoned,
    #[error(transparent)]
    Json(serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] vibe_protocol::ProtocolValidationError),
}

fn from_params<T: for<'de> Deserialize<'de>>(
    params: &BTreeMap<String, Value>,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(
        params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    ))
    .map_err(|error| error.to_string())
}

fn result_map<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn session_mut_by_id_or_alias<'a>(
    sessions: &'a mut BTreeMap<String, SessionRuntime>,
    session_id: &str,
) -> Option<&'a mut SessionRuntime> {
    if sessions.contains_key(session_id) {
        sessions.get_mut(session_id)
    } else {
        sessions
            .values_mut()
            .find(|session| session.aliases.contains(session_id))
    }
}

fn session_by_id_or_alias<'a>(
    sessions: &'a BTreeMap<String, SessionRuntime>,
    session_id: &str,
) -> Option<&'a SessionRuntime> {
    sessions.get(session_id).or_else(|| {
        sessions
            .values()
            .find(|session| session.aliases.contains(session_id))
    })
}

fn session_key_by_id_or_alias(
    sessions: &BTreeMap<String, SessionRuntime>,
    session_id: &str,
) -> Option<String> {
    if sessions.contains_key(session_id) {
        Some(session_id.to_owned())
    } else {
        sessions
            .iter()
            .find(|(_, session)| session.aliases.contains(session_id))
            .map(|(key, _)| key.clone())
    }
}

fn next_event_id(session: &mut SessionRuntime) -> u64 {
    session.event_watermark = session.event_watermark.saturating_add(1);
    session.event_watermark
}

fn attached_key(session: &SessionRuntime, attached_sessions: &BTreeSet<String>) -> Option<String> {
    attached_sessions
        .iter()
        .find(|attached| {
            attached.as_str() == session.id || session.aliases.contains(attached.as_str())
        })
        .cloned()
}

fn object(value: Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn success_batch(id: RequestId, result: BTreeMap<String, Value>) -> DispatchBatch {
    DispatchBatch {
        outbound: success_bytes(id, result).into_iter().collect(),
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

fn success_bytes(
    id: RequestId,
    result: BTreeMap<String, Value>,
) -> Result<Vec<u8>, vibe_protocol::ProtocolValidationError> {
    encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        result,
    }))
}

fn error_batch(id: RequestId, code: ProtocolErrorCode, message: &str) -> DispatchBatch {
    let frame = Envelope::Error(ErrorResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        error: ProtocolError {
            code,
            message: message.to_owned(),
            data: Value::Null,
        },
    });
    DispatchBatch {
        outbound: encode_frame(&frame).into_iter().collect(),
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

fn internal_error_batch(id: RequestId, error: &ServerError) -> DispatchBatch {
    error_batch(id, ProtocolErrorCode::InternalError, &error.to_string())
}

fn encode_notification(
    method: &str,
    params: BTreeMap<String, Value>,
) -> Result<Vec<u8>, ServerError> {
    Ok(encode_frame(&Envelope::Notification(Notification {
        jsonrpc: JsonRpcVersion::V2,
        method: method.to_owned(),
        params,
    }))?)
}

fn generated_session_id(sequence: u64) -> String {
    format!("session-{}-{sequence}", now_millis())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: i64, method: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("request fixture")
    }

    fn initialize(connection: &mut ServerConnection) {
        let batch = connection.dispatch(&request(
            1,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "test",
                    "version": "1",
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                },
                "capabilities": {
                    "callbackKinds": ["approval", "user_input"]
                }
            }),
        ));
        assert_eq!(batch.outbound.len(), 1);
        let initialized = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .expect("initialized fixture");
        assert!(connection.dispatch(&initialized).outbound.is_empty());
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    fn start_session(connection: &mut ServerConnection) {
        let batch = connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": "/workspace"}),
        ));
        assert_eq!(batch.outbound.len(), 1);
    }

    #[test]
    fn lifecycle_rejects_duplicate_initialize_and_unsolicited_responses() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let duplicate = connection.dispatch(&request(
            2,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "test",
                    "version": "1",
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                }
            }),
        ));
        let frame = decode_frame(&duplicate.outbound[0]).expect("error response");
        assert!(matches!(
            frame,
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    ..
                },
                ..
            })
        ));

        let unsolicited = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": "unknown",
            "result": {}
        }))
        .expect("response fixture");
        assert!(connection.dispatch(&unsolicited).close_after_flush);
        assert_eq!(connection.state(), ConnectionState::Closed);
    }

    #[test]
    fn turn_is_reserved_before_deferred_work_is_exposed() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);

        let batch = connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        assert_eq!(batch.outbound.len(), 1);
        assert_eq!(
            batch.deferred,
            vec![DeferredWork::RunTurn {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                prompt: "hello".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "hello".to_owned(),
                }],
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
            }]
        );
        let session = server.session("session-1").expect("reserved session");
        assert_eq!(session.active_turn.as_deref(), Some("turn-1"));
        assert_eq!(session.status, SessionStatus::Running);

        let concurrent = connection.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "second"}]}),
        ));
        let frame = decode_frame(&concurrent.outbound[0]).expect("conflict response");
        assert!(matches!(
            frame,
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::Conflict,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn stale_mutations_and_duplicate_callbacks_leave_runtime_unchanged() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let before = server.session("session-1").expect("session view");
        let stale = connection.dispatch(&request(
            4,
            "turn/steer",
            json!({
                "sessionId": "session-1",
                "expectedTurnId": "turn-stale",
                "input": [{"type": "text", "text": "wrong"}]
            }),
        ));
        assert_eq!(
            server.session("session-1").expect("unchanged session"),
            before
        );
        assert!(matches!(
            decode_frame(&stale.outbound[0]).expect("stale response"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::StaleTurn,
                    ..
                },
                ..
            })
        ));

        let (callback_id, callback_request) = connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "approve?",
            )
            .expect("callback request");
        let callback_request = decode_frame(&callback_request).expect("callback request frame");
        let callback_request_id = match &callback_request {
            Envelope::Request(request) => request.id.clone(),
            _ => RequestId::String("invalid-callback-request".to_owned()),
        };
        assert!(matches!(
            &callback_request,
            Envelope::Request(ServerRequest {
                method,
                params,
                ..
            }) if method == "callback/call"
                && params["callback"]["callbackId"].as_str() == Some(callback_id.as_str())
                && params["callback"]["detail"]["kind"] == "approval"
        ));
        let acknowledgement = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: callback_request_id,
            result: result_map([
                ("callbackId", json!(callback_id)),
                ("accepted", json!(true)),
            ]),
        }))
        .expect("callback acknowledgement");
        let acknowledgement = connection.dispatch(&acknowledgement);
        assert_eq!(acknowledgement, DispatchBatch::empty());
        let first = connection.dispatch(&request(
            5,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": callback_id,
                "output": {
                    "type": "approval",
                    "decision": {"type": "approve"}
                }
            }),
        ));
        assert_eq!(first.outbound.len(), 1);
        let after = server.session("session-1").expect("resolved session");
        let duplicate = connection.dispatch(&request(
            6,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": "callback-1",
                "output": {
                    "type": "approval",
                    "decision": {"type": "approve"}
                }
            }),
        ));
        assert_eq!(
            server.session("session-1").expect("unchanged session"),
            after
        );
        assert!(matches!(
            decode_frame(&duplicate.outbound[0]).expect("duplicate response"),
            Envelope::Success(SuccessResponse { result, .. })
                if result.get("status").and_then(Value::as_str) == Some("duplicate")
        ));
    }

    #[test]
    fn rejected_callback_delivery_cancels_the_owned_turn_once() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));

        let (callback_id, callback_request) = connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "approve?",
            )
            .expect("callback request");
        let callback_request = decode_frame(&callback_request).expect("callback request frame");
        assert!(matches!(callback_request, Envelope::Request(_)));
        let request_id = match callback_request {
            Envelope::Request(request) => request.id,
            _ => return,
        };
        let rejection = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id.clone(),
            result: result_map([
                ("callbackId", json!(callback_id)),
                ("accepted", json!(false)),
            ]),
        }))
        .expect("callback rejection");
        let batch = connection.dispatch(&rejection);
        assert_eq!(
            batch.deferred,
            vec![
                DeferredWork::ResolveCallback {
                    session_id: "session-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                    callback_id: "callback-1".to_owned(),
                    accepted: false,
                    value: Some("Client did not accept callback delivery".to_owned()),
                },
                DeferredWork::InterruptTurn {
                    session_id: "session-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                },
            ]
        );
        let session = server.session("session-1").expect("cancelled session");
        assert_eq!(session.status, SessionStatus::Cancelled);
        assert_eq!(session.pending_callback, None);

        let duplicate = connection.dispatch(&rejection);
        assert!(duplicate.close_after_flush);
        assert_eq!(connection.state(), ConnectionState::Closed);
    }

    #[test]
    fn attachment_counts_are_serialized_and_cleaned_on_close() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection
            .attach_session("session-1")
            .expect("attachment succeeds");
        connection
            .attach_session("session-1")
            .expect("duplicate attachment is idempotent");
        assert_eq!(
            server
                .session("session-1")
                .expect("attached view")
                .attachments,
            1
        );
        connection.close();
        assert_eq!(
            server
                .session("session-1")
                .expect("detached view")
                .attachments,
            0
        );
    }

    #[test]
    fn another_connection_cannot_mutate_an_unattached_session() {
        let server = AppServer::default();
        let mut owner = server.connect(TransportKind::InProcess);
        initialize(&mut owner);
        start_session(&mut owner);

        let mut intruder = server.connect(TransportKind::InProcess);
        initialize(&mut intruder);
        let read = intruder.dispatch(&request(
            3,
            "session/read",
            json!({"sessionId": "session-1"}),
        ));
        assert!(matches!(
            decode_frame(&read.outbound[0]).expect("forbidden response"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::Forbidden,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn closing_an_active_session_retains_ownership_until_terminal_cleanup() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let close = connection.dispatch(&request(
            4,
            "session/close",
            json!({"sessionId": "session-1"}),
        ));
        assert_eq!(
            close.deferred,
            vec![DeferredWork::InterruptTurn {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            }]
        );

        let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
        for envelope in [
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: None,
                emitted_at: 1,
                event_id: 1,
                event: vibe_core::events::EngineEvent::UserMessage {
                    content: "hello".to_owned(),
                },
            },
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: None,
                emitted_at: 2,
                event_id: 2,
                event: vibe_core::events::EngineEvent::Lifecycle {
                    state: LifecycleState::Cancelled,
                    message: None,
                },
            },
        ] {
            reducer.apply(&envelope).expect("terminal event applies");
        }
        server
            .complete_turn("session-1", "turn-1", reducer.state().clone())
            .expect("closed turn finalizes");
        let session = server
            .session("session-1")
            .expect("session remains addressable");
        assert_eq!(session.status, SessionStatus::Closed);
        assert!(session.active_turn.is_none());
    }

    #[test]
    fn driver_failure_releases_the_turn_reservation() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "first"}]}),
        ));
        server
            .fail_turn("session-1", "turn-1", "provider failed")
            .expect("failure finalizes");
        let retry = connection.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "retry"}]}),
        ));
        assert_eq!(retry.deferred.len(), 1);
        assert_eq!(
            server
                .session("session-1")
                .expect("retry session")
                .active_turn
                .as_deref(),
            Some("turn-2")
        );
    }

    #[test]
    fn conversation_limits_complete_with_the_public_limit_reason() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "bounded"}]}),
        ));
        let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
        for envelope in [
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 1,
                event_id: 1,
                event: vibe_core::events::EngineEvent::UserMessage {
                    content: "bounded".to_owned(),
                },
            },
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 2,
                event_id: 2,
                event: vibe_core::events::EngineEvent::Lifecycle {
                    state: LifecycleState::Completed,
                    message: Some("Token limit reached".to_owned()),
                },
            },
        ] {
            reducer.apply(&envelope).expect("limit event applies");
        }
        let notification = server
            .complete_turn_with_stop_reason(
                "session-1",
                "turn-1",
                reducer.state().clone(),
                Some(vibe_core::events::PublicTurnStopReason::Limit),
            )
            .expect("limit turn completes");
        assert!(matches!(
            decode_frame(&notification).expect("completion notification"),
            Envelope::Notification(Notification {
                method,
                params,
                ..
            }) if method == "turn/completed"
                && params["turn"]["status"] == "completed"
                && params["turn"]["stopReason"] == "limit"
        ));
    }

    #[test]
    fn provider_terminal_failures_preserve_their_public_error() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "bounded"}]}),
        ));
        let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
        for envelope in [
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 1,
                event_id: 1,
                event: vibe_core::events::EngineEvent::UserMessage {
                    content: "bounded".to_owned(),
                },
            },
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 2,
                event_id: 2,
                event: vibe_core::events::EngineEvent::Lifecycle {
                    state: LifecycleState::Failed,
                    message: Some("response too long".to_owned()),
                },
            },
        ] {
            reducer.apply(&envelope).expect("failure event applies");
        }
        let notification = server
            .complete_turn_with_details(
                "session-1",
                "turn-1",
                reducer.state().clone(),
                None,
                Some(PublicError {
                    message: "The model's response exceeded the maximum output token limit."
                        .to_owned(),
                    code: Some("response_too_long".to_owned()),
                    details: Value::Null,
                }),
            )
            .expect("failed turn completes");
        assert!(matches!(
            decode_frame(&notification).expect("completion notification"),
            Envelope::Notification(Notification {
                method,
                params,
                ..
            }) if method == "turn/completed"
                && params["turn"]["status"] == "failed"
                && params["turn"]["error"]["code"] == "response_too_long"
        ));
    }

    #[test]
    fn handoff_atomically_migrates_the_runtime_to_the_projected_id() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "compact"}]}),
        ));
        let mut reducer = vibe_core::events::ProjectionReducer::new("session-1");
        for envelope in [
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: None,
                emitted_at: 1,
                event_id: 1,
                event: vibe_core::events::EngineEvent::UserMessage {
                    content: "compact".to_owned(),
                },
            },
            vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: None,
                emitted_at: 2,
                event_id: 2,
                event: vibe_core::events::EngineEvent::SessionHandoff {
                    from_session_id: "session-1".to_owned(),
                    to_session_id: "session-2".to_owned(),
                },
            },
            vibe_core::events::EventEnvelope {
                session_id: "session-2".to_owned(),
                turn_id: None,
                emitted_at: 3,
                event_id: 3,
                event: vibe_core::events::EngineEvent::Lifecycle {
                    state: LifecycleState::Completed,
                    message: None,
                },
            },
        ] {
            reducer.apply(&envelope).expect("handoff event applies");
        }
        server
            .complete_turn("session-1", "turn-1", reducer.state().clone())
            .expect("handoff completes");
        assert!(matches!(
            server.session("session-1"),
            Err(ServerError::SessionNotFound(_))
        ));
        assert_eq!(
            server.session("session-2").expect("target runtime").id,
            "session-2"
        );
    }
}
