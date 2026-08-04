use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod batch;
mod callbacks;
mod projection;
mod registry;
mod session_management;

use batch::*;
use callbacks::*;
use projection::*;
use registry::SessionRegistry;

use crate::host::now_millis;
use crate::release3::{RELEASE3_METHODS, Release3Error, Release3Service, RuntimeAttachment};
use crate::release4::{
    LoopFire, RELEASE4_METHODS, Release4Dispatch, Release4Error, Release4Service,
};
use crate::resources::{
    BACKEND_RESOURCE_METHODS, CoreResourceBackend, RESOURCE_METHODS, ResourceBackend,
    ResourceBackendCommand, ResourceBackendRequest, ResourceDispatch, ResourceError,
    ResourceService, ResourceSession,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::events::{
    CallbackKind as EngineCallbackKind, LifecycleState, ModelMessage, ProjectionSnapshot,
    PublicCallbackState, PublicContentBlock, PublicEffectState, PublicEntryGenerationStatus,
    PublicEntryMetadata, PublicError, PublicHistoryEntry, PublicMessageRole, PublicMessageSource,
    PublicTurn, PublicTurnStatus,
};
use vibe_core::extensions::{AgentApproval, AgentProfile};
use vibe_core::integrations::redact;
use vibe_core::mcp::McpServerConfig;
pub use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionRequirement,
    PolicyError,
};
use vibe_core::policy::{PermissionRule, PermissionStore, TrustDecision, TrustRootKind};
use vibe_core::storage::HydratedSession;
pub use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolInvocation,
    ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};
use vibe_protocol::{
    CallbackKind, ClientCapabilities, Envelope, ErrorResponse, InitializeParams,
    InitializeResponse, JsonRpcVersion, Notification, ProtocolError, ProtocolErrorCode,
    ProtocolVersion, RequestId, ServerCapabilities, ServerInfo, ServerRequest, SuccessResponse,
    TransportKind, decode_frame, encode_frame, is_server_method,
};

const INITIALIZE_METHOD: &str = "initialize";
const INITIALIZED_NOTIFICATION: &str = "initialized";
const SHUTDOWN_METHOD: &str = "shutdown";
const EXIT_NOTIFICATION: &str = "exit";
const MAX_CALLBACK_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_ANSWERS: usize = 16;
const MAX_CALLBACK_OPTIONS: usize = 32;
const MAX_CALLBACK_TEXT_BYTES: usize = 8 * 1024;
const IMPLEMENTED_METHODS: &[&str] = &[
    "account/read",
    "callback/respond",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "connectors/toggle",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "mcp/add",
    "mcp/auth/complete",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "narration/summarize",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/state",
    "review/turnDiff",
    "runtime/read",
    "session/close",
    "session/compact/start",
    "session/context/inject",
    "session/read",
    "session/ready/read",
    "session/ready/wait",
    "session/settings/update",
    "session/start",
    "shell/interrupt",
    "shell/run",
    "stats/read",
    "tools/list",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
    "workspace/trust/decision",
    "workspace/trust/status",
];

struct DenyApproval;

impl ApprovalAgent for DenyApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

struct ApproveOnce;

impl ApprovalAgent for ApproveOnce {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

pub trait ApprovalAgentFactory: Send + Sync {
    fn for_session(&self, session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent>;

    fn for_agent(
        &self,
        session_id: &str,
        approval: AgentApproval,
        auto_approve: bool,
    ) -> Arc<dyn ApprovalAgent> {
        let fallback = self.for_session(session_id, auto_approve || approval == AgentApproval::All);
        match approval {
            AgentApproval::Edits => Arc::new(ApproveEdits { fallback }),
            AgentApproval::Prompt | AgentApproval::All => fallback,
        }
    }
}

struct ApproveEdits {
    fallback: Arc<dyn ApprovalAgent>,
}

impl ApprovalAgent for ApproveEdits {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        if matches!(request.tool.as_str(), "edit" | "write" | "write_file") {
            Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
        } else {
            self.fallback.request(request)
        }
    }
}

pub trait SessionToolFactory: Send + Sync {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String>;
}

struct NoAdditionalTools;

impl SessionToolFactory for NoAdditionalTools {
    fn register(&self, _session_id: &str, _tools: &ToolRegistry) -> Result<(), String> {
        Ok(())
    }
}

struct ChainedSessionToolFactory {
    existing: Arc<dyn SessionToolFactory>,
    additional: Arc<dyn SessionToolFactory>,
}

impl SessionToolFactory for ChainedSessionToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        self.existing.register(session_id, tools)?;
        self.additional.register(session_id, tools)
    }
}

struct DefaultApprovalFactory;

impl ApprovalAgentFactory for DefaultApprovalFactory {
    fn for_session(&self, _session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveOnce)
        } else {
            Arc::new(DenyApproval)
        }
    }
}

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
    ResourceRequest {
        request_id: RequestId,
        session_id: String,
        command: ResourceBackendCommand,
    },
    CloudRequest {
        request_id: RequestId,
        method: String,
        params: BTreeMap<String, Value>,
    },
    ConfigureMcp {
        session_id: String,
        configs: Vec<McpServerConfig>,
    },
    CompactSession {
        request_id: RequestId,
        session_id: String,
        extra_instructions: String,
    },
    CloseResources {
        session_id: String,
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBatch {
    pub outbound: Vec<Vec<u8>>,
    pub deferred: Vec<DeferredWork>,
    pub close_after_flush: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledLoopWork {
    pub fire: LoopFire,
    pub work: DeferredWork,
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

#[derive(Clone)]
pub struct AppServer {
    sessions: Arc<Mutex<SessionRegistry>>,
    resources: Arc<Mutex<ResourceService>>,
    resource_backend: Option<Arc<dyn ResourceBackend>>,
    release3: Arc<Release3Service>,
    release4: Arc<Release4Service>,
    approval_factory: Arc<dyn ApprovalAgentFactory>,
    session_tool_factory: Arc<dyn SessionToolFactory>,
    next_session: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
    next_callback: Arc<AtomicU64>,
    next_entry: Arc<AtomicU64>,
}

impl Default for AppServer {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(SessionRegistry::default())),
            resources: Arc::new(Mutex::new(ResourceService::default())),
            resource_backend: Some(Arc::new(CoreResourceBackend::default())),
            release3: Arc::new(Release3Service::default()),
            release4: Arc::new(Release4Service::default()),
            approval_factory: Arc::new(DefaultApprovalFactory),
            session_tool_factory: Arc::new(NoAdditionalTools),
            next_session: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            next_callback: Arc::new(AtomicU64::new(1)),
            next_entry: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl AppServer {
    #[must_use]
    pub fn with_resource_backend(backend: Arc<dyn ResourceBackend>) -> Self {
        Self {
            resource_backend: Some(backend),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_release3_service(service: Release3Service) -> Self {
        Self {
            release3: Arc::new(service),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_release4_service(service: Release4Service) -> Self {
        Self {
            release4: Arc::new(service),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn using_release3_service(mut self, service: Release3Service) -> Self {
        self.release3 = Arc::new(service);
        self
    }

    #[must_use]
    pub fn using_release4_service(mut self, service: Release4Service) -> Self {
        self.release4 = Arc::new(service);
        self
    }

    #[must_use]
    pub fn using_session_tool_factory(
        mut self,
        session_tool_factory: Arc<dyn SessionToolFactory>,
    ) -> Self {
        self.session_tool_factory = Arc::new(ChainedSessionToolFactory {
            existing: self.session_tool_factory,
            additional: session_tool_factory,
        });
        self
    }

    #[must_use]
    pub fn using_surface_extension(
        mut self,
        approval_factory: Arc<dyn ApprovalAgentFactory>,
        session_tool_factory: Arc<dyn SessionToolFactory>,
    ) -> Self {
        self.approval_factory = approval_factory;
        self.session_tool_factory = Arc::new(ChainedSessionToolFactory {
            existing: self.session_tool_factory,
            additional: session_tool_factory,
        });
        self
    }

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
        let session = sessions.get_mut(session_id)
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

    pub fn reserve_due_loop(
        &self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledLoopWork>, ServerError> {
        let (canonical_session_id, idle) = {
            let sessions = self.lock_sessions()?;
            let session = sessions.get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            (
                session.id.clone(),
                session.active_turn.is_none()
                    && !session.compaction_pending
                    && session.status != SessionStatus::Closed,
            )
        };
        if !idle {
            return Ok(None);
        }
        let Some(loop_id) = self
            .release4
            .next_due_loop_id(&canonical_session_id, now_seconds)
            .map_err(|error| ServerError::Release4(error.to_string()))?
        else {
            return Ok(None);
        };
        let mut fire = self
            .release4
            .fire_loop_for_session(&loop_id, &canonical_session_id, now_seconds, true)
            .map_err(|error| ServerError::Release4(error.to_string()))?;
        let mut sessions = self.lock_sessions()?;
        let session = sessions.get_mut(&canonical_session_id)
            .ok_or_else(|| ServerError::SessionNotFound(canonical_session_id.clone()))?;
        if session.active_turn.is_some()
            || session.compaction_pending
            || session.status == SessionStatus::Closed
        {
            self.release4
                .finish_loop_fire(&loop_id, now_seconds)
                .map_err(|error| ServerError::Release4(error.to_string()))?;
            return Ok(None);
        }
        let turn_sequence = self.next_turn.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!("turn-{turn_sequence}");
        if let Some(review) = &session.review {
            let message_index = review_message_index(&self.release3, session)?;
            review
                .begin_turn_at(&turn_id, message_index)
                .map_err(|error| ServerError::Resource(error.to_string()))?;
        }
        let started_at = now_millis();
        session.active_turn = Some(turn_id.clone());
        session.active_turn_started_at = Some(started_at);
        session.active_scheduled_loop = Some(loop_id.clone());
        session.status = SessionStatus::Running;
        session.latest_turn = Some(PublicTurn {
            id: turn_id.clone(),
            session_id: canonical_session_id.clone(),
            status: PublicTurnStatus::InProgress,
            started_at,
            completed_at: None,
            error: None,
            stop_reason: None,
        });
        session.updated_at = started_at;
        let event_id = next_event_id(session);
        fire.notice
            .params
            .insert("eventId".to_owned(), json!(event_id));
        fire.notice
            .params
            .insert("sessionId".to_owned(), json!(canonical_session_id.clone()));
        fire.notice
            .params
            .insert("turnId".to_owned(), json!(turn_id.clone()));
        fire.notice.params.insert(
            "emittedAt".to_owned(),
            json!(now_seconds.saturating_mul(1_000)),
        );
        if let Some(entry) = fire.notice.params.get_mut("entry") {
            entry["id"] = json!(format!("scheduled-loop:{turn_id}"));
            entry["sessionId"] = json!(canonical_session_id.clone());
            entry["turnId"] = json!(turn_id.clone());
        }
        let prompt = fire.prompt.clone();
        Ok(Some(ScheduledLoopWork {
            fire,
            work: DeferredWork::RunTurn {
                session_id: canonical_session_id,
                turn_id,
                prompt: prompt.clone(),
                input: vec![PublicContentBlock::Text { text: prompt }],
                client_user_message_id: None,
                auto_title: None,
                user_display_content: Some(json!({
                    "kind": "scheduled_loop",
                    "loopId": loop_id,
                    "firedAt": now_seconds,
                })),
                mention_stats: None,
            },
        }))
    }

    pub fn finish_scheduled_loop(
        &self,
        loop_id: &str,
        completed_at_seconds: u64,
    ) -> Result<(), ServerError> {
        match self
            .release4
            .finish_loop_fire(loop_id, completed_at_seconds)
        {
            Ok(()) | Err(Release4Error::Conflict(_)) => Ok(()),
            Err(error) => Err(ServerError::Release4(error.to_string())),
        }
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
        let source_key = sessions
            .key(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
            .to_owned();
        let (was_closed, started_at, active_scheduled_loop, review) = {
            let session = sessions
                .get(&source_key)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            if session.active_turn.as_deref() != Some(turn_id) {
                return Err(ServerError::StaleTurn(turn_id.to_owned()));
            }
            (
                session.status == SessionStatus::Closed,
                session.active_turn_started_at.unwrap_or_default(),
                session.active_scheduled_loop.clone(),
                session.review.clone(),
            )
        };
        if !matches!(
            snapshot.lifecycle,
            LifecycleState::Completed | LifecycleState::Cancelled | LifecycleState::Failed
        ) {
            return Err(ServerError::NonTerminalCompletion(snapshot.lifecycle));
        }
        let target_session_id = snapshot.session_id.clone();
        if sessions
            .key(&target_session_id)
            .is_some_and(|existing| existing != source_key)
        {
            return Err(ServerError::SessionConflict(target_session_id));
        }
        let status = match snapshot.lifecycle {
            LifecycleState::Completed => PublicTurnStatus::Completed,
            LifecycleState::Cancelled => PublicTurnStatus::Interrupted,
            LifecycleState::Failed => PublicTurnStatus::Failed,
            _ => return Err(ServerError::NonTerminalCompletion(snapshot.lifecycle)),
        };
        let completed_at = now_millis();
        if let Some(loop_id) = &active_scheduled_loop {
            self.release4
                .finish_loop_fire(loop_id, completed_at / 1_000)
                .map_err(|error| ServerError::Release4(error.to_string()))?;
        }
        if let Some(review) = &review {
            review
                .seal_turn()
                .map_err(|error| ServerError::Resource(error.to_string()))?;
        }
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
        sessions.alias(&source_key, session_id);
        sessions.rename(&source_key, &target_session_id)?;
        let session = sessions
            .get_mut(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        cancel_pending_callback(session, "Turn completed before callback was answered");
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.active_scheduled_loop = None;
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
        session.snapshot = Some(merge_server_callback_history(
            session.snapshot.as_ref(),
            snapshot.clone(),
        ));
        session.latest_turn = Some(turn.clone());
        session.updated_at = completed_at;
        let event_id = next_event_id(session);
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
        let session = sessions.get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let was_closed = session.status == SessionStatus::Closed;
        let started_at = session.active_turn_started_at.unwrap_or_default();
        if let Some(loop_id) = &session.active_scheduled_loop {
            self.release4
                .finish_loop_fire(loop_id, now_millis() / 1_000)
                .map_err(|error| ServerError::Release4(error.to_string()))?;
        }
        if let Some(review) = &session.review {
            review
                .seal_turn()
                .map_err(|error| ServerError::Resource(error.to_string()))?;
        }
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.active_scheduled_loop = None;
        cancel_pending_callback(session, message);
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
        self.request_callback_with_detail(session_id, turn_id, kind, prompt, detail)
    }

    pub fn request_callback_with_detail(
        &self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        title: impl Into<String>,
        detail: Value,
    ) -> Result<(String, Vec<u8>), ServerError> {
        if kind == EngineCallbackKind::ConnectorAuth {
            return Err(ServerError::UnsupportedCallbackKind(kind));
        }
        let title = title.into();
        validate_callback_request(kind, &title, &detail)
            .map_err(|message| ServerError::InvalidCallbackDetail(message.to_owned()))?;
        let related_entry_id = detail
            .get("relatedEntryId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut sessions = self.lock_sessions()?;
        let session = sessions.get_mut(session_id)
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
        let callback = PublicHistoryEntry::Callback {
            metadata: PublicEntryMetadata {
                id: format!("callback:{callback_id}"),
                session_id: session.id.clone(),
                turn_id: Some(turn_id.to_owned()),
                created_at: timestamp,
                updated_at: timestamp,
                generation_status: PublicEntryGenerationStatus::InProgress,
                related_entry_id,
            },
            callback_id: callback_id.clone(),
            title,
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
        let snapshot = session.snapshot.get_or_insert_with(|| ProjectionSnapshot {
            session_id: session.id.clone(),
            turn_id: Some(turn_id.to_owned()),
            watermark: 0,
            lifecycle: LifecycleState::Running,
            title: None,
            history: Vec::new(),
        });
        if !snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    callback_id: existing,
                    ..
                } if existing == &callback_id
            )
        }) {
            snapshot.history.push(callback.clone());
        }
        next_event_id(session);
        let request = encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: RequestId::Integer(i64::try_from(callback_sequence).unwrap_or(i64::MAX)),
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback))]),
        }))?;
        Ok((callback_id, request))
    }

    pub fn live_projection_seed(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<ProjectionSnapshot, ServerError> {
        let sessions = self.lock_sessions()?;
        let session = sessions.get(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        let mut snapshot = session
            .snapshot
            .clone()
            .unwrap_or_else(|| ProjectionSnapshot {
                session_id: session.id.clone(),
                turn_id: Some(turn_id.to_owned()),
                watermark: 0,
                lifecycle: LifecycleState::Idle,
                title: None,
                history: Vec::new(),
            });
        snapshot.session_id.clone_from(&session.id);
        snapshot.turn_id = Some(turn_id.to_owned());
        snapshot.watermark = 0;
        snapshot.lifecycle = LifecycleState::Idle;
        Ok(snapshot)
    }

    pub fn apply_live_projection(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
    ) -> Result<u64, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions.get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        if snapshot.session_id != session.id || snapshot.turn_id.as_deref() != Some(turn_id) {
            return Err(ServerError::SessionConflict(snapshot.session_id));
        }
        let event_id = next_event_id(session);
        session.snapshot = Some(merge_server_callback_history(
            session.snapshot.as_ref(),
            snapshot,
        ));
        session.updated_at = now_millis();
        Ok(event_id)
    }

    pub(crate) fn handoff_active_turn(
        &self,
        old_session_id: &str,
        new_session_id: &str,
        turn_id: &str,
        mut snapshot: ProjectionSnapshot,
        summary_length: usize,
        emitted_at: u64,
    ) -> Result<Vec<u8>, ServerError> {
        if snapshot.session_id != new_session_id {
            return Err(ServerError::SessionConflict(new_session_id.to_owned()));
        }
        let mut sessions = self.lock_sessions()?;
        let source_key = sessions
            .key(old_session_id)
            .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?
            .to_owned();
        let previous_id = {
            let session = sessions
                .get(&source_key)
                .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?;
            if session.active_turn.as_deref() != Some(turn_id) {
                return Err(ServerError::StaleTurn(turn_id.to_owned()));
            }
            session.id.clone()
        };
        sessions.rename(&source_key, new_session_id)?;
        self.release4
            .rebind_session(&previous_id, new_session_id)
            .map_err(|error| ServerError::Release4(error.to_string()))?;
        for entry in &mut snapshot.history {
            entry.rebind_session(new_session_id);
        }
        sessions.alias(&source_key, old_session_id);
        let session = sessions
            .get_mut(&source_key)
            .ok_or_else(|| ServerError::SessionNotFound(old_session_id.to_owned()))?;
        session.intent.resume = Some(new_session_id.to_owned());
        session.snapshot = Some(snapshot);
        session.updated_at = now_millis();
        session.event_watermark = 0;
        if let Some(turn) = session.latest_turn.as_mut() {
            turn.session_id = new_session_id.to_owned();
        }
        if let Some(callback) = session.pending_callback.as_mut() {
            callback.entry.rebind_session(new_session_id);
        }
        let event_id = next_event_id(session);
        let state = public_session_state(session);
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

    pub fn complete_manual_compaction(
        &self,
        request_id: RequestId,
        old_session_id: &str,
        new_session_id: &str,
        summary: &str,
        hydrated: HydratedSession,
    ) -> DispatchBatch {
        let (state, event_id, emitted_at) = {
            let mut sessions = match self.lock_sessions() {
                Ok(sessions) => sessions,
                Err(error) => return internal_error_batch(request_id, &error),
            };
            let Some(source_key) = sessions.key(old_session_id).map(ToOwned::to_owned) else {
                return error_batch(
                    request_id,
                    ProtocolErrorCode::NotFound,
                    "Session was not found",
                );
            };
            let clear_reservation = |sessions: &mut SessionRegistry| {
                if let Some(session) = sessions.get_mut(&source_key) {
                    session.compaction_pending = false;
                }
            };
            if sessions
                .key(new_session_id)
                .is_some_and(|existing| existing != source_key)
            {
                clear_reservation(&mut sessions);
                return error_batch(
                    request_id,
                    ProtocolErrorCode::Conflict,
                    "Compaction target session already exists",
                );
            }
            let previous_id = {
                let Some(session) = sessions.get(&source_key) else {
                    return error_batch(
                        request_id,
                        ProtocolErrorCode::NotFound,
                        "Session was not found",
                    );
                };
                if !session.compaction_pending || session.active_turn.is_some() {
                    return error_batch(
                        request_id,
                        ProtocolErrorCode::Conflict,
                        "Compaction reservation is stale",
                    );
                }
                session.id.clone()
            };
            if hydrated.metadata.id != new_session_id
                || hydrated.metadata.parent_session_id.as_deref() != Some(old_session_id)
            {
                clear_reservation(&mut sessions);
                return error_batch(
                    request_id,
                    ProtocolErrorCode::CompactionFailed,
                    "Compaction produced an invalid session handoff",
                );
            }
            if let Err(error) = self.release4.rebind_session(&previous_id, new_session_id) {
                clear_reservation(&mut sessions);
                return internal_error_batch(request_id, &ServerError::Release4(error.to_string()));
            }
            if let Err(error) = sessions.rename(&source_key, new_session_id) {
                clear_reservation(&mut sessions);
                return internal_error_batch(request_id, &error);
            }
            sessions.alias(&source_key, old_session_id);
            let Some(session) = sessions.get_mut(&source_key) else {
                return error_batch(
                    request_id,
                    ProtocolErrorCode::NotFound,
                    "Session was not found",
                );
            };
            session.intent.resume = Some(new_session_id.to_owned());
            session.status = SessionStatus::Idle;
            session.compaction_pending = false;
            session.persisted = Some(hydrated);
            session.updated_at = now_millis();
            session.event_watermark = 0;
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.session_id = new_session_id.to_owned();
                snapshot.turn_id = None;
                snapshot.watermark = 0;
                snapshot.lifecycle = LifecycleState::Idle;
                for entry in &mut snapshot.history {
                    entry.rebind_session(new_session_id);
                }
            }
            if let Some(callback) = session.pending_callback.as_mut() {
                callback.entry.rebind_session(new_session_id);
            }
            if let Some(turn) = session.latest_turn.as_mut() {
                turn.session_id = new_session_id.to_owned();
            }
            let event_id = next_event_id(session);
            let state = public_session_state(session);
            (state, event_id, now_millis())
        };
        let result = result_map([
            ("summary", json!(summary)),
            ("state", state.clone()),
            ("sessionLog", json!({"enabled": false})),
        ]);
        let notification = match encode_notification(
            "session/compacted",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(new_session_id)),
                ("oldSessionId", json!(old_session_id)),
                ("state", state),
                ("sessionLog", json!({"enabled": false})),
                ("summaryLength", json!(summary.chars().count())),
                ("emittedAt", json!(emitted_at)),
            ]),
        ) {
            Ok(notification) => notification,
            Err(error) => return internal_error_batch(request_id, &error),
        };
        DispatchBatch {
            outbound: success_bytes(request_id, result)
                .into_iter()
                .chain([notification])
                .collect(),
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }

    pub fn fail_manual_compaction(
        &self,
        request_id: RequestId,
        session_id: &str,
        reason: &str,
    ) -> DispatchBatch {
        if let Ok(mut sessions) = self.lock_sessions()
            && let Some(session) = sessions.get_mut(session_id)
        {
            session.compaction_pending = false;
            session.updated_at = now_millis();
        }
        error_batch(request_id, ProtocolErrorCode::CompactionFailed, reason)
    }

    fn reject_callback(
        &self,
        route: &CallbackRoute,
        reason: &str,
    ) -> Result<Vec<DeferredWork>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions.get_mut(&route.session_id)
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
        settle_pending_callback(
            session,
            &route.callback_id,
            PublicCallbackState::Cancelled {
                reason: reason.to_owned(),
            },
        );
        session.pending_callback = None;
        session.status = SessionStatus::Cancelled;
        session.updated_at = now_millis();
        next_event_id(session);
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
        sessions.get(session_id)
            .map(SessionView::from)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))
    }

    pub(crate) fn tool_registry(&self, session_id: &str) -> Result<ToolRegistry, ServerError> {
        let sessions = self.lock_sessions()?;
        sessions.get(session_id)
            .map(|session| session.tools.clone())
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))
    }

    pub async fn invoke_tool(
        &self,
        session_id: &str,
        name: &str,
        invocation: ToolInvocation,
    ) -> Result<ToolExecutionOutput, ServerError> {
        let tools = {
            let sessions = self.lock_sessions()?;
            sessions.get(session_id)
                .map(|session| session.tools.clone())
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
        };
        tools
            .invoke(name, invocation)
            .await
            .map_err(|error| ServerError::Tool(error.to_string()))
    }

    pub async fn execute_resource_request(
        &self,
        request_id: RequestId,
        session_id: String,
        command: ResourceBackendCommand,
    ) -> DispatchBatch {
        let Some(backend) = &self.resource_backend else {
            return error_batch(
                request_id,
                ProtocolErrorCode::Conflict,
                "Operational resource backend is not attached",
            );
        };
        let request = ResourceBackendRequest {
            session_id,
            command,
        };
        resource_result_batch(request_id, backend.dispatch(request).await)
    }

    pub async fn execute_cloud_request(
        &self,
        request_id: RequestId,
        method: String,
        params: BTreeMap<String, Value>,
    ) -> DispatchBatch {
        match self.release4.dispatch_deferred(&method, &params).await {
            Ok(dispatch) => release4_dispatch_batch(request_id, dispatch),
            Err(error) => release4_error_batch(request_id, error),
        }
    }

    pub async fn close_resource_session(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), ServerError> {
        match &self.resource_backend {
            Some(backend) => backend
                .close_session(session_id, generation)
                .await
                .map_err(|error| ServerError::Resource(error.to_string())),
            None => Ok(()),
        }
    }

    pub async fn configure_mcp_servers(
        &self,
        session_id: &str,
        configs: Vec<McpServerConfig>,
    ) -> Result<Vec<u8>, ServerError> {
        let Some(backend) = &self.resource_backend else {
            return encode_notification(
                "mcp/updated",
                result_map([
                    ("mcp", json!({"sources": []})),
                    (
                        "diagnostics",
                        json!(["MCP transport backend is not configured"]),
                    ),
                ]),
            );
        };
        match backend.configure_mcp(session_id, configs).await {
            Ok(dispatch) => match dispatch.notification {
                Some(notification) => {
                    encode_notification(&notification.method, notification.params)
                }
                None => encode_notification(
                    "mcp/updated",
                    result_map([("mcp", json!({"sources": []})), ("diagnostics", json!([]))]),
                ),
            },
            Err(error) => encode_notification(
                "mcp/updated",
                result_map([
                    ("mcp", json!({"sources": []})),
                    ("diagnostics", json!([redact(&error.to_string())])),
                ]),
            ),
        }
    }

    pub(crate) fn orphaned_resource_generation(
        &self,
        session_id: &str,
    ) -> Result<Option<u64>, ServerError> {
        let sessions = self.lock_sessions()?;
        Ok(sessions.get(session_id)
            .filter(|session| session.attachments == 0)
            .map(|session| session.resource_generation))
    }

    fn lock_sessions(&self) -> Result<std::sync::MutexGuard<'_, SessionRegistry>, ServerError> {
        self.sessions.lock().map_err(|_| ServerError::StatePoisoned)
    }

    fn attach_release3_runtime(
        &self,
        attachment: &RuntimeAttachment,
        review_override: Option<Arc<ReviewManager>>,
    ) -> Result<(), ServerError> {
        let mut sessions = self.lock_sessions()?;
        if let Some(session) = sessions.get_mut(&attachment.id) {
            session.attachments = session.attachments.saturating_add(1);
            session.persisted = Some(attachment.hydrated.clone());
            recompose_agent_profile_settings(
                &mut session.intent,
                &attachment.hydrated,
                attachment.agent_profile.as_ref(),
            );
            if review_override.is_some() {
                session.review = review_override;
            }
            session.updated_at = now_millis();
            let session_id = session.id.clone();
            drop(sessions);
            return self.refresh_session_workspace_tools(&session_id);
        }
        let policy = PermissionStore::default();
        let tools = ToolRegistry::default();
        let mut intent = SessionIntent {
            agent: attachment.agent.clone(),
            resume: Some(attachment.id.clone()),
            ..SessionIntent::default()
        };
        apply_persisted_session_settings(&mut intent, &attachment.hydrated);
        if let Some(profile) = &attachment.agent_profile {
            apply_agent_profile_settings(&mut intent, profile);
        }
        policy
            .try_replace_rules_with_rationale_prefix(
                "agent-profile:",
                intent.agent_permission_rules.clone(),
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let review = self.register_workspace_tools(
            &attachment.id,
            &attachment.working_directory,
            &policy,
            &tools,
            &intent,
            review_override,
        )?;
        self.session_tool_factory
            .register(&attachment.id, &tools)
            .map_err(ServerError::Resource)?;
        let mut session = SessionRuntime::new(
            attachment.id.clone(),
            attachment.working_directory.clone(),
            intent,
            policy.clone(),
            tools.clone(),
            review,
            now_millis(),
        );
        session.persisted = Some(attachment.hydrated.clone());
        sessions.insert(session);
        self.open_session_resources(
            &mut sessions,
            ResourceSession {
                session_id: attachment.id.clone(),
                generation: 1,
                working_directory: attachment.working_directory.clone(),
                project_trusted: matches!(
                    policy.try_trust_decision(&attachment.working_directory),
                    Ok(Some(TrustDecision::Trusted | TrustDecision::SessionTrusted))
                ),
                policy,
                tools,
            },
        )
        .map_err(|error| ServerError::Resource(error.to_string()))
    }

    /// Registers the workspace tool surface for a session root.
    ///
    /// Returns the review manager that owns the session's file checkpoints, or
    /// `None` when the root is not a usable workspace.
    fn register_workspace_tools(
        &self,
        session_id: &str,
        working_directory: &str,
        policy: &PermissionStore,
        tools: &ToolRegistry,
        intent: &SessionIntent,
        review: Option<Arc<ReviewManager>>,
    ) -> Result<Option<Arc<ReviewManager>>, ServerError> {
        let Ok(workspace) = Workspace::open(working_directory) else {
            return Ok(None);
        };
        let workspace = Arc::new(workspace);
        let review = review.unwrap_or_else(|| Arc::new(ReviewManager::new(workspace.clone())));
        WorkspaceTools::new(workspace, review.clone())
            .register(
                tools,
                policy.clone(),
                self.approval_factory
                    .for_agent(session_id, intent.approval, intent.auto_approve),
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        Ok(Some(review))
    }

    /// Opens the in-memory and operational resource state for a session that is
    /// already in the map, removing it again if either backend refuses.
    fn open_session_resources(
        &self,
        sessions: &mut SessionRegistry,
        session: ResourceSession,
    ) -> Result<(), ResourceError> {
        let session_id = session.session_id.clone();
        let opened = self
            .resources
            .lock()
            .map_err(|_| ResourceError::Unavailable("resource state lock is poisoned".to_owned()))
            .and_then(|mut resources| {
                resources.open_session(&session_id, session.policy.clone(), session.tools.clone())
            });
        if let Err(error) = opened {
            sessions.remove(&session_id);
            return Err(error);
        }
        if let Some(backend) = &self.resource_backend
            && let Err(error) = backend.open_session(session)
        {
            if let Ok(mut resources) = self.resources.lock() {
                resources.close_session(&session_id);
            }
            sessions.remove(&session_id);
            return Err(error);
        }
        Ok(())
    }

    fn refresh_release3_runtime(
        &self,
        attachment: &RuntimeAttachment,
        review_override: Option<Arc<ReviewManager>>,
    ) -> Result<(), ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(&attachment.id)
            .ok_or_else(|| ServerError::SessionNotFound(attachment.id.clone()))?;
        let previous = session.clone();
        session.persisted = Some(attachment.hydrated.clone());
        session.intent.agent = attachment.agent.clone();
        recompose_agent_profile_settings(
            &mut session.intent,
            &attachment.hydrated,
            attachment.agent_profile.as_ref(),
        );
        if review_override.is_some() {
            session.review = review_override;
        }
        session.updated_at = now_millis();
        drop(sessions);
        if let Err(error) = self.refresh_session_workspace_tools(&attachment.id) {
            self.lock_sessions()?.insert(previous);
            if let Err(rollback) = self.refresh_session_workspace_tools(&attachment.id) {
                return Err(ServerError::Resource(format!(
                    "{error}; runtime rollback failed ({rollback})"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    fn refresh_session_workspace_tools(&self, session_id: &str) -> Result<(), ServerError> {
        let (working_directory, policy, tools, intent, previous_review) = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            (
                session.working_directory.clone(),
                session.policy.clone(),
                session.tools.clone(),
                session.intent.clone(),
                session.review.clone(),
            )
        };
        policy
            .try_replace_rules_with_rationale_prefix(
                "agent-profile:",
                intent.agent_permission_rules.clone(),
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let review = self
            .register_workspace_tools(
                session_id,
                &working_directory,
                &policy,
                &tools,
                &intent,
                previous_review.clone(),
            )?
            .or(previous_review);
        if let Some(session) = self.lock_sessions()?.get_mut(session_id) {
            session.review = review;
        }
        Ok(())
    }
}

fn apply_persisted_session_settings(intent: &mut SessionIntent, hydrated: &HydratedSession) {
    let config = &hydrated.metadata.config;
    if let Some(value) = config.get("active_model").and_then(Value::as_str) {
        intent.model = Some(value.to_owned());
    }
    if let Some(value) = config.get("maxTurns").and_then(Value::as_u64)
        && let Ok(value) = u32::try_from(value)
    {
        intent.max_turns = Some(value);
    }
    if let Some(value) = config.get("maxTokens").and_then(Value::as_u64) {
        intent.max_tokens = Some(value);
    }
    if let Some(value) = config.get("mode").and_then(Value::as_str)
        && matches!(value, "code" | "plan")
    {
        intent.mode = Some(value.to_owned());
    }
    if let Some(value) = config.get("thinking").and_then(Value::as_bool) {
        intent.thinking = value;
    }
    if let Some(value) = config.get("reasoningEffort").and_then(Value::as_str)
        && matches!(value, "low" | "medium" | "high" | "max")
    {
        intent.reasoning_effort = Some(value.to_owned());
    }
    if let Some(value) = config.get("autoApprove").and_then(Value::as_bool) {
        intent.auto_approve = value;
    }
}

fn apply_agent_profile_settings(intent: &mut SessionIntent, profile: &AgentProfile) {
    let settings = profile.runtime_settings();
    intent.enabled_tools = if settings.enabled_tools.is_empty() {
        intent.requested_enabled_tools.clone()
    } else {
        settings.enabled_tools
    };
    intent
        .disabled_tools
        .clone_from(&intent.requested_disabled_tools);
    intent.disabled_tools.extend(settings.disabled_tools);
    intent.disabled_tools.sort();
    intent.disabled_tools.dedup();
    intent.agent_permission_rules = settings.permission_rules;
    intent.approval = settings.approval;
    intent.auto_approve = intent.requested_auto_approve || settings.approval == AgentApproval::All;
    intent.system_prompt_id = settings.system_prompt_id;
    if let Some(model) = settings.model {
        intent.model = Some(model);
    }
    if let Some(thinking) = settings.thinking {
        intent.thinking = thinking;
    }
    if settings.reasoning_effort.is_some() {
        intent.reasoning_effort = settings.reasoning_effort;
    }
    if settings.mode.is_some() {
        intent.mode = settings.mode;
    }
}

fn recompose_agent_profile_settings(
    intent: &mut SessionIntent,
    hydrated: &HydratedSession,
    profile: Option<&AgentProfile>,
) {
    intent
        .enabled_tools
        .clone_from(&intent.requested_enabled_tools);
    intent
        .disabled_tools
        .clone_from(&intent.requested_disabled_tools);
    intent.model = None;
    intent.mode = None;
    intent.thinking = false;
    intent.reasoning_effort = None;
    intent.auto_approve = intent.requested_auto_approve;
    intent.approval = AgentApproval::Prompt;
    intent.agent_permission_rules.clear();
    intent.system_prompt_id = None;
    apply_persisted_session_settings(intent, hydrated);
    if let Some(profile) = profile {
        apply_agent_profile_settings(intent, profile);
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
    ) -> Result<(String, Vec<u8>), ServerError> {
        let title = title.into();
        self.deliver_callback(session_id, turn_id, kind, |server| {
            server.request_callback_with_detail(session_id, turn_id, kind, title, detail)
        })
    }

    /// Mints a callback through `request` and routes the client's answer back
    /// to the turn that is waiting for it.
    fn deliver_callback(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        request: impl FnOnce(&AppServer) -> Result<(String, Vec<u8>), ServerError>,
    ) -> Result<(String, Vec<u8>), ServerError> {
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
        let (callback_id, bytes) = request(&self.server)?;
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
                answered: false,
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
        if accepted || route.answered {
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
        if route.answered {
            return DispatchBatch::empty();
        }
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
        if let Ok(mut sessions) = self.server.lock_sessions() {
            for session_id in &self.attached_sessions {
                if let Some(session) = sessions.get_mut(session_id) {
                    session.attachments = session.attachments.saturating_sub(1);
                }
            }
        }
        self.attached_sessions.clear();
        self.pending_server_requests.clear();
        self.state = ConnectionState::Closed;
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
        if !is_server_method(&request.method) {
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
            "session/compact/start" => self.session_compact_start(request),
            "session/settings/update" => self.session_settings_update(request),
            "turn/start" => self.turn_start(request),
            "turn/steer" => self.turn_steer(request),
            "turn/interrupt" => self.turn_interrupt(request),
            "session/context/inject" => self.context_inject(request),
            "callback/respond" => self.callback_respond(request),
            method if RESOURCE_METHODS.contains(&method) => self.resource_request(request),
            method if RELEASE3_METHODS.contains(&method) => self.release3_request(request),
            method if RELEASE4_METHODS.contains(&method) => self.release4_request(request),
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
                    .chain(RELEASE3_METHODS)
                    .chain(RELEASE4_METHODS)
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
        let requested_working_directory = params
            .working_directory
            .clone()
            .unwrap_or_else(|| ".".to_owned());
        let attachment = if let Some(selector) = params.resume.as_deref() {
            match self.server.release3.dispatch(
                "session/resume",
                &result_map([
                    ("sessionId", json!(selector)),
                    ("systemPrompt", json!("")),
                    ("config", json!({})),
                ]),
            ) {
                Ok(dispatch) => dispatch.attachment,
                Err(error) => return release3_error_batch(request.id, error),
            }
        } else if params.continue_session {
            match self.server.release3.dispatch(
                "session/continue",
                &result_map([
                    ("cwd", json!(requested_working_directory)),
                    ("systemPrompt", json!("")),
                    ("config", json!({})),
                ]),
            ) {
                Ok(dispatch) => dispatch.attachment,
                Err(error) => return release3_error_batch(request.id, error),
            }
        } else {
            None
        };
        let session_id = attachment
            .as_ref()
            .map(|attachment| attachment.id.clone())
            .or_else(|| params.session_id.clone())
            .unwrap_or_else(|| {
                generated_session_id(self.server.next_session.fetch_add(1, Ordering::Relaxed))
            });
        let working_directory = attachment
            .as_ref()
            .map(|attachment| attachment.working_directory.clone())
            .unwrap_or(requested_working_directory);
        let created_at = attachment
            .as_ref()
            .map(|attachment| attachment.hydrated.metadata.created_at_ms)
            .filter(|timestamp| *timestamp != 0)
            .unwrap_or_else(now_millis);
        let updated_at = attachment
            .as_ref()
            .map(|attachment| attachment.hydrated.metadata.updated_at_ms)
            .filter(|timestamp| *timestamp != 0)
            .unwrap_or(created_at);
        let initial_snapshot = attachment
            .as_ref()
            .map(|attachment| persisted_projection(&attachment.hydrated, params.history_limit));
        let mut persisted = attachment
            .as_ref()
            .map(|attachment| attachment.hydrated.clone());
        let attachment_agent = attachment
            .as_ref()
            .and_then(|attachment| attachment.agent.clone());
        let mut aliases = BTreeSet::new();
        for alias in [params.session_id.as_deref(), params.resume.as_deref()]
            .into_iter()
            .flatten()
        {
            if alias != session_id {
                aliases.insert(alias.to_owned());
            }
        }
        let mcp_configs = match self.server.release3.mcp_servers_for_session(
            Path::new(&working_directory),
            params.trusted,
            &params.mcp_servers,
        ) {
            Ok(configs) => configs,
            Err(error) => return release3_error_batch(request.id, error),
        };
        let selected_agent = match params.agent.clone().or(attachment_agent) {
            Some(agent) => agent,
            None => match self.server.release3.default_agent_name() {
                Ok(agent) => agent,
                Err(error) => return release3_error_batch(request.id, error),
            },
        };
        let agent_profile = if params.agent.is_none() {
            attachment
                .as_ref()
                .and_then(|attachment| attachment.agent_profile.clone())
                .map_or_else(|| self.server.release3.agent_profile(&selected_agent), Ok)
        } else {
            self.server.release3.agent_profile(&selected_agent)
        };
        let agent_profile = match agent_profile {
            Ok(profile) => profile,
            Err(error) => return release3_error_batch(request.id, error),
        };
        let should_persist_agent = attachment.is_none() || params.agent.is_some();
        let mut intent = SessionIntent {
            add_directories: params.add_directories,
            trusted: params.trusted,
            agent: Some(selected_agent),
            tool_filters: params.tool_filters,
            enabled_tools: params.enabled_tools.unwrap_or_default(),
            disabled_tools: params.disabled_tools,
            requested_enabled_tools: Vec::new(),
            requested_disabled_tools: Vec::new(),
            agent_permission_rules: Vec::new(),
            mcp_servers: params.mcp_servers,
            model: params.model,
            max_turns: params.max_turns,
            max_tokens: params.max_tokens,
            max_price_micros,
            mode: params.mode,
            thinking: params.thinking,
            reasoning_effort: params.reasoning_effort,
            auto_approve: params.auto_approve,
            requested_auto_approve: params.auto_approve,
            approval: AgentApproval::Prompt,
            system_prompt_id: None,
            resume: attachment
                .as_ref()
                .map(|attachment| attachment.id.clone())
                .or(params.resume),
            continue_session: params.continue_session && attachment.is_none(),
        };
        intent
            .requested_enabled_tools
            .clone_from(&intent.enabled_tools);
        intent
            .requested_disabled_tools
            .clone_from(&intent.disabled_tools);
        apply_agent_profile_settings(&mut intent, &agent_profile);
        let permission_store = PermissionStore::default();
        let tools = ToolRegistry::default();
        if let Err(error) = permission_store.try_replace_rules_with_rationale_prefix(
            "agent-profile:",
            intent.agent_permission_rules.clone(),
        ) {
            return error_batch(
                request.id,
                ProtocolErrorCode::InternalError,
                &error.to_string(),
            );
        }
        if intent.trusted
            && Workspace::open(&working_directory).is_ok()
            && let Err(error) = permission_store.try_set_trust(
                &working_directory,
                TrustDecision::SessionTrusted,
                TrustRootKind::Workspace,
            )
        {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                &error.to_string(),
            );
        }
        let review = match self.server.register_workspace_tools(
            &session_id,
            &working_directory,
            &permission_store,
            &tools,
            &intent,
            None,
        ) {
            Ok(review) => review,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        if let Err(error) = self
            .server
            .session_tool_factory
            .register(&session_id, &tools)
        {
            return error_batch(request.id, ProtocolErrorCode::InternalError, &error);
        }
        if persisted.is_none() && self.server.release3.persists_runtime_sessions() {
            match self.server.release3.create_runtime_session(
                &session_id,
                &working_directory,
                created_at,
            ) {
                Ok(hydrated) => persisted = Some(hydrated),
                Err(error) => return release3_error_batch(request.id, error),
            }
        }
        if should_persist_agent {
            match self
                .server
                .release3
                .update_runtime_agent(&session_id, &agent_profile.name)
            {
                Ok(Some(hydrated)) => persisted = Some(hydrated),
                Ok(None) => {}
                Err(error) => return release3_error_batch(request.id, error),
            }
        }
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        if sessions.contains(&session_id) {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Session already exists",
            );
        }
        let project_trusted = intent.trusted;
        let mut session = SessionRuntime::new(
            session_id.clone(),
            working_directory.clone(),
            intent,
            permission_store.clone(),
            tools.clone(),
            review,
            created_at,
        );
        session.updated_at = updated_at;
        session.snapshot = initial_snapshot;
        session.aliases = aliases;
        session.persisted = persisted;
        sessions.insert(session);
        if let Err(error) = self.server.open_session_resources(
            &mut sessions,
            ResourceSession {
                session_id: session_id.clone(),
                generation: 1,
                working_directory,
                project_trusted,
                policy: permission_store,
                tools,
            },
        ) {
            return resource_error_batch(request.id, error);
        }
        self.attached_sessions.insert(session_id.clone());
        let state = sessions
            .get(&session_id)
            .map(public_session_state)
            .unwrap_or(Value::Null);
        let mut batch = success_batch(request.id, result_map([("state", state)]));
        if !mcp_configs.is_empty() {
            batch.deferred.push(DeferredWork::ConfigureMcp {
                session_id,
                configs: mcp_configs,
            });
        }
        batch
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
        let Some(key) = sessions.key(&params.session_id).map(ToOwned::to_owned) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        let Some(session) = sessions.get_mut(&key) else {
            return error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found");
        };
        if session.compaction_pending {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Cannot close while compaction is active",
            );
        }
        let canonical_session_id = session.id.clone();
        if let Err(error) = self
            .server
            .release3
            .close_saved_session(&canonical_session_id, now_millis())
        {
            return release3_error_batch(request.id, error);
        }
        if let Err(error) = self
            .server
            .release4
            .close_transient_session(&canonical_session_id)
        {
            return internal_error_batch(request.id, &ServerError::Release4(error.to_string()));
        }
        let active_turn = session.active_turn.clone();
        session.status = SessionStatus::Closed;
        session.updated_at = now_millis();
        if cancel_pending_callback(session, "Session was closed") {
            next_event_id(session);
        }
        if self.attached_sessions.remove(&key) {
            session.attachments = session.attachments.saturating_sub(1);
        }
        let session_id = canonical_session_id;
        let resource_generation = session.resource_generation;
        drop(sessions);
        if let Ok(mut resources) = self.server.resources.lock() {
            resources.close_session(&session_id);
        }
        self.state = ConnectionState::Closed;
        let mut deferred = active_turn
            .map(|turn_id| DeferredWork::InterruptTurn {
                session_id: session_id.clone(),
                turn_id,
            })
            .into_iter()
            .collect::<Vec<_>>();
        if self.server.resource_backend.is_some() {
            deferred.push(DeferredWork::CloseResources {
                session_id: session_id.clone(),
                generation: resource_generation,
            });
        }
        DispatchBatch {
            outbound: success_bytes(request.id, BTreeMap::new())
                .into_iter()
                .collect(),
            deferred,
            close_after_flush: true,
        }
    }

    fn release3_request(&mut self, request: ServerRequest) -> DispatchBatch {
        session_management::dispatch(self, request)
    }

    fn session_settings_update(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionSettingsUpdateParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if params.max_turns.is_none()
            && params.model.is_none()
            && params.max_tokens.is_none()
            && params.mode.is_none()
            && params.thinking.is_none()
            && params.reasoning_effort.is_none()
            && params.auto_approve.is_none()
        {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "At least one session setting must be provided",
            );
        }
        if params
            .mode
            .as_deref()
            .is_some_and(|mode| !matches!(mode, "code" | "plan"))
        {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "mode must be `code` or `plan`",
            );
        }
        if params
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| !matches!(effort, "low" | "medium" | "high" | "max"))
        {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "reasoningEffort must be low, medium, high, or max",
            );
        }
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let canonical_session_id = {
            let sessions = match self.server.lock_sessions() {
                Ok(sessions) => sessions,
                Err(error) => return internal_error_batch(request.id, &error),
            };
            let Some(session) = sessions.get(&params.session_id) else {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::NotFound,
                    "Session was not found",
                );
            };
            if params.auto_approve.is_some() && session.active_turn.is_some() {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::Conflict,
                    "autoApprove can only change while the session is idle",
                );
            }
            session.id.clone()
        };
        let approval_changed = params.auto_approve.is_some();
        let mut persisted_settings = BTreeMap::new();
        if let Some(value) = &params.model {
            persisted_settings.insert("active_model".to_owned(), json!(value));
        }
        if let Some(value) = params.max_turns {
            persisted_settings.insert("maxTurns".to_owned(), json!(value));
        }
        if let Some(value) = params.max_tokens {
            persisted_settings.insert("maxTokens".to_owned(), json!(value));
        }
        if let Some(value) = &params.mode {
            persisted_settings.insert("mode".to_owned(), json!(value));
        }
        if let Some(value) = params.thinking {
            persisted_settings.insert("thinking".to_owned(), json!(value));
        }
        if let Some(value) = &params.reasoning_effort {
            persisted_settings.insert("reasoningEffort".to_owned(), json!(value));
        }
        if let Some(value) = params.auto_approve {
            persisted_settings.insert("autoApprove".to_owned(), json!(value));
        }
        let persisted = match self
            .server
            .release3
            .update_runtime_settings(&canonical_session_id, &persisted_settings)
        {
            Ok(persisted) => persisted,
            Err(error) => return release3_error_batch(request.id, error),
        };
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request.id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(
                request.id,
                ProtocolErrorCode::NotFound,
                "Session was not found",
            );
        };
        if let Some(model) = params.model {
            session.intent.model = Some(model);
        }
        if let Some(max_turns) = params.max_turns {
            session.intent.max_turns = Some(max_turns);
        }
        if let Some(max_tokens) = params.max_tokens {
            session.intent.max_tokens = Some(max_tokens);
        }
        if let Some(mode) = params.mode {
            session.intent.mode = Some(mode);
        }
        if let Some(thinking) = params.thinking {
            session.intent.thinking = thinking;
        }
        if let Some(reasoning_effort) = params.reasoning_effort {
            session.intent.reasoning_effort = Some(reasoning_effort);
        }
        if let Some(auto_approve) = params.auto_approve {
            session.intent.auto_approve = auto_approve;
            session.intent.requested_auto_approve = auto_approve;
        }
        if let Some(persisted) = persisted {
            session.persisted = Some(persisted);
        }
        session.updated_at = now_millis();
        drop(sessions);
        if approval_changed
            && let Err(error) = self
                .server
                .refresh_session_workspace_tools(&canonical_session_id)
        {
            return internal_error_batch(request.id, &error);
        }
        success_batch(request.id, BTreeMap::new())
    }

    fn session_compact_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionCompactParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        let canonical_session_id = {
            let mut sessions = match self.server.lock_sessions() {
                Ok(sessions) => sessions,
                Err(error) => return internal_error_batch(request.id, &error),
            };
            let Some(session) = sessions.get_mut(&params.session_id)
            else {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::NotFound,
                    "Session was not found",
                );
            };
            if session.active_turn.is_some() || session.compaction_pending {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::Conflict,
                    "Cannot compact while the session has active work",
                );
            }
            if session.status == SessionStatus::Closed {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::Conflict,
                    "Cannot compact a closed session",
                );
            }
            session.compaction_pending = true;
            session.updated_at = now_millis();
            session.id.clone()
        };
        DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::CompactSession {
                request_id: request.id,
                session_id: canonical_session_id,
                extra_instructions: params.extra_instructions,
            }],
            close_after_flush: false,
        }
    }

    fn release4_request(&mut self, mut request: ServerRequest) -> DispatchBatch {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(session_id) = &session_id {
            if let Some(batch) = self.attachment_error(request.id.clone(), session_id) {
                return batch;
            }
            let sessions = match self.server.lock_sessions() {
                Ok(sessions) => sessions,
                Err(error) => return internal_error_batch(request.id, &error),
            };
            let Some(session) = sessions.get(session_id) else {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::NotFound,
                    "Session was not found",
                );
            };
            if matches!(
                request.method.as_str(),
                "loops/create" | "loops/delete" | "loops/clear"
            ) && session.active_turn.is_some()
            {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::Conflict,
                    "Scheduled loops can only change while the session is idle",
                );
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
            .release4
            .requires_deferred_dispatch(&request.method)
        {
            return DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::CloudRequest {
                    request_id: request.id,
                    method: request.method,
                    params: request.params,
                }],
                close_after_flush: false,
            };
        }
        match self
            .server
            .release4
            .dispatch(&request.method, &request.params)
        {
            Ok(dispatch) => release4_dispatch_batch(request.id, dispatch),
            Err(error) => release4_error_batch(request.id, error),
        }
    }

    fn turn_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let mut params = match from_params::<TurnStartParams>(&request.params) {
            Ok(params) => params,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, &message);
            }
        };
        let scheduled = match scheduled_loop_turn(&params.user_display_content) {
            Ok(scheduled) => scheduled,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, message);
            }
        };
        let mut prompt = content_text(&params.input);
        if scheduled.is_none() && prompt.trim().is_empty() {
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
        if session.active_turn.is_some()
            || session.compaction_pending
            || session.status == SessionStatus::Closed
        {
            return error_batch(
                request.id,
                ProtocolErrorCode::Conflict,
                "Session cannot start another turn",
            );
        }
        let mut loop_notice = None;
        if let Some((loop_id, fired_at)) = scheduled {
            let fire = match self.server.release4.fire_loop_for_session(
                &loop_id,
                &session.id,
                fired_at,
                true,
            ) {
                Ok(fire) => fire,
                Err(error) => return release4_error_batch(request.id, error),
            };
            prompt.clone_from(&fire.prompt);
            params.input = vec![PublicContentBlock::Text { text: fire.prompt }];
            params.user_display_content = Some(json!({
                "kind": "scheduled_loop",
                "loopId": fire.loop_id,
                "firedAt": fired_at,
            }));
            session.active_scheduled_loop = Some(loop_id);
            loop_notice = Some((fire.notice, fired_at));
        }
        let turn_sequence = self.server.next_turn.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!("turn-{turn_sequence}");
        if let Some(review) = &session.review {
            let message_index = match review_message_index(&self.server.release3, session) {
                Ok(message_index) => message_index,
                Err(error) => return internal_error_batch(request.id, &error),
            };
            if let Err(error) = review.begin_turn_at(&turn_id, message_index) {
                return internal_error_batch(request.id, &ServerError::Resource(error.to_string()));
            }
        }
        session.active_turn = Some(turn_id.clone());
        let started_at = now_millis();
        session.active_turn_started_at = Some(started_at);
        session.status = SessionStatus::Running;
        let canonical_session_id = session.id.clone();
        let turn = PublicTurn {
            id: turn_id.clone(),
            session_id: canonical_session_id.clone(),
            status: PublicTurnStatus::InProgress,
            started_at,
            completed_at: None,
            error: None,
            stop_reason: None,
        };
        session.latest_turn = Some(turn.clone());
        session.updated_at = started_at;
        let mut outbound = success_bytes(request.id.clone(), result_map([("turn", json!(turn))]))
            .into_iter()
            .collect::<Vec<_>>();
        if let Some((mut notice, fired_at)) = loop_notice {
            let event_id = next_event_id(session);
            notice.params.insert("eventId".to_owned(), json!(event_id));
            notice
                .params
                .insert("sessionId".to_owned(), json!(canonical_session_id.clone()));
            notice
                .params
                .insert("turnId".to_owned(), json!(turn_id.clone()));
            notice.params.insert(
                "emittedAt".to_owned(),
                json!(fired_at.saturating_mul(1_000)),
            );
            if let Some(entry) = notice.params.get_mut("entry") {
                entry["id"] = json!(format!("scheduled-loop:{turn_id}"));
                entry["sessionId"] = json!(canonical_session_id.clone());
                entry["turnId"] = json!(turn_id.clone());
            }
            match encode_notification(&notice.method, notice.params) {
                Ok(notification) => outbound.push(notification),
                Err(error) => return internal_error_batch(request.id, &error),
            }
        }
        DispatchBatch {
            outbound,
            deferred: vec![DeferredWork::RunTurn {
                session_id: canonical_session_id,
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
        let completed_at = now_millis();
        if let Some(loop_id) = &session.active_scheduled_loop
            && let Err(error) = self
                .server
                .release4
                .finish_loop_fire(loop_id, completed_at / 1_000)
        {
            return release4_error_batch(request.id, error);
        }
        if let Some(review) = &session.review
            && let Err(error) = review.seal_turn()
        {
            return internal_error_batch(request.id, &ServerError::Resource(error.to_string()));
        }
        let started_at = session.active_turn_started_at.unwrap_or_default();
        let canonical_session_id = session.id.clone();
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.active_scheduled_loop = None;
        session.status = SessionStatus::Cancelled;
        session.latest_turn = Some(PublicTurn {
            id: params.expected_turn_id.clone(),
            session_id: canonical_session_id,
            status: PublicTurnStatus::Interrupted,
            started_at,
            completed_at: Some(completed_at),
            error: None,
            stop_reason: None,
        });
        session.updated_at = completed_at;
        cancel_pending_callback(session, "Turn was interrupted");
        next_event_id(session);
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
        let Some(callback) = &session.pending_callback else {
            let kind = match validate_callback_output(&params.output) {
                Ok(kind) => kind,
                Err(message) => {
                    return error_batch(request.id, ProtocolErrorCode::InvalidParams, message);
                }
            };
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
        let kind = match validate_callback_output_against_request(&params.output, callback) {
            Ok(kind) => kind,
            Err(message) => {
                return error_batch(request.id, ProtocolErrorCode::InvalidParams, message);
            }
        };
        let cancel_turn = callback_requests_turn_cancel(&params.output);
        let value = serde_json::to_string(&params.output).ok();
        settle_pending_callback(
            session,
            &params.callback_id,
            PublicCallbackState::Answered {
                output: params.output.clone(),
            },
        );
        session.pending_callback = None;
        session.resolved_callbacks.insert(
            params.callback_id.clone(),
            ResolvedCallback {
                kind,
                output: params.output.clone(),
            },
        );
        session.status = SessionStatus::Running;
        session.updated_at = now_millis();
        next_event_id(session);
        let result = result_map([("status", json!("accepted"))]);
        let mut deferred = vec![DeferredWork::ResolveCallback {
            session_id: params.session_id.clone(),
            turn_id: turn_id.clone(),
            callback_id: params.callback_id.clone(),
            accepted: true,
            value,
        }];
        if cancel_turn {
            deferred.push(DeferredWork::InterruptTurn {
                session_id: params.session_id.clone(),
                turn_id,
            });
        }
        drop(sessions);
        for route in self.pending_server_requests.values_mut() {
            if route.session_id == params.session_id && route.callback_id == params.callback_id {
                route.answered = true;
            }
        }
        DispatchBatch {
            outbound: success_bytes(request.id, result).into_iter().collect(),
            deferred,
            close_after_flush: false,
        }
    }

    fn resource_request(&mut self, mut request: ServerRequest) -> DispatchBatch {
        let Some(session_id) = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned)
        else {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "sessionId must be a non-empty string",
            );
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &session_id) {
            return batch;
        }
        if request.method.starts_with("workspace/trust/")
            && let Some(batch) = self.confine_trust_request(&mut request, &session_id)
        {
            return batch;
        }
        let session_active = match self.server.lock_sessions() {
            Ok(sessions) => sessions
                .get(&session_id)
                .is_some_and(|session| session.active_turn.is_some()),
            Err(error) => return internal_error_batch(request.id, &error),
        };
        if BACKEND_RESOURCE_METHODS.contains(&request.method.as_str())
            && self.server.resource_backend.is_some()
        {
            let command = match ResourceBackendCommand::parse(
                &request.method,
                &request.params,
                session_active,
            ) {
                Ok(command) => command,
                Err(error) => return resource_error_batch(request.id, error),
            };
            return DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::ResourceRequest {
                    request_id: request.id,
                    session_id,
                    command,
                }],
                close_after_flush: false,
            };
        }
        let result = match self.server.resources.lock() {
            Ok(mut resources) => {
                resources.dispatch(&request.method, &request.params, session_active)
            }
            Err(_) => {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::InternalError,
                    "Resource state lock is poisoned",
                );
            }
        };
        if request.method == "workspace/trust/decision" && result.is_ok() {
            let trusted = request
                .params
                .get("decision")
                .and_then(Value::as_str)
                .is_some_and(|decision| matches!(decision, "trust_repo" | "trust_cwd"));
            match self.server.lock_sessions() {
                Ok(mut sessions) => {
                    if let Some(session) = sessions.get_mut(&session_id) {
                        session.intent.trusted = trusted;
                    }
                }
                Err(error) => return internal_error_batch(request.id, &error),
            }
        }
        resource_result_batch(request.id, result)
    }

    /// Pins a workspace-trust request to the attached session root.
    ///
    /// The request may omit `cwd`, in which case the session root is filled in;
    /// naming any other directory is refused, so a connection cannot grant trust
    /// outside the workspace it is attached to.
    fn confine_trust_request(
        &self,
        request: &mut ServerRequest,
        session_id: &str,
    ) -> Option<DispatchBatch> {
        let working_directory = match self.server.lock_sessions() {
            Ok(sessions) => sessions
                .get(session_id)
                .map(|session| session.working_directory.clone()),
            Err(error) => return Some(internal_error_batch(request.id.clone(), &error)),
        };
        let Some(working_directory) = working_directory else {
            return Some(error_batch(
                request.id.clone(),
                ProtocolErrorCode::NotFound,
                "Session was not found",
            ));
        };
        let requested = request
            .params
            .entry("cwd".to_owned())
            .or_insert_with(|| json!(working_directory));
        let Some(requested) = requested.as_str() else {
            return Some(error_batch(
                request.id.clone(),
                ProtocolErrorCode::InvalidParams,
                "cwd must be a string",
            ));
        };
        (!same_filesystem_path(requested, &working_directory)).then(|| {
            error_batch(
                request.id.clone(),
                ProtocolErrorCode::Forbidden,
                "Workspace trust can only change the attached session root",
            )
        })
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

fn same_filesystem_path(left: &str, right: &str) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
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

#[derive(Clone)]
struct SessionRuntime {
    id: String,
    working_directory: String,
    intent: SessionIntent,
    status: SessionStatus,
    active_turn: Option<String>,
    active_turn_started_at: Option<u64>,
    active_scheduled_loop: Option<String>,
    compaction_pending: bool,
    pending_callback: Option<PendingCallback>,
    resolved_callbacks: BTreeMap<String, ResolvedCallback>,
    context: Vec<String>,
    steering: Vec<String>,
    snapshot: Option<ProjectionSnapshot>,
    attachments: u32,
    resource_generation: u64,
    aliases: BTreeSet<String>,
    created_at: u64,
    updated_at: u64,
    latest_turn: Option<PublicTurn>,
    event_watermark: u64,
    policy: PermissionStore,
    tools: ToolRegistry,
    persisted: Option<HydratedSession>,
    review: Option<Arc<ReviewManager>>,
}

impl SessionRuntime {
    /// A freshly attached, idle session holding one attachment.
    ///
    /// Callers set the fields that vary by entry point: `persisted`,
    /// `snapshot`, `aliases` and `updated_at`.
    fn new(
        id: String,
        working_directory: String,
        intent: SessionIntent,
        policy: PermissionStore,
        tools: ToolRegistry,
        review: Option<Arc<ReviewManager>>,
        created_at: u64,
    ) -> Self {
        Self {
            id,
            working_directory,
            intent,
            status: SessionStatus::Idle,
            active_turn: None,
            active_turn_started_at: None,
            active_scheduled_loop: None,
            compaction_pending: false,
            pending_callback: None,
            resolved_callbacks: BTreeMap::new(),
            context: Vec::new(),
            steering: Vec::new(),
            snapshot: None,
            attachments: 1,
            resource_generation: 1,
            aliases: BTreeSet::new(),
            created_at,
            updated_at: created_at,
            latest_turn: None,
            event_watermark: 0,
            policy,
            tools,
            persisted: None,
            review,
        }
    }
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
    answered: bool,
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
    model: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default, rename = "maxSessionTokens", alias = "maxTokens")]
    max_tokens: Option<u64>,
    #[serde(default)]
    max_price_micros: Option<u64>,
    #[serde(default)]
    max_price: Option<f64>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    thinking: bool,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    auto_approve: bool,
    #[serde(default)]
    resume: Option<String>,
    #[serde(default, rename = "continue")]
    continue_session: bool,
    /// Accepted for wire compatibility. The server behaves the same either
    /// way, so nothing reads it yet.
    #[allow(dead_code)]
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
    #[serde(skip)]
    pub requested_enabled_tools: Vec<String>,
    #[serde(skip)]
    pub requested_disabled_tools: Vec<String>,
    #[serde(skip)]
    pub agent_permission_rules: Vec<PermissionRule>,
    pub mcp_servers: Vec<Value>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_price_micros: Option<u64>,
    pub mode: Option<String>,
    pub thinking: bool,
    pub reasoning_effort: Option<String>,
    pub auto_approve: bool,
    #[serde(skip)]
    pub requested_auto_approve: bool,
    #[serde(skip)]
    pub approval: AgentApproval,
    #[serde(default)]
    pub system_prompt_id: Option<String>,
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
struct SessionSettingsUpdateParams {
    session_id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    thinking: Option<bool>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    auto_approve: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionCompactParams {
    session_id: String,
    #[serde(default)]
    extra_instructions: String,
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
    /// Accepted for wire compatibility. Steering does not create a history
    /// entry, so none of these three reach the engine yet.
    #[allow(dead_code)]
    #[serde(default)]
    client_user_message_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default = "default_true")]
    inject_invoked_skill: bool,
    #[allow(dead_code)]
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
    /// Accepted for wire compatibility; injection does not resolve skills or
    /// mentions yet.
    #[allow(dead_code)]
    #[serde(default)]
    inject_invoked_skill: bool,
    #[serde(default)]
    client_user_message_id: Option<String>,
    #[allow(dead_code)]
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

fn scheduled_loop_turn(
    user_display_content: &Option<Value>,
) -> Result<Option<(String, u64)>, &'static str> {
    let Some(content) = user_display_content else {
        return Ok(None);
    };
    if content.get("kind").and_then(Value::as_str) != Some("scheduled_loop") {
        return Ok(None);
    }
    let loop_id = content
        .get("loopId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("scheduled loop turn requires loopId")?;
    let fired_at = content
        .get("firedAt")
        .and_then(Value::as_u64)
        .ok_or("scheduled loop turn requires integer firedAt")?;
    Ok(Some((loop_id.to_owned(), fired_at)))
}

const fn default_true() -> bool {
    true
}


fn price_dollars_to_micros(price: f64) -> Option<u64> {
    (price.is_finite() && price >= 0.0 && price <= u64::MAX as f64 / 1_000_000.0)
        .then(|| (price * 1_000_000.0).round() as u64)
}

fn review_message_index(
    release3: &Release3Service,
    session: &SessionRuntime,
) -> Result<usize, ServerError> {
    release3
        .message_count(&session.id)
        .map_err(|error| ServerError::Resource(error.to_string()))
        .map(|message_count| {
            message_count.unwrap_or_else(|| {
                session
                    .persisted
                    .as_ref()
                    .map(|persisted| persisted.messages.len())
                    .unwrap_or_default()
            })
        })
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
    #[error("release-4 workflow failed: {0}")]
    Release4(String),
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
    #[error("callback detail is invalid: {0}")]
    InvalidCallbackDetail(String),
    #[error("server state lock is poisoned")]
    StatePoisoned,
    #[error("tool execution failed: {0}")]
    Tool(String),
    #[error("resource cleanup failed: {0}")]
    Resource(String),
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

fn next_event_id(session: &mut SessionRuntime) -> u64 {
    session.event_watermark = session.event_watermark.saturating_add(1);
    session.event_watermark
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


fn generated_session_id(sequence: u64) -> String {
    format!("session-{}-{sequence}", now_millis())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use toml::Table;

    /// `SERVER_METHODS` is the contract this build advertises; the routing
    /// tables are what it actually answers. Nothing else keeps them aligned.
    #[test]
    fn advertised_methods_match_routed_methods() {
        let routed = IMPLEMENTED_METHODS
            .iter()
            .chain(RELEASE3_METHODS)
            .chain(RELEASE4_METHODS)
            .chain(RESOURCE_METHODS)
            .copied()
            .collect::<BTreeSet<_>>();
        let advertised = vibe_protocol::SERVER_METHODS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            advertised.difference(&routed).copied().collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "declared in SERVER_METHODS but routed nowhere"
        );
        assert_eq!(
            routed.difference(&advertised).copied().collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "routed but missing from SERVER_METHODS"
        );
    }

    struct RejectForkTools;

    impl SessionToolFactory for RejectForkTools {
        fn register(&self, session_id: &str, _tools: &ToolRegistry) -> Result<(), String> {
            if session_id == "source-session" {
                Ok(())
            } else {
                Err("injected fork attachment failure".to_owned())
            }
        }
    }

    #[derive(Default)]
    struct RecordingResourceBackend {
        opened_with_tools: Mutex<Option<usize>>,
        mcp_added: Mutex<bool>,
        closed: Mutex<Vec<String>>,
    }

    impl ResourceBackend for RecordingResourceBackend {
        fn open_session(&self, session: ResourceSession) -> Result<(), ResourceError> {
            let count = session
                .tools
                .list()
                .map_err(|error| ResourceError::Unavailable(error.to_string()))?
                .len();
            *self
                .opened_with_tools
                .lock()
                .map_err(|_| ResourceError::Unavailable("test backend lock".to_owned()))? =
                Some(count);
            Ok(())
        }

        fn dispatch<'a>(
            &'a self,
            request: ResourceBackendRequest,
        ) -> crate::resources::ResourceFuture<'a, ResourceDispatch> {
            Box::pin(async move {
                match request.command {
                    ResourceBackendCommand::Mcp(crate::resources::McpCommand::Add(_)) => {
                        *self.mcp_added.lock().map_err(|_| {
                            ResourceError::Unavailable("test backend lock".to_owned())
                        })? = true;
                        Ok(ResourceDispatch {
                            result: result_map([("mcp", json!({"sources": ["example"]}))]),
                            notification: Some(crate::resources::ResourceNotification {
                                method: "mcp/updated".to_owned(),
                                params: result_map([("mcp", json!({"sources": ["example"]}))]),
                            }),
                        })
                    }
                    ResourceBackendCommand::Mcp(crate::resources::McpCommand::Read) => {
                        let added = *self.mcp_added.lock().map_err(|_| {
                            ResourceError::Unavailable("test backend lock".to_owned())
                        })?;
                        Ok(ResourceDispatch {
                            result: result_map([(
                                "mcp",
                                json!({"sources": if added { vec!["example"] } else { vec![] }}),
                            )]),
                            notification: None,
                        })
                    }
                    command => Err(ResourceError::MethodNotFound(format!("{command:?}"))),
                }
            })
        }

        fn close_session<'a>(
            &'a self,
            session_id: &'a str,
            _generation: u64,
        ) -> crate::resources::ResourceFuture<'a, ()> {
            Box::pin(async move {
                self.closed
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("test backend lock".to_owned()))?
                    .push(session_id.to_owned());
                Ok(())
            })
        }
    }

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

    #[tokio::test]
    async fn accept_edits_profile_auto_approves_only_mutating_file_tools() {
        let factory = DefaultApprovalFactory;
        let approval = factory.for_agent("session", AgentApproval::Edits, false);
        let edit = approval
            .request(ApprovalRequest {
                tool: "edit".to_owned(),
                input: Value::Null,
                requirements: vec![PermissionRequirement::Write {
                    path: PathBuf::from("/workspace/file.rs"),
                }],
                rationale: "edit file".to_owned(),
            })
            .await
            .expect("edit decision");
        let read = approval
            .request(ApprovalRequest {
                tool: "read".to_owned(),
                input: Value::Null,
                requirements: vec![PermissionRequirement::Read {
                    path: PathBuf::from("/workspace/file.rs"),
                }],
                rationale: "read file".to_owned(),
            })
            .await
            .expect("read decision");

        assert_eq!(edit, ApprovalDecision::ApproveOnce);
        assert_eq!(read, ApprovalDecision::Deny);
    }

    #[test]
    fn agent_profile_keeps_requested_denials_and_explicit_auto_approval() {
        let temporary = tempfile::tempdir().expect("temporary profile root");
        let profile = crate::builtin_agents::profiles(temporary.path())
            .into_iter()
            .find(|profile| profile.name == "plan")
            .expect("plan profile");
        let mut intent = SessionIntent {
            disabled_tools: vec!["shell".to_owned()],
            requested_disabled_tools: vec!["shell".to_owned()],
            auto_approve: true,
            requested_auto_approve: true,
            ..SessionIntent::default()
        };

        apply_agent_profile_settings(&mut intent, &profile);

        assert_eq!(intent.disabled_tools, ["shell"]);
        assert!(intent.agent_permission_rules.iter().any(|rule| {
            rule.tool == "edit"
                && rule.scope.ends_with("/plans *")
                && rule.mode == vibe_core::policy::PermissionMode::Always
        }));
        assert!(intent.auto_approve);
        assert_eq!(intent.mode.as_deref(), Some("plan"));
    }

    fn message_entry(
        id: &str,
        session_id: &str,
        turn_id: &str,
        created_at: u64,
        text: &str,
    ) -> PublicHistoryEntry {
        PublicHistoryEntry::Message {
            metadata: PublicEntryMetadata {
                id: id.to_owned(),
                session_id: session_id.to_owned(),
                turn_id: Some(turn_id.to_owned()),
                created_at,
                updated_at: created_at,
                generation_status: PublicEntryGenerationStatus::Completed,
                related_entry_id: None,
            },
            role: PublicMessageRole::Assistant,
            content: vec![PublicContentBlock::Text {
                text: text.to_owned(),
            }],
            source: None,
            user_display_content: None,
        }
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
    fn settings_update_is_strict_and_applies_to_the_next_turn_while_active() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        let turn = connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        assert_eq!(turn.deferred.len(), 1);

        let update = connection.dispatch(&request(
            4,
            "session/settings/update",
            json!({
                "sessionId": "session-1",
                "model": "next-model",
                "maxTurns": 0,
                "maxTokens": 64
            }),
        ));
        assert_eq!(update.outbound.len(), 1);
        let session = server.session("session-1").expect("session");
        assert_eq!(session.intent.max_turns, Some(0));
        assert_eq!(session.intent.max_tokens, Some(64));
        assert_eq!(session.intent.model.as_deref(), Some("next-model"));
        assert_eq!(session.active_turn.as_deref(), Some("turn-1"));

        for (id, params) in [
            (5, json!({"sessionId": "session-1"})),
            (6, json!({"sessionId": "session-1", "maxTurns": 1.5})),
            (7, json!({"sessionId": "session-1", "maxTokens": -1})),
            (
                8,
                json!({"sessionId": "session-1", "maxTurns": 1, "future": true}),
            ),
        ] {
            let invalid = decode_frame(
                &connection
                    .dispatch(&request(id, "session/settings/update", params))
                    .outbound[0],
            )
            .expect("error response");
            assert!(matches!(
                invalid,
                Envelope::Error(ErrorResponse {
                    error: ProtocolError {
                        code: ProtocolErrorCode::InvalidParams,
                        ..
                    },
                    ..
                })
            ));
        }
        let active_approval = connection.dispatch(&request(
            9,
            "session/settings/update",
            json!({"sessionId": "session-1", "autoApprove": true}),
        ));
        assert!(matches!(
            decode_frame(&active_approval.outbound[0]).expect("active approval response"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::Conflict,
                    ..
                },
                ..
            })
        ));
        let active_agent = connection.dispatch(&request(
            10,
            "session/agent/update",
            json!({"sessionId": "session-1", "name": "plan"}),
        ));
        assert!(matches!(
            decode_frame(&active_agent.outbound[0]).expect("active agent response"),
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
    fn manual_compaction_reserves_exclusive_session_work_and_failure_releases_it() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);

        let compact = connection.dispatch(&request(
            3,
            "session/compact/start",
            json!({"sessionId": "session-1", "extraInstructions": "preserve decisions"}),
        ));
        assert!(compact.outbound.is_empty());
        assert_eq!(
            compact.deferred,
            vec![DeferredWork::CompactSession {
                request_id: RequestId::Integer(3),
                session_id: "session-1".to_owned(),
                extra_instructions: "preserve decisions".to_owned(),
            }]
        );
        let blocked = connection.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "race"}]}),
        ));
        assert!(matches!(
            decode_frame(&blocked.outbound[0]).expect("conflict"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::Conflict,
                    ..
                },
                ..
            })
        ));

        let failure = server.fail_manual_compaction(
            RequestId::Integer(3),
            "session-1",
            "injected provider failure",
        );
        assert!(matches!(
            decode_frame(&failure.outbound[0]).expect("typed compaction failure"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::CompactionFailed,
                    ..
                },
                ..
            })
        ));
        let next = connection.dispatch(&request(
            5,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "retry"}]}),
        ));
        assert_eq!(next.deferred.len(), 1);
    }

    #[test]
    fn due_loop_reserves_the_normal_turn_path_and_emits_a_sequenced_notice() {
        let temporary = tempfile::tempdir().expect("loop store");
        let loop_path = temporary.path().join("loops.json");
        let release4 = Release4Service::default()
            .with_loop_store(loop_path)
            .expect("persistent loop service");
        let created = release4
            .dispatch(
                "loops/create",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!("session-1")),
                    ("interval".to_owned(), json!("30s")),
                    ("prompt".to_owned(), json!("scheduled prompt")),
                    ("nowSeconds".to_owned(), json!(10)),
                ]),
            )
            .expect("loop");
        let loop_id = created.result["loop"]["id"]
            .as_str()
            .expect("loop id")
            .to_owned();
        assert_eq!(loop_id.len(), 8);
        assert!(
            loop_id
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );

        let server = AppServer::with_release4_service(release4);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        let scheduled = server
            .reserve_due_loop("session-1", 40)
            .expect("scheduler")
            .expect("due loop");
        assert_eq!(scheduled.fire.loop_id, loop_id);
        assert_eq!(
            scheduled.fire.notice.params["entry"]["turnId"],
            scheduled.fire.notice.params["turnId"]
        );
        assert_eq!(scheduled.fire.notice.params["eventId"], 1);
        let DeferredWork::RunTurn {
            turn_id, prompt, ..
        } = scheduled.work
        else {
            return;
        };
        assert_eq!(prompt, "scheduled prompt");
        assert!(
            server
                .reserve_due_loop("session-1", 40)
                .expect("busy scheduler")
                .is_none()
        );
        server
            .fail_turn("session-1", &turn_id, "injected interruption")
            .expect("turn releases");
        server
            .finish_scheduled_loop(&loop_id, 41)
            .expect("loop reschedules");
        assert!(
            server
                .reserve_due_loop("session-1", 69)
                .expect("not yet due")
                .is_none()
        );
        assert!(
            server
                .reserve_due_loop("session-1", 70)
                .expect("due again")
                .is_some()
        );
    }

    #[test]
    fn deleting_a_saved_session_removes_its_loops_from_durable_restart_state() {
        let temporary = tempfile::tempdir().expect("session deletion stores");
        let session_root = temporary.path().join("sessions");
        let working_directory = temporary.path().join("workspace");
        let loop_path = temporary.path().join("loops.json");
        fs::create_dir_all(&working_directory).expect("workspace");
        vibe_core::storage::SessionStore::new(&session_root)
            .create(
                "deleted-session",
                &working_directory.to_string_lossy(),
                None,
                1,
            )
            .expect("saved session");
        let release3 =
            Release3Service::for_runtime_session_root(session_root, working_directory.clone());
        let release4 = Release4Service::default()
            .with_loop_store(loop_path.clone())
            .expect("loop store");
        release4
            .dispatch(
                "loops/create",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!("deleted-session")),
                    ("interval".to_owned(), json!("30s")),
                    ("prompt".to_owned(), json!("orphan check")),
                    ("nowSeconds".to_owned(), json!(10)),
                ]),
            )
            .expect("owned loop");
        let server = AppServer::with_release3_service(release3).using_release4_service(release4);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let deleted = connection.dispatch(&request(
            2,
            "session/delete",
            json!({"sessionId": "deleted-session"}),
        ));
        assert!(matches!(
            decode_frame(&deleted.outbound[0]).expect("delete response"),
            Envelope::Success(SuccessResponse { result, .. })
                if result.get("deleted").and_then(Value::as_bool) == Some(true)
        ));

        let restarted = Release4Service::default()
            .with_loop_store(loop_path)
            .expect("reloaded loop store");
        let listed = restarted
            .dispatch(
                "loops/list",
                &BTreeMap::from([("sessionId".to_owned(), json!("deleted-session"))]),
            )
            .expect("reloaded loops");
        assert_eq!(listed.result["loops"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn rewind_read_and_restore_use_live_target_specific_checkpoints() {
        let temporary = tempfile::tempdir().expect("rewind stores");
        let session_root = temporary.path().join("sessions");
        let working_directory = temporary.path().join("workspace");
        fs::create_dir_all(&working_directory).expect("workspace");
        fs::write(working_directory.join("main.txt"), "before\n").expect("workspace fixture");
        let store = vibe_core::storage::SessionStore::new(&session_root);
        let mut metadata = store
            .create(
                "source-session",
                &working_directory.to_string_lossy(),
                None,
                1,
            )
            .expect("source session");
        for (timestamp, content) in [(2, "first"), (3, "restore target"), (4, "latest")] {
            store
                .append_message(
                    &mut metadata,
                    &ModelMessage::User {
                        content: content.to_owned(),
                    },
                    timestamp,
                )
                .expect("user message");
        }
        let release3 = Release3Service::for_runtime_session_root(
            session_root.clone(),
            working_directory.clone(),
        );
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let started = connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "source-session", "resume": "source-session"}),
        ));
        assert!(matches!(
            decode_frame(&started.outbound[0]).expect("start response"),
            Envelope::Success(_)
        ));

        let review = server
            .lock_sessions()
            .expect("runtime sessions")
            .get("source-session")
            .and_then(|session| session.review.clone())
            .expect("live review manager");
        let first_turn = connection.dispatch(&request(
            3,
            "turn/start",
            json!({
                "sessionId": "source-session",
                "input": [{"type": "text", "text": "prior live turn"}]
            }),
        ));
        let first_turn_id = first_turn.deferred.iter().find_map(|work| match work {
            DeferredWork::RunTurn { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        });
        assert!(first_turn_id.is_some(), "first turn is reserved");
        let Some(first_turn_id) = first_turn_id else {
            return;
        };
        store
            .append_message(
                &mut metadata,
                &ModelMessage::User {
                    content: "prior live turn".to_owned(),
                },
                5,
            )
            .expect("persist first live user message");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::Assistant {
                    content: "prior answer".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                },
                6,
            )
            .expect("persist first live assistant message");
        server
            .fail_turn("source-session", &first_turn_id, "completed fixture turn")
            .expect("first turn seals");

        let checkpoint_turn = connection.dispatch(&request(
            4,
            "turn/start",
            json!({
                "sessionId": "source-session",
                "input": [{"type": "text", "text": "restore live target"}]
            }),
        ));
        let checkpoint_turn_id = checkpoint_turn.deferred.iter().find_map(|work| match work {
            DeferredWork::RunTurn { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        });
        assert!(checkpoint_turn_id.is_some(), "checkpoint turn is reserved");
        let Some(checkpoint_turn_id) = checkpoint_turn_id else {
            return;
        };
        review
            .edit(
                "main.txt",
                &[vibe_core::workspace::EditOperation {
                    old_text: "before".to_owned(),
                    new_text: "after".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("checkpoint edit");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::User {
                    content: "restore live target".to_owned(),
                },
                7,
            )
            .expect("persist checkpoint user message");
        server
            .fail_turn(
                "source-session",
                &checkpoint_turn_id,
                "completed checkpoint fixture",
            )
            .expect("checkpoint seals");

        let read = connection.dispatch(&request(
            5,
            "session/rewind/read",
            json!({"sessionId": "source-session"}),
        ));
        let decoded = decode_frame(&read.outbound[0]).expect("rewind read response");
        assert!(matches!(decoded, Envelope::Success(_)));
        let Envelope::Success(SuccessResponse { result, .. }) = decoded else {
            return;
        };
        let messages = result["messages"].as_array().expect("rewind targets");
        let live_target = messages
            .iter()
            .find(|message| message["messageIndex"] == 5)
            .expect("live target");
        assert_eq!(live_target["hasFileChanges"], true);
        assert_eq!(live_target["restorablePaths"], json!(["main.txt"]));

        let rejected = connection.dispatch(&request(
            6,
            "session/rewind",
            json!({
                "sessionId": "source-session",
                "messageIndex": 5,
                "restoreFiles": true,
                "inplace": "invalid"
            }),
        ));
        assert!(matches!(
            decode_frame(&rejected.outbound[0]).expect("rejected rewind"),
            Envelope::Error(_)
        ));
        assert_eq!(
            fs::read_to_string(working_directory.join("main.txt")).expect("rolled back workspace"),
            "after\n"
        );

        let rewound = connection.dispatch(&request(
            7,
            "session/rewind",
            json!({
                "sessionId": "source-session",
                "messageIndex": 5,
                "restoreFiles": true
            }),
        ));
        let decoded = decode_frame(&rewound.outbound[0]).expect("rewind response");
        assert!(matches!(decoded, Envelope::Success(_)));
        let Envelope::Success(SuccessResponse { result, .. }) = decoded else {
            return;
        };
        let child_id = result["metadata"]["session_id"]
            .as_str()
            .expect("branch id");
        assert_ne!(child_id, "source-session");
        assert_eq!(result["restoredPaths"], json!(["main.txt"]));
        assert_eq!(result["restoreErrors"], json!([]));
        assert_eq!(
            fs::read_to_string(working_directory.join("main.txt")).expect("restored workspace"),
            "before\n"
        );
        assert!(store.load("source-session").is_ok());
        assert!(server.session("source-session").is_ok());
        assert!(server.session(child_id).is_ok());
    }

    #[test]
    fn failed_rewind_attachment_rolls_back_session_and_workspace() {
        let temporary = tempfile::tempdir().expect("rewind rollback stores");
        let session_root = temporary.path().join("sessions");
        let working_directory = temporary.path().join("workspace");
        fs::create_dir_all(&working_directory).expect("workspace");
        fs::write(working_directory.join("main.txt"), "before\n").expect("workspace fixture");
        let store = vibe_core::storage::SessionStore::new(&session_root);
        let mut metadata = store
            .create(
                "source-session",
                &working_directory.to_string_lossy(),
                None,
                1,
            )
            .expect("source session");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::User {
                    content: "restore target".to_owned(),
                },
                2,
            )
            .expect("user message");
        let release3 = Release3Service::for_runtime_session_root(
            session_root.clone(),
            working_directory.clone(),
        );
        let server = AppServer::with_release3_service(release3)
            .using_session_tool_factory(Arc::new(RejectForkTools));
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "source-session", "resume": "source-session"}),
        ));
        let review = server
            .lock_sessions()
            .expect("runtime sessions")
            .get("source-session")
            .and_then(|session| session.review.clone())
            .expect("review manager");
        review.begin_turn_at("checkpoint", 0).expect("begin turn");
        review
            .edit(
                "main.txt",
                &[vibe_core::workspace::EditOperation {
                    old_text: "before".to_owned(),
                    new_text: "after".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");

        let rewound = connection.dispatch(&request(
            3,
            "session/rewind",
            json!({
                "sessionId": "source-session",
                "messageIndex": 0,
                "restoreFiles": true
            }),
        ));

        assert!(matches!(
            decode_frame(&rewound.outbound[0]).expect("rewind failure"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InternalError,
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            fs::read_to_string(working_directory.join("main.txt")).expect("rolled back workspace"),
            "after\n"
        );
        let saved = store.list(None, 0, 100).expect("saved sessions").sessions;
        assert_eq!(
            saved
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["source-session"]
        );
        assert!(server.session("source-session").is_ok());
    }

    #[test]
    fn failed_durable_session_delete_keeps_the_saved_session() {
        let temporary = tempfile::tempdir().expect("delete rollback stores");
        let session_root = temporary.path().join("sessions");
        let working_directory = temporary.path().join("workspace");
        let loop_path = temporary.path().join("loops.json");
        fs::create_dir_all(&working_directory).expect("workspace");
        let store = vibe_core::storage::SessionStore::new(&session_root);
        store
            .create(
                "retained-session",
                &working_directory.to_string_lossy(),
                None,
                1,
            )
            .expect("saved session");
        let release3 =
            Release3Service::for_runtime_session_root(session_root, working_directory.clone());
        let release4 = Release4Service::default()
            .with_loop_store(loop_path.clone())
            .expect("loop store");
        release4
            .dispatch(
                "loops/create",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!("retained-session")),
                    ("interval".to_owned(), json!("30s")),
                    ("prompt".to_owned(), json!("retain on failure")),
                    ("nowSeconds".to_owned(), json!(10)),
                ]),
            )
            .expect("owned loop");
        fs::remove_file(&loop_path).expect("remove loop file");
        fs::create_dir(&loop_path).expect("block loop persistence");
        let server = AppServer::with_release3_service(release3).using_release4_service(release4);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);

        let deleted = connection.dispatch(&request(
            2,
            "session/delete",
            json!({"sessionId": "retained-session"}),
        ));
        assert!(matches!(
            decode_frame(&deleted.outbound[0]).expect("delete failure"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InternalError,
                    ..
                },
                ..
            })
        ));
        assert!(store.load("retained-session").is_ok());
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

        let event_id_before_callback = server
            .lock_sessions()
            .expect("sessions")
            .get("session-1")
            .expect("session")
            .event_watermark;
        let (callback_id, callback_request) = connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "approve?",
            )
            .expect("callback request");
        assert_eq!(
            server
                .lock_sessions()
                .expect("sessions")
                .get("session-1")
                .expect("session")
                .event_watermark,
            event_id_before_callback + 1
        );
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
        assert_eq!(
            server
                .lock_sessions()
                .expect("sessions")
                .get("session-1")
                .expect("session")
                .event_watermark,
            event_id_before_callback + 2
        );
        let after = server.session("session-1").expect("resolved session");
        assert!(after.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.history.iter().any(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Callback {
                        callback_id,
                        metadata:
                            PublicEntryMetadata {
                                generation_status: PublicEntryGenerationStatus::Completed,
                                ..
                            },
                        state: PublicCallbackState::Answered { output },
                        ..
                    } if callback_id == "callback-1"
                        && output.pointer("/decision/type").and_then(Value::as_str)
                            == Some("approve")
                )
            })
        }));
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

        server
            .complete_turn(
                "session-1",
                "turn-1",
                ProjectionSnapshot {
                    session_id: "session-1".to_owned(),
                    turn_id: Some("turn-1".to_owned()),
                    watermark: 12,
                    lifecycle: LifecycleState::Completed,
                    title: Some("Retained title".to_owned()),
                    history: Vec::new(),
                },
            )
            .expect("terminal projection");
        let completed = server.session("session-1").expect("completed session");
        assert!(completed.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.title.as_deref() == Some("Retained title")
                && snapshot.history.iter().any(|entry| {
                    matches!(
                        entry,
                        PublicHistoryEntry::Callback {
                            callback_id,
                            state: PublicCallbackState::Answered { .. },
                            ..
                        } if callback_id == "callback-1"
                    )
                })
        }));
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
        assert!(session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.history.iter().any(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Callback {
                        metadata:
                            PublicEntryMetadata {
                                generation_status: PublicEntryGenerationStatus::Completed,
                                ..
                            },
                        state: PublicCallbackState::Cancelled { reason },
                        ..
                    } if reason == "Client did not accept callback delivery"
                )
            })
        }));

        let duplicate = connection.dispatch(&rejection);
        assert!(duplicate.close_after_flush);
        assert_eq!(connection.state(), ConnectionState::Closed);
    }

    #[test]
    fn cancelled_user_input_is_answered_without_interrupting_the_turn() {
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
                EngineCallbackKind::UserInput,
                "continue?",
            )
            .expect("callback request");
        let request_id = match decode_frame(&callback_request).expect("callback frame") {
            Envelope::Request(request) => request.id,
            _ => return,
        };
        let acknowledgement = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id,
            result: result_map([
                ("callbackId", json!(callback_id)),
                ("accepted", json!(true)),
            ]),
        }))
        .expect("callback acknowledgement");
        assert_eq!(
            connection.dispatch(&acknowledgement),
            DispatchBatch::empty()
        );

        let response = connection.dispatch(&request(
            4,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": "callback-1",
                "output": {
                    "type": "user_input",
                    "result": {"answers": [], "cancelled": true}
                }
            }),
        ));
        assert_eq!(
            response.deferred,
            vec![DeferredWork::ResolveCallback {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                callback_id: "callback-1".to_owned(),
                accepted: true,
                value: Some(
                    json!({
                        "type": "user_input",
                        "result": {"answers": [], "cancelled": true}
                    })
                    .to_string()
                ),
            }]
        );
        let session = server.session("session-1").expect("session");
        assert_eq!(session.status, SessionStatus::Running);
        assert!(session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.history.iter().any(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Callback {
                        metadata:
                            PublicEntryMetadata {
                                generation_status: PublicEntryGenerationStatus::Completed,
                                ..
                            },
                        state: PublicCallbackState::Answered { output },
                        ..
                    } if output.pointer("/result/cancelled").and_then(Value::as_bool)
                        == Some(true)
                )
            })
        }));
    }

    #[test]
    fn approval_denial_is_answered_and_only_cancel_turn_interrupts() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));

        connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "approve?",
            )
            .expect("denial callback");
        let denied = connection.dispatch(&request(
            4,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": "callback-1",
                "output": {
                    "type": "approval",
                    "decision": {"type": "deny"}
                }
            }),
        ));
        assert_eq!(
            denied.deferred,
            vec![DeferredWork::ResolveCallback {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                callback_id: "callback-1".to_owned(),
                accepted: true,
                value: Some(
                    json!({
                        "type": "approval",
                        "decision": {"type": "deny"}
                    })
                    .to_string()
                ),
            }]
        );
        let denied_session = server.session("session-1").expect("denied session");
        assert_eq!(denied_session.status, SessionStatus::Running);
        assert!(denied_session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.history.iter().any(|entry| {
                matches!(
                    entry,
                    PublicHistoryEntry::Callback {
                        callback_id,
                        state: PublicCallbackState::Answered { output },
                        ..
                    } if callback_id == "callback-1"
                        && output.pointer("/decision/type").and_then(Value::as_str)
                            == Some("deny")
                )
            })
        }));

        connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "cancel?",
            )
            .expect("cancel callback");
        let cancelled = connection.dispatch(&request(
            5,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": "callback-2",
                "output": {
                    "type": "approval",
                    "decision": {"type": "cancel_turn"}
                }
            }),
        ));
        assert_eq!(
            cancelled.deferred,
            vec![
                DeferredWork::ResolveCallback {
                    session_id: "session-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                    callback_id: "callback-2".to_owned(),
                    accepted: true,
                    value: Some(
                        json!({
                            "type": "approval",
                            "decision": {"type": "cancel_turn"}
                        })
                        .to_string()
                    ),
                },
                DeferredWork::InterruptTurn {
                    session_id: "session-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                },
            ]
        );
        assert!(
            server
                .session("session-1")
                .expect("session")
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.history.iter().any(|entry| {
                    matches!(
                        entry,
                        PublicHistoryEntry::Callback {
                            callback_id,
                            state: PublicCallbackState::Answered { .. },
                            ..
                        } if callback_id == "callback-2"
                    )
                }))
        );
    }

    #[test]
    fn answered_delivery_ignores_a_late_negative_acknowledgement() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let (_, first_delivery) = connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "first?",
            )
            .expect("first callback");
        let first_request_id = match decode_frame(&first_delivery).expect("callback delivery") {
            Envelope::Request(request) => request.id,
            _ => return,
        };
        connection.dispatch(&request(
            4,
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
        connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "second?",
            )
            .expect("second callback");

        let late_rejection = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: first_request_id,
            result: result_map([
                ("callbackId", json!("callback-1")),
                ("accepted", json!(false)),
            ]),
        }))
        .expect("late rejection");
        assert_eq!(connection.dispatch(&late_rejection), DispatchBatch::empty());
        assert_eq!(connection.state(), ConnectionState::Ready);
        assert_eq!(
            server
                .session("session-1")
                .expect("session")
                .pending_callback,
            Some("callback-2".to_owned())
        );
    }

    #[test]
    fn callback_requests_and_answers_are_validated_before_mutation() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let before = server.session("session-1").expect("session");
        for detail in [
            json!({
                "kind": "user_input",
                "request": {"questions": [], "footerNote": null},
                "relatedEntryId": null,
            }),
            json!({
                "kind": "user_input",
                "request": {
                    "questions": [{
                        "question": "continue?",
                        "options": [
                            {"label": "Yes"},
                            {"label": "No"}
                        ]
                    }],
                    "footerNote": "x".repeat(MAX_CALLBACK_REQUEST_BYTES),
                },
                "relatedEntryId": null,
            }),
        ] {
            assert!(matches!(
                connection.request_callback_with_detail(
                    "session-1",
                    "turn-1",
                    EngineCallbackKind::UserInput,
                    "continue?",
                    detail,
                ),
                Err(ServerError::InvalidCallbackDetail(_))
            ));
            assert_eq!(
                server.session("session-1").expect("unchanged session"),
                before
            );
        }

        connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::UserInput,
                "continue?",
            )
            .expect("valid callback");
        for (id, output) in [
            (
                4,
                json!({
                    "type": "user_input",
                    "result": {
                        "answers": [{
                            "question": "wrong question",
                            "answer": "Yes",
                            "isOther": false
                        }],
                        "cancelled": false
                    }
                }),
            ),
            (
                5,
                json!({
                    "type": "user_input",
                    "result": {
                        "answers": [{
                            "question": "continue?",
                            "answer": "Maybe",
                            "isOther": false
                        }],
                        "cancelled": false
                    }
                }),
            ),
        ] {
            let before_answer = server.session("session-1").expect("pending session");
            let rejected = connection.dispatch(&request(
                id,
                "callback/respond",
                json!({
                    "sessionId": "session-1",
                    "callbackId": "callback-1",
                    "output": output,
                }),
            ));
            assert!(matches!(
                decode_frame(&rejected.outbound[0]).expect("invalid response"),
                Envelope::Error(ErrorResponse {
                    error: ProtocolError {
                        code: ProtocolErrorCode::InvalidParams,
                        ..
                    },
                    ..
                })
            ));
            assert_eq!(
                server.session("session-1").expect("unchanged answer"),
                before_answer
            );
        }
    }

    #[test]
    fn final_handoff_preserves_prior_turn_history_and_rebinds_callbacks() {
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
            .complete_turn(
                "session-1",
                "turn-1",
                ProjectionSnapshot {
                    session_id: "session-1".to_owned(),
                    turn_id: Some("turn-1".to_owned()),
                    watermark: 1,
                    lifecycle: LifecycleState::Completed,
                    title: Some("Cumulative".to_owned()),
                    history: vec![message_entry(
                        "turn-1:entry-1",
                        "session-1",
                        "turn-1",
                        1,
                        "first",
                    )],
                },
            )
            .expect("first turn");
        connection.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "second"}]}),
        ));
        connection
            .request_callback(
                "session-1",
                "turn-2",
                EngineCallbackKind::Approval,
                "approve?",
            )
            .expect("second-turn callback");
        connection.dispatch(&request(
            5,
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
        server
            .complete_turn(
                "session-1",
                "turn-2",
                ProjectionSnapshot {
                    session_id: "session-2".to_owned(),
                    turn_id: Some("turn-2".to_owned()),
                    watermark: 2,
                    lifecycle: LifecycleState::Completed,
                    title: None,
                    history: vec![message_entry(
                        "turn-2:entry-1",
                        "session-2",
                        "turn-2",
                        2,
                        "second",
                    )],
                },
            )
            .expect("second turn handoff");
        let session = server.session("session-1").expect("old alias resolves");
        assert_eq!(session.id, "session-2");
        let snapshot = session.snapshot.expect("cumulative snapshot");
        assert_eq!(snapshot.title.as_deref(), Some("Cumulative"));
        assert_eq!(snapshot.history.len(), 3);
        assert!(
            snapshot
                .history
                .iter()
                .all(|entry| { entry.metadata().session_id == "session-2" })
        );
        assert!(["turn-1:entry-1", "turn-2:entry-1"].into_iter().all(|id| {
            snapshot
                .history
                .iter()
                .any(|entry| entry.metadata().id == id)
        }));
        assert!(snapshot.history.iter().any(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    state: PublicCallbackState::Answered { .. },
                    ..
                }
            )
        }));
    }

    #[test]
    fn malformed_callback_outputs_fail_closed_without_settling_state() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let (_, callback_request) = connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "approve?",
            )
            .expect("callback request");
        let request_id = match decode_frame(&callback_request).expect("callback frame") {
            Envelope::Request(request) => request.id,
            _ => return,
        };
        let acknowledgement = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id,
            result: result_map([
                ("callbackId", json!("callback-1")),
                ("accepted", json!(true)),
            ]),
        }))
        .expect("callback acknowledgement");
        assert_eq!(
            connection.dispatch(&acknowledgement),
            DispatchBatch::empty()
        );

        for (id, output) in [
            (
                4,
                json!({"type": "approval", "decision": {"type": "future_choice"}}),
            ),
            (
                5,
                json!({
                    "type": "user_input",
                    "result": {"answers": [], "cancelled": "false"}
                }),
            ),
            (
                6,
                json!({
                    "type": "user_input",
                    "result": {
                        "answers": [{
                            "question": "q",
                            "answer": "a",
                            "isOther": false,
                            "unexpected": true
                        }],
                        "cancelled": false
                    }
                }),
            ),
        ] {
            let before = server.session("session-1").expect("pending session");
            let batch = connection.dispatch(&request(
                id,
                "callback/respond",
                json!({
                    "sessionId": "session-1",
                    "callbackId": "callback-1",
                    "output": output,
                }),
            ));
            assert!(matches!(
                decode_frame(&batch.outbound[0]).expect("invalid-params response"),
                Envelope::Error(ErrorResponse {
                    error: ProtocolError {
                        code: ProtocolErrorCode::InvalidParams,
                        ..
                    },
                    ..
                })
            ));
            assert_eq!(
                server.session("session-1").expect("unchanged session"),
                before
            );
        }
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
        connection
            .detach_session("session-1")
            .expect("explicit detach succeeds");
        assert_eq!(
            server
                .session("session-1")
                .expect("explicitly detached view")
                .attachments,
            0
        );
        connection.close();
        assert_eq!(
            server
                .session("session-1")
                .expect("detached view")
                .attachments,
            0
        );
        let mut reattached = server.connect(TransportKind::InProcess);
        initialize(&mut reattached);
        reattached
            .attach_session("session-1")
            .expect("reattachment succeeds");
        let sessions = server.lock_sessions().expect("session state");
        let session = sessions.get("session-1").expect("reattached session");
        assert_eq!(session.attachments, 1);
        assert_eq!(session.resource_generation, 2);
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

    #[tokio::test]
    async fn operational_resources_are_typed_and_transport_failures_are_canonical() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);

        let tools =
            connection.dispatch(&request(3, "tools/list", json!({"sessionId": "session-1"})));
        assert!(matches!(
            decode_frame(&tools.outbound[0]).expect("tools response"),
            Envelope::Success(SuccessResponse { result, .. })
                if result["tools"].as_array().is_some_and(Vec::is_empty)
        ));

        let add = connection.dispatch(&request(
            4,
            "mcp/add",
            json!({
                "sessionId": "session-1",
                "url": "https://127.0.0.1:9/mcp",
                "name": "example"
            }),
        ));
        let deferred = add.deferred.first();
        assert!(matches!(
            deferred,
            Some(DeferredWork::ResourceRequest { .. })
        ));
        let Some(DeferredWork::ResourceRequest {
            request_id,
            session_id,
            command,
        }) = deferred
        else {
            return;
        };
        let add = server
            .execute_resource_request(request_id.clone(), session_id.clone(), command.clone())
            .await;
        assert!(matches!(
            decode_frame(&add.outbound[0]).expect("MCP response"),
            Envelope::Success(SuccessResponse { result, .. })
                if result["mcp"]["sources"][0]["status"] == json!("failed")
                    && result["diagnostics"][0]
                        .as_str()
                        .is_some_and(|message| message.contains("MCP `example`"))
        ));
        assert!(matches!(
            decode_frame(&add.outbound[1]).expect("MCP notification"),
            Envelope::Notification(Notification { method, .. }) if method == "mcp/updated"
        ));
    }

    #[tokio::test]
    async fn attached_resource_backend_uses_session_tools_and_returns_canonical_state() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = Arc::new(RecordingResourceBackend::default());
        let server = AppServer::with_resource_backend(backend.clone());
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let started = connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "session-1",
                "workingDirectory": workspace.path()
            }),
        ));
        assert_eq!(started.outbound.len(), 1);
        assert!(
            backend
                .opened_with_tools
                .lock()
                .expect("backend state")
                .is_some_and(|count| count > 0)
        );

        let add = connection.dispatch(&request(
            3,
            "mcp/add",
            json!({
                "sessionId": "session-1",
                "url": "https://mcp.example",
                "name": "example"
            }),
        ));
        assert!(add.outbound.is_empty());
        let deferred = add.deferred.first();
        assert!(matches!(
            deferred,
            Some(DeferredWork::ResourceRequest { .. })
        ));
        let Some(DeferredWork::ResourceRequest {
            request_id,
            session_id,
            command,
        }) = deferred
        else {
            return;
        };
        let added = server
            .execute_resource_request(request_id.clone(), session_id.clone(), command.clone())
            .await;
        assert_eq!(added.outbound.len(), 2);
        assert!(matches!(
            decode_frame(&added.outbound[0]).expect("response"),
            Envelope::Success(SuccessResponse {
                id: RequestId::Integer(3),
                ..
            })
        ));
        assert!(matches!(
            decode_frame(&added.outbound[1]).expect("notification"),
            Envelope::Notification(Notification { method, .. }) if method == "mcp/updated"
        ));

        let read = connection.dispatch(&request(4, "mcp/read", json!({"sessionId": "session-1"})));
        let deferred = read.deferred.first();
        assert!(matches!(
            deferred,
            Some(DeferredWork::ResourceRequest { .. })
        ));
        let Some(DeferredWork::ResourceRequest {
            request_id,
            session_id,
            command,
        }) = deferred
        else {
            return;
        };
        let read = server
            .execute_resource_request(request_id.clone(), session_id.clone(), command.clone())
            .await;
        assert!(matches!(
            decode_frame(&read.outbound[0]).expect("canonical state"),
            Envelope::Success(SuccessResponse { result, .. })
                if result["mcp"]["sources"] == json!(["example"])
        ));

        let close = connection.dispatch(&request(
            5,
            "session/close",
            json!({"sessionId": "session-1"}),
        ));
        let deferred = close.deferred.last();
        assert!(matches!(
            deferred,
            Some(DeferredWork::CloseResources { .. })
        ));
        let Some(DeferredWork::CloseResources {
            session_id,
            generation,
        }) = deferred
        else {
            return;
        };
        server
            .close_resource_session(session_id, *generation)
            .await
            .expect("resource cleanup");
        assert_eq!(
            *backend.closed.lock().expect("closed state"),
            vec!["session-1".to_owned()]
        );
    }

    #[test]
    fn trusted_session_start_schedules_typed_project_mcp_activation() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
        fs::create_dir_all(&vibe_home).expect("user config directory");
        fs::write(
            working_directory.join(".vibe/config.toml"),
            r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/fixture"
args = ["--stdio"]
startup_timeout_sec = 1
tool_timeout_sec = 2
"#,
        )
        .expect("project MCP config");
        let release3 = Release3Service::new(
            crate::release3::Release3Paths {
                vibe_home,
                working_directory: working_directory.clone(),
                session_root: temporary.path().join("sessions"),
            },
            Table::new(),
            true,
        )
        .expect("release-3 service");
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);

        let trusted = connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "trusted",
                "workingDirectory": working_directory,
                "trustWorkspace": true
            }),
        ));
        assert!(matches!(
            trusted.deferred.as_slice(),
            [DeferredWork::ConfigureMcp {
                session_id,
                configs
            }] if session_id == "trusted"
                && configs.len() == 1
                && configs[0].alias == "fixture"
                && configs[0].startup_timeout_ms == 1_000
                && configs[0].tool_timeout_ms == 2_000
        ));

        let untrusted = connection.dispatch(&request(
            3,
            "session/start",
            json!({
                "sessionId": "untrusted",
                "workingDirectory": temporary.path().join("workspace"),
                "trustWorkspace": false
            }),
        ));
        assert!(untrusted.deferred.is_empty());
    }

    #[test]
    fn session_start_hydrates_bounded_public_resume_history() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        let session_root = temporary.path().join("sessions");
        fs::create_dir_all(&working_directory).expect("working directory");
        let store = vibe_core::storage::SessionStore::new(&session_root);
        let mut metadata = store
            .create(
                "durable-session",
                &working_directory.to_string_lossy(),
                None,
                10,
            )
            .expect("durable session");
        for (timestamp, message) in [
            (
                11,
                ModelMessage::System {
                    content: "private system".to_owned(),
                },
            ),
            (
                12,
                ModelMessage::User {
                    content: "older question".to_owned(),
                },
            ),
            (
                13,
                ModelMessage::Assistant {
                    content: "older answer".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                },
            ),
            (
                14,
                ModelMessage::User {
                    content: "latest question".to_owned(),
                },
            ),
            (
                15,
                ModelMessage::Assistant {
                    content: "latest answer".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                },
            ),
        ] {
            store
                .append_message(&mut metadata, &message, timestamp)
                .expect("message persists");
        }
        let release3 = Release3Service::new(
            crate::release3::Release3Paths {
                vibe_home,
                working_directory,
                session_root,
            },
            Table::new(),
            false,
        )
        .expect("release-3 service");
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);

        let started = connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "durable-session",
                "resume": "durable-session",
                "historyLimit": 2
            }),
        ));
        let decoded = decode_frame(&started.outbound[0]).expect("start response");
        assert!(matches!(decoded, Envelope::Success(_)));
        let Envelope::Success(SuccessResponse { result, .. }) = decoded else {
            return;
        };
        let entries = result["state"]["history"]["entries"]
            .as_array()
            .expect("public history");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"][0]["text"], "latest question");
        assert_eq!(entries[1]["content"][0]["text"], "latest answer");
        assert!(
            entries
                .iter()
                .all(|entry| entry["sessionId"] == "durable-session")
        );
        let session = server.session("durable-session").expect("runtime session");
        assert_eq!(session.intent.resume.as_deref(), Some("durable-session"));
        assert!(!session.intent.continue_session);
        assert_eq!(
            session
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.history.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn workspace_trust_controls_the_session_tool_registry() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("visible.txt"), "safe\n").expect("fixture");
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let started = connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "session-1",
                "workingDirectory": workspace.path()
            }),
        ));
        assert_eq!(started.outbound.len(), 1);

        let invocation = || ToolInvocation {
            call_id: "read-1".to_owned(),
            arguments: json!({"path": "visible.txt"}),
        };
        assert!(
            server
                .invoke_tool("session-1", "read", invocation())
                .await
                .is_err()
        );

        let trusted = connection.dispatch(&request(
            3,
            "workspace/trust/decision",
            json!({
                "sessionId": "session-1",
                "cwd": workspace.path(),
                "decision": "trust_cwd"
            }),
        ));
        assert_eq!(trusted.outbound.len(), 2);
        assert_eq!(
            server
                .invoke_tool("session-1", "read", invocation())
                .await
                .expect("trusted read")
                .model_text,
            "1|safe"
        );

        connection.dispatch(&request(
            4,
            "workspace/trust/decision",
            json!({
                "sessionId": "session-1",
                "cwd": workspace.path(),
                "decision": "decline"
            }),
        ));
        assert!(
            server
                .invoke_tool("session-1", "read", invocation())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auto_approve_update_rebinds_existing_workspace_tool_handlers() {
        let workspace = tempfile::tempdir().expect("workspace");
        let file = workspace.path().join("editable.txt");
        std::fs::write(&file, "before\n").expect("fixture");
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let started = connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "session-1",
                "workingDirectory": workspace.path()
            }),
        ));
        assert_eq!(started.outbound.len(), 1);
        let invocation = |call_id: &str| ToolInvocation {
            call_id: call_id.to_owned(),
            arguments: json!({
                "path": "editable.txt",
                "oldText": "before",
                "newText": "after",
                "replaceAll": false
            }),
        };

        assert!(
            server
                .invoke_tool("session-1", "edit", invocation("edit-denied"))
                .await
                .is_err()
        );
        let updated = connection.dispatch(&request(
            3,
            "session/settings/update",
            json!({"sessionId": "session-1", "autoApprove": true}),
        ));
        assert!(matches!(
            decode_frame(&updated.outbound[0]).expect("settings response"),
            Envelope::Success(_)
        ));
        let rebound = server
            .invoke_tool("session-1", "edit", invocation("edit-approved"))
            .await
            .expect_err("edit still requires an active review turn");
        assert!(
            rebound
                .to_string()
                .contains("mutation requires an active turn"),
            "updated approval must reach the rebound edit handler: {rebound}"
        );
    }

    #[test]
    fn resource_requests_validate_session_ownership_and_idle_review_mutations() {
        let server = AppServer::default();
        let mut owner = server.connect(TransportKind::InProcess);
        initialize(&mut owner);
        start_session(&mut owner);

        let malformed = owner.dispatch(&request(3, "tools/list", json!({"sessionId": 7})));
        assert!(matches!(
            decode_frame(&malformed.outbound[0]).expect("invalid params"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ));

        owner.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "busy"}]}),
        ));
        let review = owner.dispatch(&request(
            5,
            "review/revert",
            json!({"sessionId": "session-1", "target": {"kind": "all"}}),
        ));
        assert!(matches!(
            decode_frame(&review.outbound[0]).expect("review conflict"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::Conflict,
                    ..
                },
                ..
            })
        ));

        let mut intruder = server.connect(TransportKind::InProcess);
        initialize(&mut intruder);
        let forbidden = intruder.dispatch(&request(
            6,
            "runtime/read",
            json!({"sessionId": "session-1"}),
        ));
        assert!(matches!(
            decode_frame(&forbidden.outbound[0]).expect("forbidden resource"),
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
            vec![
                DeferredWork::InterruptTurn {
                    session_id: "session-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                },
                DeferredWork::CloseResources {
                    session_id: "session-1".to_owned(),
                    generation: 1,
                },
            ]
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
        assert_eq!(
            server
                .session("session-1")
                .expect("source alias resolves to target runtime")
                .id,
            "session-2"
        );
        assert_eq!(
            server.session("session-2").expect("target runtime").id,
            "session-2"
        );
    }
}
