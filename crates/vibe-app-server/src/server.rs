use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod batch;
mod callbacks;
mod projection;
mod registry;
mod review;
mod session_management;

use batch::*;
use callbacks::*;
use projection::*;
use registry::SessionRegistry;

use crate::client::public_turn_failure;
use crate::client_tools::ClientToolBridge;
use crate::host::{now_millis, vibe_home};
use crate::release3::{RELEASE3_METHODS, Release3Error, Release3Service, RuntimeAttachment};
use crate::release4::{
    LoopFire, RELEASE4_METHODS, Release4Dispatch, Release4Error, Release4Service,
};
use crate::resources::{
    BACKEND_RESOURCE_METHODS, CoreResourceBackend, RESOURCE_METHODS, ResourceBackend,
    ResourceBackendCommand, ResourceBackendRequest, ResourceDispatch, ResourceError,
    ResourceService, ResourceSession, ResourceSignals,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::config::DotenvValues;
use vibe_core::events::{
    CallbackDetail, CallbackKind as EngineCallbackKind, CallbackOutput, EffectDetail,
    EffectResultDisplay, LifecycleState, ModelMessage, NoticeDetail, ProjectionSnapshot,
    PublicCallbackState, PublicContentBlock, PublicEffectState, PublicEntryGenerationStatus,
    PublicEntryMetadata, PublicError, PublicHistoryEntry, PublicMessageRole, PublicMessageSource,
    PublicTurn, PublicTurnStatus, TurnErrorCode,
};
use vibe_core::extensions::{AgentApproval, AgentProfile};
use vibe_core::integrations::redact;
use vibe_core::matching::NameFilter;
use vibe_core::mcp::McpServerConfig;
pub use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionRequirement,
    PolicyError,
};
use vibe_core::policy::{PermissionRule, PermissionStore, ToolGuard, TrustDecision, TrustRootKind};
use vibe_core::scratchpad::{cleanup_scratchpad, init_scratchpad, scratchpad_path};
use vibe_core::storage::HydratedSession;
pub use vibe_core::tools::builtins::{BuiltinTools, WebSearchAccess};
use vibe_core::tools::shell::{ShellRollout, ShellTools};
pub use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolInvocation,
    ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};
use vibe_protocol::{
    CallbackKind, ClientCapabilities, Envelope, ErrorResponse, InitializeParams,
    InitializeResponse, InvalidParamsData, InvalidParamsIssue, JsonRpcVersion, Notification,
    PathSegment, ProtocolError, ProtocolErrorCode, ProtocolVersion, RequestId, ServerCapabilities,
    ServerInfo, ServerRequest, SuccessResponse, TransportKind, decode_frame, encode_frame,
    is_dispatchable_method, is_server_method,
};

const INITIALIZE_METHOD: &str = "initialize";
const INITIALIZED_NOTIFICATION: &str = "initialized";
const SHUTDOWN_METHOD: &str = "shutdown";
const EXIT_NOTIFICATION: &str = "exit";
/// Where the managed shell rollout is read from, standing in for the reference
/// experiment variant that has no client in this port.
const MANAGED_SHELL_VARIABLE: &str = "VIBE_MANAGED_SHELL_TOOLS";
/// What a tool-surface diagnostic is attributed to: the tool filters and the
/// availability conditions both come from the configuration the session loaded.
pub(crate) const CONFIG_FILE_LABEL: &str = "config.toml";
const MAX_CALLBACK_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_ANSWERS: usize = 16;
const MAX_CALLBACK_OPTIONS: usize = 32;
const MAX_CALLBACK_TEXT_BYTES: usize = 8 * 1024;
/// Every notification name this build emits, sorted and unique.
///
/// [`notification_method`] gates each outbound name against this list, so it is
/// what the server actually sends rather than a hand-kept description of it. The
/// app-server surface replay reads it to report notification conformance, and
/// `docs/parity.md` records the names here that the reference does not declare.
pub const EMITTED_NOTIFICATIONS: &[&str] = &[
    "error",
    "history/entryAdded",
    "history/entryUpdated",
    "mcp/authUrl",
    "runtime/updated",
    "session/compacted",
    "session/contextCleared",
    "session/snapshot",
    "session/statsUpdated",
    "session/updated",
    "turn/completed",
    "turn/retrying",
    "turn/started",
    "vibeCode/teleport/event",
    "warning",
];

/// Which handoff notification a session rotation publishes, with the one field
/// that name adds to the shared handoff body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandoffNotice {
    Compacted { summary_length: usize },
    ContextCleared { plan_file_path: Option<String> },
}

/// Every method this build routes, sorted and unique, whether or not the
/// reference declares it.
pub(crate) fn routed_methods() -> BTreeSet<&'static str> {
    IMPLEMENTED_METHODS
        .iter()
        .chain(RELEASE3_METHODS)
        .chain(RELEASE4_METHODS)
        .chain(RESOURCE_METHODS)
        .copied()
        .collect()
}

/// What `initialize` advertises: the methods this build routes, minus the local
/// extensions.
///
/// A client written against the reference protocol must never learn a name only
/// this implementation answers, so [`LOCAL_EXTENSION_METHODS`] stays out even
/// though those methods are dispatched.
fn advertised_methods() -> Vec<String> {
    routed_methods()
        .into_iter()
        .filter(|method| is_server_method(method))
        .map(ToOwned::to_owned)
        .collect()
}

/// Gates a notification name on [`EMITTED_NOTIFICATIONS`] as it leaves.
///
/// Both notification funnels pass through here: the frames a dispatch batch
/// encodes and the frames the live projection builds. A name emitted without
/// being declared is a surface the conformance replay would never see.
pub(crate) fn notification_method(method: &str) -> &str {
    debug_assert!(
        EMITTED_NOTIFICATIONS.contains(&method),
        "{method} is emitted but absent from EMITTED_NOTIFICATIONS"
    );
    method
}

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
    "session/overrides/write",
    "session/read",
    "session/ready/read",
    "session/ready/wait",
    "session/settings/update",
    "session/start",
    "shell/interrupt",
    "shell/run",
    "stats/read",
    "telemetry/record",
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
    builtin_tools: Arc<BuiltinTools>,
    shell_tools: Arc<ShellTools>,
    /// The `clientTool/*` delegation this server's connection offers. Empty
    /// until a client declares a capability, which is what keeps a client that
    /// hosts nothing on the server's own filesystem and terminals.
    client_tools: Arc<ClientToolBridge>,
    next_session: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
    next_callback: Arc<AtomicU64>,
    next_entry: Arc<AtomicU64>,
}

impl Default for AppServer {
    fn default() -> Self {
        let home = vibe_home();
        Self {
            sessions: Arc::new(Mutex::new(SessionRegistry::default())),
            resources: Arc::new(Mutex::new(ResourceService::default())),
            resource_backend: Some(Arc::new(CoreResourceBackend::default())),
            release3: Arc::new(Release3Service::default()),
            release4: Arc::new(Release4Service::default()),
            approval_factory: Arc::new(DefaultApprovalFactory),
            session_tool_factory: Arc::new(NoAdditionalTools),
            // The reference resolves the web-search key from the environment or
            // the OS keyring. Only the environment branch is reachable from
            // here; a client holding a keyring credential installs it with
            // [`AppServer::using_web_search_access`]. The environment includes
            // `{vibe_home}/.env`, which the reference startup has folded into
            // the process by this point.
            builtin_tools: Arc::new(BuiltinTools::new(
                home.clone(),
                WebSearchAccess::from_environment(&DotenvValues::global(&home), "MISTRAL_API_KEY"),
            )),
            // The reference gates the managed shell family on a remote
            // experiment whose default variant is `legacy`. There is no
            // experiment client here, so the operator's environment is the
            // only thing that can ask for the managed variant.
            shell_tools: Arc::new(ShellTools::new(
                home,
                ShellRollout::from_environment(MANAGED_SHELL_VARIABLE),
            )),
            client_tools: Arc::new(ClientToolBridge::default()),
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

    /// Installs the credential `web_search` reaches the endpoint with, or
    /// withholds the tool when `None`.
    ///
    /// The reference publishes `web_search` only when a Mistral key resolves,
    /// and a client that read its key from the OS keyring is the only party
    /// that can hand it down.
    #[must_use]
    pub fn using_web_search_access(mut self, access: Option<WebSearchAccess>) -> Self {
        self.builtin_tools = Arc::new(self.builtin_tools.as_ref().clone().with_web_search(access));
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
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        self.complete_turn_with_stop_reason(session_id, turn_id, snapshot, None)
    }

    /// The frames a started turn puts on the wire, in emission order.
    ///
    /// `turn/started` first, then the `session/updated` that moves the session
    /// to `running`, matching the order the reference emits them in.
    pub fn turn_started(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
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
        session.stats.begin_turn();
        let event_id = next_event_id(session);
        let started = encode_notification(
            "turn/started",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session.id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        );
        Ok(vec![started, session_updated_frame(session)])
    }

    pub fn reserve_due_loop(
        &self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledLoopWork>, ServerError> {
        let (canonical_session_id, idle) = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(session_id)
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
        let session = sessions
            .get_mut(&canonical_session_id)
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
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        self.complete_turn_with_details(session_id, turn_id, snapshot, stop_reason, None)
    }

    /// The frames a settled turn puts on the wire, in emission order.
    ///
    /// The reference publishes the session's new status before the turn's own
    /// terminal notification, so a client that renders status from
    /// `session/updated` never shows a running session after the turn is gone.
    pub fn complete_turn_with_details(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
        stop_reason: Option<vibe_core::events::PublicTurnStopReason>,
        error: Option<PublicError>,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
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
                error.unwrap_or_else(|| {
                    public_turn_failure(TurnErrorCode::BackendError, "Turn failed")
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
        session.stats.last_turn_duration_ms = completed_at.saturating_sub(started_at);
        let stats = stats_updated_frame(session);
        let status = session_updated_frame(session);
        let event_id = next_event_id(session);
        let completed = encode_notification(
            "turn/completed",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(target_session_id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        );
        Ok(vec![stats, status, completed])
    }

    pub fn fail_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<Vec<Vec<u8>>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
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
            error: Some(public_turn_failure(code, message)),
            stop_reason: None,
        };
        session.latest_turn = Some(turn.clone());
        session.updated_at = turn.completed_at.unwrap_or(started_at);
        session.stats.last_turn_duration_ms = turn
            .completed_at
            .unwrap_or(started_at)
            .saturating_sub(started_at);
        let stats = stats_updated_frame(session);
        let status = session_updated_frame(session);
        let event_id = next_event_id(session);
        let completed = encode_notification(
            "turn/completed",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session_id)),
                ("turn", json!(turn)),
                ("emittedAt", json!(now_millis())),
            ]),
        );
        Ok(vec![stats, status, completed])
    }

    /// The effect an approval raised from a bare prompt presents.
    ///
    /// There is no tool behind it, so it publishes the generic kind and carries
    /// the prompt in the display's free-form content.
    fn approval_prompt_effect(prompt: &str) -> EffectDetail {
        let mut effect = EffectDetail::for_call("callback", &json!({}));
        effect.display.content = Some(prompt.to_owned());
        effect
    }

    pub fn request_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        prompt: impl Into<String>,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        if kind == EngineCallbackKind::ConnectorAuth {
            return Err(ServerError::UnsupportedCallbackKind(kind));
        }
        let prompt = prompt.into();
        let detail = match kind {
            EngineCallbackKind::Approval => json!({
                "kind": "approval",
                "effect": Self::approval_prompt_effect(&prompt),
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
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        if kind == EngineCallbackKind::ConnectorAuth {
            return Err(ServerError::UnsupportedCallbackKind(kind));
        }
        let title = title.into();
        let CallbackRequestDetail {
            detail,
            plan_review_path,
        } = parse_callback_request(kind, &title, &detail)
            .map_err(|message| ServerError::InvalidCallbackDetail(message.to_owned()))?;
        let related_entry_id = detail.related_entry_id().map(str::to_owned);
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
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
        let title_for_notice = title.clone();
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
        // The reference publishes a plan review as its own notice rather than as
        // a field on the callback, so the entry that names the plan lands ahead
        // of the question a client is about to be asked.
        if let Some(file_path) = plan_review_path {
            snapshot.history.push(PublicHistoryEntry::Notice {
                metadata: PublicEntryMetadata {
                    id: format!("notice:{callback_id}:plan-review"),
                    session_id: snapshot.session_id.clone(),
                    turn_id: Some(turn_id.to_owned()),
                    created_at: timestamp,
                    updated_at: timestamp,
                    generation_status: PublicEntryGenerationStatus::Completed,
                    related_entry_id: Some(format!("callback:{callback_id}")),
                },
                level: vibe_core::events::PublicNoticeLevel::Info,
                message: title_for_notice.clone(),
                detail: NoticeDetail::PlanReviewStarted { file_path },
            });
        }
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
        // The block is published before the callback is delivered, so a client
        // that renders status from `session/updated` is already showing the
        // session as blocked when the question arrives.
        let status = session_updated_frame(session);
        let request = encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: RequestId::Integer(i64::try_from(callback_sequence).unwrap_or(i64::MAX)),
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback))]),
        }));
        Ok((callback_id, vec![status, request]))
    }

    pub fn live_projection_seed(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<ProjectionSnapshot, ServerError> {
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(session_id)
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

    /// Records the usage one provider round trip reported and publishes it.
    ///
    /// The engine reports usage while the turn runs, which is what lets a client
    /// show context pressure before the turn settles rather than after.
    pub fn record_turn_stats(
        &self,
        session_id: &str,
        turn_id: &str,
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<Vec<u8>, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
        session
            .stats
            .observe(context_tokens, input_tokens, output_tokens);
        Ok(stats_updated_frame(session))
    }

    pub fn apply_live_projection(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
    ) -> Result<u64, ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
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

    /// Publishes the handoff of a session running a turn, under the name its
    /// cause earns.
    pub(crate) fn handoff_active_turn(
        &self,
        old_session_id: &str,
        new_session_id: &str,
        turn_id: &str,
        mut snapshot: ProjectionSnapshot,
        notice: &HandoffNotice,
        emitted_at: u64,
    ) -> Result<Vec<u8>, ServerError> {
        if snapshot.session_id != new_session_id {
            return Err(ServerError::SessionConflict(new_session_id.to_owned()));
        }
        // A reference projection validates a handoff by the two identifiers
        // differing; one that rotated onto its own name is a state change it
        // rejects rather than applies.
        if old_session_id == new_session_id {
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
        let (method, cause_field) = match notice {
            HandoffNotice::Compacted { summary_length } => (
                "session/compacted",
                ("summaryLength", json!(summary_length)),
            ),
            HandoffNotice::ContextCleared { plan_file_path } => (
                "session/contextCleared",
                ("planFilePath", json!(plan_file_path)),
            ),
        };
        Ok(encode_notification(
            method,
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(new_session_id)),
                ("oldSessionId", json!(old_session_id)),
                ("state", state),
                ("sessionLog", json!({"enabled": false})),
                cause_field,
                ("emittedAt", json!(emitted_at)),
            ]),
        ))
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
        let notification = encode_notification(
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
        );
        DispatchBatch {
            outbound: vec![success_bytes(request_id, result), notification],
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
    ) -> Result<(Vec<u8>, Vec<DeferredWork>), ServerError> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(&route.session_id)
            .ok_or_else(|| ServerError::SessionNotFound(route.session_id.clone()))?;
        if session.active_turn.as_deref() != Some(&route.turn_id) {
            return Err(ServerError::StaleTurn(route.turn_id.clone()));
        }
        let Some(callback) = &session.pending_callback else {
            return Ok((Vec::new(), Vec::new()));
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
        let status = session_updated_frame(session);
        Ok((
            status,
            vec![
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
            ],
        ))
    }

    pub fn session(&self, session_id: &str) -> Result<SessionView, ServerError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(session_id)
            .map(SessionView::from)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))
    }

    pub(crate) fn tool_registry(&self, session_id: &str) -> Result<ToolRegistry, ServerError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(session_id)
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
            sessions
                .get(session_id)
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
        let session_id = request.session_id.clone();
        let method = request.command.method();
        resource_result_batch(
            request_id,
            self,
            &session_id,
            method,
            backend.dispatch(request).await,
        )
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
        // A managed shell session outlives the call that started it, so this is
        // the only place left that can stop one. The same holds for a terminal
        // the client opened on our behalf. A failure in either must not skip the
        // backend teardown, so all three run and the first failure is reported.
        let shell = self
            .shell_tools
            .close_session(session_id)
            .await
            .map_err(|error| ServerError::Resource(error.to_string()));
        let delegated = self
            .client_tools
            .close_session(session_id)
            .await
            .map_err(|error| ServerError::Resource(error.to_string()));
        let backend = match &self.resource_backend {
            Some(backend) => backend
                .close_session(session_id, generation)
                .await
                .map_err(|error| ServerError::Resource(error.to_string())),
            None => Ok(()),
        };
        shell.and(delegated).and(backend)
    }

    /// Configures a session's MCP sources and publishes what changed.
    ///
    /// Discovery is best-effort: a source that will not start is reported as a
    /// `warning` rather than failing the session, which is why this answers with
    /// frames instead of a result.
    pub async fn configure_mcp_servers(
        &self,
        session_id: &str,
        configs: Vec<McpServerConfig>,
    ) -> Vec<Vec<u8>> {
        let warning = |message: String| ResourceSignals {
            runtime_updated: false,
            warnings: vec![message],
            auth_url: None,
            integrations: None,
        };
        let mut signals = match &self.resource_backend {
            None => warning("MCP transport backend is not configured".to_owned()),
            Some(backend) => match backend.configure_mcp(session_id, configs).await {
                Ok(dispatch) => dispatch.signals,
                Err(error) => warning(redact(&error.to_string())),
            },
        };
        if let Some(state) = signals.integrations.take()
            && let Ok(mut resources) = self.resources.lock()
        {
            resources.record_integrations(session_id, state);
        }
        signal_frames(self, session_id, &signals)
    }

    /// The session's runtime as `runtime/read` answers and `runtime/updated`
    /// publishes it, or `None` when the resource state cannot be read.
    ///
    /// Three owners contribute: the resource service holds the tool surface and
    /// the integrations, the release3 service holds the configuration and the
    /// catalogs, and the session itself holds its accounting. Composing here is
    /// what makes the answer live rather than a fixed payload, and it is the
    /// same composition the notification publishes.
    pub(crate) fn runtime_snapshot(&self, session_id: &str) -> Option<Value> {
        let mut snapshot = self.resources.lock().ok()?.runtime(session_id).ok()?;
        let (active_agent, stats, context_window) = match self.lock_sessions() {
            Ok(sessions) => {
                let session = sessions.get(session_id);
                (
                    session.and_then(|session| session.intent.agent.clone()),
                    public_stats(session),
                    session.map_or(0, |session| session.context_window),
                )
            }
            Err(_) => (None, public_stats(None), 0),
        };
        let projection = self.release3.runtime_projection(active_agent.as_deref());
        // Discovery issues and configuration diagnostics are the same fact to a
        // client: a file the session could not read cleanly.
        if let Some(Value::Array(issues)) = snapshot.get_mut("issues") {
            issues.extend(projection.issues);
        }
        snapshot.insert("config".to_owned(), projection.config);
        snapshot.insert("baseConfig".to_owned(), projection.base_config);
        snapshot.insert("activeAgent".to_owned(), projection.active_agent);
        snapshot.insert("agents".to_owned(), Value::Array(projection.agents));
        snapshot.insert("skills".to_owned(), Value::Array(projection.skills));
        snapshot.insert("hooksCount".to_owned(), json!(projection.hooks_count));
        snapshot.insert("stats".to_owned(), stats);
        snapshot.insert("contextWindow".to_owned(), json!(context_window));
        Some(Value::Object(snapshot))
    }

    /// How many images in the session's history the active model cannot read.
    ///
    /// A client shows this after a configuration change so the operator learns
    /// that switching model dropped what the transcript already carries. A model
    /// that reads images strips nothing, so the count is zero without walking
    /// the history.
    pub(crate) fn stripped_history_images(&self, session_id: &str) -> usize {
        if self.release3.active_model_supports_images() {
            return 0;
        }
        let Ok(sessions) = self.lock_sessions() else {
            return 0;
        };
        sessions
            .get(session_id)
            .and_then(|session| session.snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .history
                    .iter()
                    .filter_map(|entry| match entry {
                        PublicHistoryEntry::Message { content, .. } => Some(content),
                        _ => None,
                    })
                    .flatten()
                    .filter(|block| matches!(block, PublicContentBlock::Image { .. }))
                    .count()
            })
            .unwrap_or(0)
    }

    /// The session's logging state as `SessionLogSummary` declares it.
    ///
    /// A session the store never persisted reports its configured switch and
    /// nothing else, which is what a client renders as "not being written".
    pub(crate) fn session_log_summary(&self, session_id: &str) -> Value {
        let enabled = self.release3.session_logging_enabled();
        let Ok(sessions) = self.lock_sessions() else {
            return json!({
                "enabled": enabled,
                "sessionId": null,
                "persisted": false,
                "path": null,
                "title": null,
                "needsInitialAutoTitle": false,
            });
        };
        let session = sessions.get(session_id);
        let persisted = session.and_then(|session| session.persisted.as_ref());
        let title = session
            .and_then(|session| {
                session
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.title.clone())
            })
            .or_else(|| persisted.and_then(|hydrated| hydrated.metadata.title.clone()));
        json!({
            "enabled": enabled,
            "sessionId": persisted.map(|hydrated| hydrated.metadata.id.clone()),
            "persisted": persisted.is_some(),
            "path": persisted
                .map(|hydrated| hydrated.metadata.directory.clone())
                .filter(|directory| !directory.is_empty()),
            "title": title,
            // A persisted session whose title is still the one the store
            // generated is waiting for its first real one.
            "needsInitialAutoTitle": persisted.is_some_and(|hydrated| {
                hydrated.metadata.title.is_none() && hydrated.metadata.title_source == "auto"
            }),
        })
    }

    pub(crate) fn orphaned_resource_generation(
        &self,
        session_id: &str,
    ) -> Result<Option<u64>, ServerError> {
        let sessions = self.lock_sessions()?;
        Ok(sessions
            .get(session_id)
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
            session.agent_summary = attachment
                .agent_profile
                .as_ref()
                .map(crate::release3::agent_summary);
            if review_override.is_some() {
                session.review = review_override;
            }
            session.updated_at = now_millis();
            let session_id = session.id.clone();
            drop(sessions);
            return self.refresh_session_workspace_tools(&session_id);
        }
        let policy = PermissionStore::default()
            .with_tool_config(self.release3.tool_config())
            .with_allowlist_persistence(self.release3.allowlist_persistence());
        let tools = ToolRegistry::default();
        // A resumed session runs under the same configuration a fresh one does,
        // so its two filter lists are read again here rather than left empty.
        let (enabled_tools, disabled_tools) = self
            .release3
            .tool_filters_for_session(
                Path::new(&attachment.working_directory),
                matches!(
                    policy.try_trust_decision(&attachment.working_directory),
                    Ok(Some(TrustDecision::Trusted | TrustDecision::SessionTrusted))
                ),
            )
            .unwrap_or_default();
        let mut intent = SessionIntent {
            agent: attachment.agent.clone(),
            resume: Some(attachment.id.clone()),
            requested_enabled_tools: enabled_tools.clone(),
            requested_disabled_tools: disabled_tools.clone(),
            enabled_tools,
            disabled_tools,
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
        session.agent_summary = attachment
            .agent_profile
            .as_ref()
            .map(crate::release3::agent_summary);
        session.context_window = self.release3.context_window();
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

    /// Registers the builtin tool surface for a session root.
    ///
    /// The universal tools need nothing from the filesystem root and register
    /// first; the workspace family registers only when the root opens.
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
        let approval =
            self.approval_factory
                .for_agent(session_id, intent.approval, intent.auto_approve);
        // The delegation is resolved once per registration and handed to both
        // families, so the file tools and the shell agree on what this client
        // hosts for the session they are being published into.
        let client_io = self.client_tools.session_io(session_id);
        // Every family is handed the resolver rather than a snapshot of what it
        // currently answers, so a `tools.<name>` change observed between two
        // turns reaches the handlers without the surface being registered
        // again. The store narrows it with this session's permission
        // overrides, which is the one per-session part of the composition.
        //
        // The scratchpad opens with the session and is the one directory the
        // file tools reach without consulting a list, which is the capability
        // reference `init_scratchpad` gives the agent-loop runtime.
        let guard =
            ToolGuard::new(policy.clone(), approval).with_scratchpad(init_scratchpad(session_id));
        self.builtin_tools
            .register(
                session_id,
                Path::new(working_directory),
                intent.trusted,
                tools,
                &guard,
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        self.shell_tools
            .register(
                session_id,
                Path::new(working_directory),
                tools,
                client_io.clone(),
                &guard,
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let Ok(workspace) = Workspace::open(working_directory) else {
            return Ok(None);
        };
        let workspace = Arc::new(workspace);
        let review = review.unwrap_or_else(|| Arc::new(ReviewManager::new(workspace.clone())));
        WorkspaceTools::new(workspace, review.clone())
            .with_client_io(client_io)
            .register(tools, &guard)
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
        session.agent_summary = attachment
            .agent_profile
            .as_ref()
            .map(crate::release3::agent_summary);
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
        if let Ok(mut sessions) = self.server.lock_sessions() {
            for session_id in &self.attached_sessions {
                if let Some(session) = sessions.get_mut(session_id) {
                    session.attachments = session.attachments.saturating_sub(1);
                }
            }
        }
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
        if !is_dispatchable_method(&request.method) {
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
            "session/overrides/write" => self.session_overrides_write(request),
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.capabilities = params.capabilities;
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

    /// Reports what the session's tool surface could not honor: a filter entry
    /// that does not compile, and a registered tool whose runtime prerequisite
    /// does not hold.
    ///
    /// Both are published on `diagnostics/list` rather than failing the session:
    /// the reference drops an uncompilable pattern and withholds an unavailable
    /// tool, and neither is a reason to refuse to start.
    fn record_tool_surface_diagnostics(&self, intent: &SessionIntent, tools: &ToolRegistry) {
        let mut issues = Vec::new();
        for entry in NameFilter::new(&intent.enabled_tools).invalid() {
            issues.push(format!(
                "enabled_tools entry `{entry}` is not a valid regular expression and is ignored"
            ));
        }
        for entry in NameFilter::new(&intent.disabled_tools).invalid() {
            issues.push(format!(
                "disabled_tools entry `{entry}` is not a valid regular expression and is ignored"
            ));
        }
        let withheld = tools.withheld().unwrap_or_default();
        for name in withheld {
            issues.push(format!(
                "tool `{name}` is withheld: its runtime prerequisite is missing"
            ));
        }
        if issues.is_empty() {
            return;
        }
        if let Ok(mut resources) = self.server.resources.lock() {
            for issue in issues {
                resources.record_diagnostic(CONFIG_FILE_LABEL, &issue);
            }
        }
    }

    fn session_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionStartParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
        let (config_enabled_tools, config_disabled_tools) = match self
            .server
            .release3
            .tool_filters_for_session(Path::new(&working_directory), params.trusted)
        {
            Ok(filters) => filters,
            Err(error) => return release3_error_batch(request.id, error),
        };
        // Reference `_session_config_overrides`: an `enabled_tools` the client
        // sent replaces the configured allowlist, while `disabled_tools`
        // concatenates onto it.
        let enabled_tools = params.enabled_tools.unwrap_or(config_enabled_tools);
        let mut disabled_tools = config_disabled_tools;
        disabled_tools.extend(params.disabled_tools);
        disabled_tools.sort();
        disabled_tools.dedup();
        let mut intent = SessionIntent {
            add_directories: params.add_directories,
            trusted: params.trusted,
            agent: Some(selected_agent),
            tool_filters: params.tool_filters,
            enabled_tools,
            disabled_tools,
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
        let permission_store = PermissionStore::default()
            .with_tool_config(self.server.release3.tool_config())
            .with_allowlist_persistence(self.server.release3.allowlist_persistence());
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
        self.record_tool_surface_diagnostics(&intent, &tools);
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
        session.agent_summary = Some(crate::release3::agent_summary(&agent_profile));
        session.context_window = self.server.release3.context_window();
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
        drop(sessions);
        let mut batch = success_batch(request.id, result_map([("state", state)]));
        batch.outbound.extend(self.attachment_frames(&session_id));
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
        cancel_pending_callback(session, "Session was closed");
        let closed_status = session_updated_frame(session);
        if self.attached_sessions.remove(&key) {
            session.attachments = session.attachments.saturating_sub(1);
        }
        let session_id = canonical_session_id;
        let resource_generation = session.resource_generation;
        drop(sessions);
        if let Ok(mut resources) = self.server.resources.lock() {
            resources.close_session(&session_id);
        }
        // The scratchpad is a capability of the runtime, not of the workspace,
        // so it goes with the session that opened it. Reference
        // `cleanup_scratchpad` on the agent-loop shutdown path.
        cleanup_scratchpad(scratchpad_path(&session_id).as_path().into());
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
            outbound: vec![success_bytes(request.id, BTreeMap::new()), closed_status],
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.write_session_settings(request.id, params.into())
    }

    /// The local extension that writes what the reference settings method does
    /// not declare.
    fn session_overrides_write(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionOverridesWriteParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.write_session_settings(request.id, params.into())
    }

    /// Applies whichever settings the two methods carried, to the session and to
    /// its persisted runtime.
    fn write_session_settings(
        &mut self,
        request_id: RequestId,
        params: SessionSettings,
    ) -> DispatchBatch {
        if params.max_turns.is_none()
            && params.model.is_none()
            && params.max_tokens.is_none()
            && params.mode.is_none()
            && params.thinking.is_none()
            && params.reasoning_effort.is_none()
            && params.auto_approve.is_none()
        {
            return error_batch(
                request_id,
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
                request_id,
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
                request_id,
                ProtocolErrorCode::InvalidParams,
                "reasoningEffort must be low, medium, high, or max",
            );
        }
        if let Some(batch) = self.attachment_error(request_id.clone(), &params.session_id) {
            return batch;
        }
        let canonical_session_id = {
            let sessions = match self.server.lock_sessions() {
                Ok(sessions) => sessions,
                Err(error) => return internal_error_batch(request_id, &error),
            };
            let Some(session) = sessions.get(&params.session_id) else {
                return error_batch(
                    request_id,
                    ProtocolErrorCode::NotFound,
                    "Session was not found",
                );
            };
            if params.auto_approve.is_some() && session.active_turn.is_some() {
                return error_batch(
                    request_id,
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
            Err(error) => return release3_error_batch(request_id, error),
        };
        let mut sessions = match self.server.lock_sessions() {
            Ok(sessions) => sessions,
            Err(error) => return internal_error_batch(request_id, &error),
        };
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return error_batch(
                request_id,
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
            return internal_error_batch(request_id, &error);
        }
        success_batch(request_id, BTreeMap::new())
    }

    fn session_compact_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionCompactParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
            let Some(session) = sessions.get_mut(&params.session_id) else {
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
        let mut outbound = vec![success_bytes(
            request.id,
            result_map([("turn", json!(turn))]),
        )];
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
            outbound.push(encode_notification(&notice.method, notice.params));
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
            outbound: vec![success_bytes(
                request.id,
                result_map([("entries", json!([entry]))]),
            )],
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
        let status = session_updated_frame(session);
        DispatchBatch {
            outbound: vec![
                success_bytes(request.id, result_map([("interrupted", json!(true))])),
                status,
            ],
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
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
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
        // The answer is published as the union the reference declares, so a
        // body that passed the checks above but is not one of its two forms is
        // rejected rather than echoed back in the answered state.
        let output = match serde_json::from_value::<CallbackOutput>(params.output.clone()) {
            Ok(output) => output,
            Err(_) => {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::InvalidParams,
                    "Callback output does not match the protocol union",
                );
            }
        };
        let cancel_turn = callback_requests_turn_cancel(&params.output);
        let value = serde_json::to_string(&params.output).ok();
        settle_pending_callback(
            session,
            &params.callback_id,
            PublicCallbackState::Answered { output },
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
        let status = session_updated_frame(session);
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
            outbound: vec![success_bytes(request.id, result), status],
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
        // The three live-state reads are composed here rather than inside the
        // resource service: each one needs the configuration, the catalogs or
        // the session's own accounting, and the service holds none of them.
        if let Some(batch) = self.live_state_request(&request, &session_id) {
            return batch;
        }
        if request.method == "telemetry/record" {
            return self.telemetry_record(request);
        }
        let session_active = match self.server.lock_sessions() {
            Ok(sessions) => sessions
                .get(&session_id)
                .is_some_and(|session| session.active_turn.is_some()),
            Err(error) => return internal_error_batch(request.id, &error),
        };
        // The review surface is composed here for the same reason the live-state
        // reads are: all six of its methods answer from the session's checkpoint
        // engine, and the resource service holds no session.
        if review::is_review_method(&request.method) {
            let review = match self.server.lock_sessions() {
                Ok(sessions) => sessions
                    .get(&session_id)
                    .and_then(|session| session.review.clone()),
                Err(error) => return internal_error_batch(request.id, &error),
            };
            let result = review::dispatch(
                &request.method,
                &request.params,
                review.as_deref(),
                session_active,
            )
            .map(|result| ResourceDispatch {
                result,
                signals: ResourceSignals::default(),
            });
            return resource_result_batch(
                request.id,
                &self.server,
                &session_id,
                &request.method,
                result,
            );
        }
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
        resource_result_batch(
            request.id,
            &self.server,
            &session_id,
            &request.method,
            result,
        )
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
                result_map([("stats", stats), ("contextWindow", json!(context_window))])
            }
            "account/read" => result_map([("account", self.server.release3.account_view())]),
            _ => return None,
        };
        Some(success_batch(request.id.clone(), result))
    }

    /// Records a client-reported event against the attached session.
    ///
    /// The reference forwards the event to the agent loop's telemetry client,
    /// which ships it to the datalake under the reference envelope. This port
    /// publishes a deliberately different envelope from a closed vocabulary
    /// (`docs/parity.md`, Accepted divergences), so a client-authored name and
    /// its free-form properties have no envelope to travel in. The event is
    /// kept where an operator can read it back, on `diagnostics/logs/read`,
    /// and is dropped entirely when `enable_telemetry` is off, which is the
    /// decision the reference delegates to the same key.
    fn telemetry_record(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<TelemetryRecordParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => return invalid_params_batch(request.id, rejection),
        };
        if self.server.release3.telemetry_enabled() {
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
            match self.server.resources.lock() {
                Ok(mut resources) => resources.record_log(now_millis(), "INFO", &message),
                Err(_) => {
                    return error_batch(
                        request.id,
                        ProtocolErrorCode::InternalError,
                        "Resource state lock is poisoned",
                    );
                }
            }
        }
        success_batch(request.id, BTreeMap::new())
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
                outbound: vec![success_bytes(request_id, result)],
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
    /// The agent profile this session runs, projected as `AgentSummary`.
    ///
    /// The intent carries the name; a client renders the profile, so the
    /// summary is composed where the profile is resolved rather than looked up
    /// again at every projection.
    agent_summary: Option<Value>,
    event_watermark: u64,
    stats: SessionStats,
    /// The active model's compaction threshold, read once when the session
    /// opens. Zero means no model declares one.
    context_window: u64,
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
            agent_summary: None,
            attachments: 1,
            resource_generation: 1,
            aliases: BTreeSet::new(),
            created_at,
            updated_at: created_at,
            latest_turn: None,
            event_watermark: 0,
            stats: SessionStats::default(),
            context_window: 0,
            policy,
            tools,
            persisted: None,
            review,
        }
    }
}

/// The token and tool accounting one session publishes.
///
/// The reference keeps this on the agent loop and projects it into
/// `AgentStatsSnapshot`; here it lives on the session because the loop runs in
/// a driver the server does not own. The tool counters are derived from the
/// projected history rather than counted twice, so a replayed snapshot and a
/// live turn report the same numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SessionStats {
    session_prompt_tokens: u64,
    session_completion_tokens: u64,
    session_cached_tokens: u64,
    context_tokens: u64,
    last_turn_prompt_tokens: u64,
    last_turn_completion_tokens: u64,
    last_turn_cached_tokens: u64,
    last_turn_duration_ms: u64,
    /// Where the running turn started from, so its own usage is the difference.
    turn_baseline_prompt_tokens: u64,
    turn_baseline_completion_tokens: u64,
    turn_baseline_cached_tokens: u64,
}

impl SessionStats {
    /// Records the usage one provider round trip reported.
    fn observe(&mut self, context_tokens: u64, input_tokens: u64, output_tokens: u64) {
        self.context_tokens = context_tokens;
        self.session_prompt_tokens = input_tokens;
        self.session_completion_tokens = output_tokens;
        self.last_turn_prompt_tokens =
            input_tokens.saturating_sub(self.turn_baseline_prompt_tokens);
        self.last_turn_completion_tokens =
            output_tokens.saturating_sub(self.turn_baseline_completion_tokens);
        self.last_turn_cached_tokens = self
            .session_cached_tokens
            .saturating_sub(self.turn_baseline_cached_tokens);
    }

    /// Opens a turn: what follows counts against it rather than the session.
    fn begin_turn(&mut self) {
        self.turn_baseline_prompt_tokens = self.session_prompt_tokens;
        self.turn_baseline_completion_tokens = self.session_completion_tokens;
        self.turn_baseline_cached_tokens = self.session_cached_tokens;
        self.last_turn_prompt_tokens = 0;
        self.last_turn_completion_tokens = 0;
        self.last_turn_cached_tokens = 0;
        self.last_turn_duration_ms = 0;
    }
}

/// The 17-field snapshot `AgentStatsSnapshot` declares.
///
/// A session with no completed turn reports zeros rather than omitting the
/// last-turn fields, because a client renders them as numbers either way, and
/// so does a session the registry no longer holds.
fn public_stats(session: Option<&SessionRuntime>) -> Value {
    let history = session
        .and_then(|session| session.snapshot.as_ref())
        .map(|snapshot| snapshot.history.as_slice())
        .unwrap_or_default();
    let mut steps = 0_u64;
    let mut succeeded = 0_u64;
    let mut failed = 0_u64;
    let mut agreed = 0_u64;
    let mut rejected = 0_u64;
    for entry in history {
        match entry {
            PublicHistoryEntry::Message {
                role: PublicMessageRole::Assistant,
                ..
            } => steps = steps.saturating_add(1),
            PublicHistoryEntry::Effect { state, .. } => match state {
                PublicEffectState::Completed { .. } => succeeded = succeeded.saturating_add(1),
                PublicEffectState::Failed { .. } => failed = failed.saturating_add(1),
                _ => {}
            },
            PublicHistoryEntry::Callback { state, .. } => match state {
                PublicCallbackState::Answered { .. } => agreed = agreed.saturating_add(1),
                PublicCallbackState::Cancelled { .. } | PublicCallbackState::Expired { .. } => {
                    rejected = rejected.saturating_add(1);
                }
                PublicCallbackState::Open => {}
            },
            _ => {}
        }
    }
    let owned;
    let stats = match session {
        Some(session) => &session.stats,
        None => {
            owned = SessionStats::default();
            &owned
        }
    };
    let seconds = as_f64(stats.last_turn_duration_ms) / 1_000.0;
    let tokens_per_second = if seconds > 0.0 {
        as_f64(stats.last_turn_completion_tokens) / seconds
    } else {
        0.0
    };
    json!({
        "steps": steps,
        "sessionPromptTokens": stats.session_prompt_tokens,
        "sessionCompletionTokens": stats.session_completion_tokens,
        "sessionCachedTokens": stats.session_cached_tokens,
        "inputPricePerMillion": 0.0,
        "outputPricePerMillion": 0.0,
        "cachedInputPricePerMillion": null,
        "toolCallsAgreed": agreed,
        "toolCallsRejected": rejected,
        "toolCallsFailed": failed,
        "toolCallsSucceeded": succeeded,
        "contextTokens": stats.context_tokens,
        "lastTurnPromptTokens": stats.last_turn_prompt_tokens,
        "lastTurnCompletionTokens": stats.last_turn_completion_tokens,
        "lastTurnCachedTokens": stats.last_turn_cached_tokens,
        "lastTurnDuration": seconds,
        "tokensPerSecond": tokens_per_second,
    })
}

/// Widens a counter for the float fields the wire declares, saturating rather
/// than losing precision silently on a value no session reaches.
fn as_f64(value: u64) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

/// Publishes a fatal server-side problem as `error`.
///
/// The reference sends one before it drops a client whose background work
/// raised, so the client learns why the stream stopped instead of only that it
/// did.
pub(crate) fn server_error_frame(message: &str) -> Vec<u8> {
    encode_notification(
        "error",
        result_map([(
            "error",
            json!({"message": redact(message), "code": null, "details": null}),
        )]),
    )
}

/// Publishes the session's accounting as a sequenced `session/statsUpdated`.
fn stats_updated_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let stats = public_stats(Some(session));
    let context_window = session.context_window;
    let event_id = next_event_id(session);
    encode_notification(
        "session/statsUpdated",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("stats", stats),
            ("contextWindow", json!(context_window)),
            ("emittedAt", json!(now_millis())),
        ]),
    )
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

/// The event `telemetry/record` carries, exactly as the reference model
/// declares it: the session it belongs to, a client-authored name, free-form
/// properties and whether it correlates with the last backend request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TelemetryRecordParams {
    session_id: String,
    name: String,
    #[serde(default)]
    properties: BTreeMap<String, Value>,
    #[serde(default)]
    correlate_last_request: bool,
}

/// The settings `session/settings/update` changes, which is exactly what the
/// reference declares: the two turn budgets and nothing else.
///
/// A field the reference does not declare is refused here, including the five
/// this port lets a session override. Those moved to
/// [`SessionOverridesWriteParams`] under a local method name, so a client
/// written against the reference protocol sees this method behave as its own
/// model describes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionSettingsUpdateParams {
    session_id: String,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u64>,
}

/// What a session may override beyond the reference settings, under the local
/// method `session/overrides/write`.
///
/// Upstream none of these is session-scoped: the model and the thinking level
/// are configuration writes and the mode and the approval stance come from an
/// agent profile. This port lets a session hold them for its own lifetime,
/// which is what `vibe-cli` switches a model, a mode, a thinking level, a
/// reasoning effort and an approval stance through, and what `vibe-acp` maps
/// its session modes and config options onto. The name stays out of
/// `SERVER_METHODS` and out of the advertised capabilities, so it is offered to
/// nobody who did not already call it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionOverridesWriteParams {
    session_id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    thinking: Option<bool>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    auto_approve: Option<bool>,
}

/// The settings both methods write, whichever name carried them.
///
/// Splitting the wire shapes is what keeps the reference method exact; the
/// write itself is one operation on one session, so it stays one function.
#[derive(Debug, Default)]
struct SessionSettings {
    session_id: String,
    model: Option<String>,
    max_turns: Option<u32>,
    max_tokens: Option<u64>,
    mode: Option<String>,
    thinking: Option<bool>,
    reasoning_effort: Option<String>,
    auto_approve: Option<bool>,
}

impl From<SessionSettingsUpdateParams> for SessionSettings {
    fn from(params: SessionSettingsUpdateParams) -> Self {
        Self {
            session_id: params.session_id,
            max_turns: params.max_turns,
            max_tokens: params.max_tokens,
            ..Self::default()
        }
    }
}

impl From<SessionOverridesWriteParams> for SessionSettings {
    fn from(params: SessionOverridesWriteParams) -> Self {
        Self {
            session_id: params.session_id,
            model: params.model,
            mode: params.mode,
            thinking: params.thinking,
            reasoning_effort: params.reasoning_effort,
            auto_approve: params.auto_approve,
            ..Self::default()
        }
    }
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

/// Why a request's parameters were rejected, with the path to the value that
/// caused it.
///
/// Serde stops at the first violation, so `issues` carries one entry; the shape
/// is a list because the contract is a list and a client reads `errorCount`
/// rather than assuming.
pub(crate) struct ParamsRejection {
    message: String,
    issues: Vec<InvalidParamsIssue>,
}

impl ParamsRejection {
    /// A rejection a dispatcher raised by hand rather than through a
    /// deserializer, so there is no traversal to read a path from and the issue
    /// sits at the parameter object itself.
    ///
    /// The detail is still structured: a client reads `errorCount` and `issues`
    /// on every `invalid_params`, and an empty path says the failure is about
    /// the object rather than about a value inside it.
    fn at_root(message: String) -> Self {
        Self {
            issues: vec![InvalidParamsIssue {
                path: Vec::new(),
                message: message.clone(),
            }],
            message,
        }
    }
}

fn from_params<T: for<'de> Deserialize<'de>>(
    params: &BTreeMap<String, Value>,
) -> Result<T, ParamsRejection> {
    let value = Value::Object(
        params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error
            .path()
            .iter()
            .filter_map(|segment| match segment {
                serde_path_to_error::Segment::Seq { index } => Some(PathSegment::Index(*index)),
                serde_path_to_error::Segment::Map { key }
                | serde_path_to_error::Segment::Enum { variant: key } => {
                    Some(PathSegment::Field(key.clone()))
                }
                serde_path_to_error::Segment::Unknown => None,
            })
            .collect();
        let message = error.into_inner().to_string();
        ParamsRejection {
            issues: vec![InvalidParamsIssue {
                path,
                message: message.clone(),
            }],
            message,
        }
    })
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

/// Publishes the session's status as a sequenced `session/updated`.
///
/// Every server-side status transition mints one. The client's projection reads
/// the per-session `eventId` as a contiguous counter, so a transition that
/// advanced the watermark without publishing anything would read to it as a gap
/// rather than as silence.
fn session_updated_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let status = public_session_status(session);
    let updated_at = session.updated_at;
    let event_id = next_event_id(session);
    encode_notification(
        "session/updated",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            (
                "patch",
                json!([
                    {"op": "replace", "path": "/status", "value": status},
                    {"op": "replace", "path": "/updatedAt", "value": updated_at},
                ]),
            ),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// Publishes the whole session as a sequenced `session/snapshot`.
///
/// The watermark is advanced before the state is projected so the embedded
/// `state.eventId` equals the notification's own, which the client's projection
/// asserts before it replaces its state.
fn session_snapshot_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let event_id = next_event_id(session);
    let state = public_session_state(session);
    encode_notification(
        "session/snapshot",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("state", state),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// The outbound frames a rejected callback delivery produces.
///
/// [`AppServer::reject_callback`] answers with an empty frame when no callback
/// was pending, because there was no transition to publish.
fn rejection_frames(status: Vec<u8>) -> Vec<Vec<u8>> {
    if status.is_empty() {
        Vec::new()
    } else {
        vec![status]
    }
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
    use vibe_core::events::ApprovalDecisionType;

    /// `SERVER_METHODS` is the reference contract, not this build's routing
    /// table: a name belongs there whether or not this build answers it. What
    /// stays enforced here is the other direction, that nothing is routed
    /// outside the contract plus the declared local extensions, and that the
    /// advertised set is the routed reference subset.
    ///
    /// Which reference methods are still unrouted is a moving backlog, so it is
    /// tracked where it is measured, in `app_server_surface_parity_tests`.
    #[test]
    fn every_routed_method_is_declared_or_a_local_extension() {
        let routed = routed_methods();
        let undeclared = routed
            .iter()
            .filter(|method| !is_dispatchable_method(method))
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            undeclared,
            Vec::<&str>::new(),
            "routed but neither declared by the reference nor a local extension"
        );

        let advertised = advertised_methods();
        for method in vibe_protocol::LOCAL_EXTENSION_METHODS {
            assert!(
                routed.contains(method),
                "{method} is a local extension but is routed nowhere"
            );
            assert!(
                !advertised.contains(&method.to_owned()),
                "{method} is a local extension and must not be advertised"
            );
        }
        assert!(advertised.iter().all(|method| is_server_method(method)));
        assert_eq!(
            advertised.len(),
            routed.len() - vibe_protocol::LOCAL_EXTENSION_METHODS.len(),
            "the advertised set is the routed methods minus the local extensions"
        );
    }

    /// A client library written against the reference protocol always may send
    /// `disabledNotifications`. Rejecting it made the port unreachable for every
    /// such client, since no second frame is ever sent after a failed
    /// `initialize`.
    #[test]
    fn the_handshake_accepts_the_reference_capability_set() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        let response = initialize_with(
            &mut connection,
            json!({
                "callbackKinds": ["approval"],
                "clientTools": ["filesystem/read"],
                "disabledNotifications": ["runtime/updated"]
            }),
        );
        assert_eq!(connection.state(), ConnectionState::Ready);

        let advertised = response["capabilities"]["methods"]
            .as_array()
            .expect("the handshake advertises a method list")
            .iter()
            .filter_map(|method| method.as_str())
            .collect::<BTreeSet<_>>();
        for method in vibe_protocol::LOCAL_EXTENSION_METHODS {
            assert!(
                !advertised.contains(method),
                "{method} is a local extension and must stay unadvertised"
            );
        }
        assert!(
            advertised.iter().all(|method| is_server_method(method)),
            "the handshake advertises a name the reference does not declare"
        );

        // A capability the reference does not declare still fails, which is
        // what keeps `deny_unknown_fields` discriminating the envelope.
        let mut fresh = server.connect(TransportKind::InProcess);
        let rejected = fresh.dispatch(&request(
            1,
            "initialize",
            json!({
                "clientInfo": {"name": "test", "version": "1"},
                "capabilities": {"invented": true}
            }),
        ));
        assert!(matches!(
            decode_frame(&rejected.outbound[0]).expect("rejection"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ));
    }

    /// The mute list silences a notification the client does not want, and stops
    /// at the sequenced event stream: dropping one of those would open a gap the
    /// client's own projection reads as a fault.
    #[test]
    fn a_muted_notification_is_dropped_and_a_sequenced_event_is_not() {
        let workspace = tempfile::tempdir().expect("workspace");
        let open_session = |connection: &mut ServerConnection| {
            let started = connection.dispatch(&request(
                2,
                "session/start",
                json!({"sessionId": "session-1", "workingDirectory": workspace.path()}),
            ));
            // The answer, then the snapshot the attachment publishes.
            assert_eq!(started.outbound.len(), 2);
        };
        let trust = |connection: &mut ServerConnection| {
            connection.dispatch(&request(
                3,
                "workspace/trust/decision",
                json!({
                    "sessionId": "session-1",
                    "cwd": workspace.path(),
                    "decision": "trust_cwd"
                }),
            ))
        };

        let server = AppServer::default();
        let mut listening = server.connect(TransportKind::InProcess);
        initialize_with(&mut listening, json!({"callbackKinds": ["approval"]}));
        open_session(&mut listening);
        assert_eq!(
            trust(&mut listening).outbound.len(),
            2,
            "an unmuted client receives the response and the notification"
        );

        let server = AppServer::default();
        let mut muted = server.connect(TransportKind::InProcess);
        initialize_with(
            &mut muted,
            json!({
                "callbackKinds": ["approval"],
                "disabledNotifications": ["runtime/updated"]
            }),
        );
        open_session(&mut muted);
        let batch = trust(&mut muted);
        assert_eq!(
            batch.outbound.len(),
            1,
            "a muted client receives the response alone"
        );
        assert!(matches!(
            decode_frame(&batch.outbound[0]).expect("trust answer"),
            Envelope::Success(_)
        ));

        // The mute consumed no event id, so the sequence still runs on from the
        // snapshot the attachment published.
        muted.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let started = server
            .turn_started("session-1", "turn-1")
            .expect("the turn starts");
        assert!(matches!(
            decode_frame(&started[0]).expect("started notification"),
            Envelope::Notification(Notification { ref params, .. })
                if params["eventId"] == json!(2)
        ));
        assert!(
            muted.delivers(&started[0]),
            "a sequenced event is delivered even when its name is muted"
        );
    }

    /// The status a `session/updated` publishes is the one a client renders,
    /// so each transition has to name what it is waiting on or what broke.
    #[test]
    fn session_updated_names_the_turn_the_callback_and_the_failure() {
        let patch_status = |frame: &[u8]| -> Value {
            match decode_frame(frame).expect("status notification") {
                Envelope::Notification(Notification { method, params, .. }) => {
                    assert_eq!(method, "session/updated");
                    assert_eq!(params["sessionId"], json!("session-1"));
                    assert!(params["emittedAt"].is_u64(), "the status is timestamped");
                    assert_eq!(params["patch"][1]["path"], json!("/updatedAt"));
                    assert_eq!(params["patch"][0]["path"], json!("/status"));
                    params["patch"][0]["value"].clone()
                }
                other => unreachable!("expected a status notification: {other:?}"),
            }
        };

        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let started = server
            .turn_started("session-1", "turn-1")
            .expect("the turn starts");
        assert_eq!(
            patch_status(&started[1]),
            json!({"type": "running", "activeTurnId": "turn-1"})
        );

        let (callback_id, delivery) = connection
            .request_callback(
                "session-1",
                "turn-1",
                EngineCallbackKind::Approval,
                "May I run this?",
            )
            .expect("the callback is delivered");
        assert_eq!(
            patch_status(&delivery[0]),
            json!({
                "type": "blocked",
                "activeTurnId": "turn-1",
                "callbackId": callback_id,
                "reason": "approval",
            })
        );

        // A client attaching now is handed the state and the question still
        // open on it, so it can answer a callback raised before it arrived.
        let mut arriving = server.connect(TransportKind::InProcess);
        initialize_with(&mut arriving, json!({"callbackKinds": ["approval"]}));
        let attachment = arriving.attachment_frames("session-1");
        assert!(matches!(
            decode_frame(&attachment[0]).expect("snapshot"),
            Envelope::Notification(Notification { ref method, ref params, .. })
                if method == "session/snapshot"
                    && params["state"]["eventId"] == params["eventId"]
        ));
        assert!(matches!(
            decode_frame(&attachment[1]).expect("redelivered callback"),
            Envelope::Request(ServerRequest { ref method, ref params, .. })
                if method == "callback/call"
                    && params["callback"]["callbackId"] == json!(callback_id)
        ));

        let failed = server
            .fail_turn(
                "session-1",
                "turn-1",
                "the provider refused",
                TurnErrorCode::Refusal,
            )
            .expect("the turn fails");
        assert_eq!(
            patch_status(&failed[1]),
            json!({"type": "failed", "message": "the provider refused"})
        );
    }

    /// Usage reported mid-turn is pushed as it arrives and lands on the session
    /// a client reads, so context pressure is visible before the turn settles.
    #[test]
    fn stats_updated_carries_the_whole_snapshot_and_the_session_token_usage() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        server
            .turn_started("session-1", "turn-1")
            .expect("the turn starts");
        let frame = server
            .record_turn_stats("session-1", "turn-1", 1_200, 900, 300)
            .expect("the usage is recorded");
        let Envelope::Notification(Notification { method, params, .. }) =
            decode_frame(&frame).expect("stats notification")
        else {
            unreachable!("the usage is published as a notification");
        };
        assert_eq!(method, "session/statsUpdated");
        assert_eq!(params["sessionId"], json!("session-1"));
        assert!(params["eventId"].as_u64().is_some_and(|id| id > 0));
        assert!(params["emittedAt"].is_u64());
        assert!(params["contextWindow"].is_u64(), "a threshold is published");
        assert_eq!(
            params["stats"]
                .as_object()
                .expect("the snapshot is an object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "cachedInputPricePerMillion",
                "contextTokens",
                "inputPricePerMillion",
                "lastTurnCachedTokens",
                "lastTurnCompletionTokens",
                "lastTurnDuration",
                "lastTurnPromptTokens",
                "outputPricePerMillion",
                "sessionCachedTokens",
                "sessionCompletionTokens",
                "sessionPromptTokens",
                "steps",
                "tokensPerSecond",
                "toolCallsAgreed",
                "toolCallsFailed",
                "toolCallsRejected",
                "toolCallsSucceeded",
            ],
            "the reference declares seventeen fields"
        );
        assert_eq!(params["stats"]["contextTokens"], json!(1_200));
        assert_eq!(params["stats"]["sessionPromptTokens"], json!(900));
        // A session with no completed turn reports zeroes rather than absences.
        assert_eq!(params["stats"]["lastTurnDuration"], json!(0.0));

        let read = connection.dispatch(&request(
            4,
            "session/read",
            json!({"sessionId": "session-1"}),
        ));
        let Envelope::Success(SuccessResponse { result, .. }) =
            decode_frame(&read.outbound[0]).expect("session state")
        else {
            unreachable!("session/read answers");
        };
        assert_eq!(
            result["state"]["session"]["tokenUsage"],
            json!({"inputTokens": 900, "outputTokens": 300, "totalTokens": 1_200})
        );
    }

    /// US-090: the single call a client renders a session from reports what the
    /// session is actually running, not a fixed payload.
    #[test]
    fn runtime_read_reports_the_live_catalogs_configuration_and_accounting() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        let runtime = call(&mut connection, 10, "runtime/read")["runtime"].clone();

        // The catalogs are the ones the dedicated calls publish, rather than
        // the empty lists this method used to answer with.
        let agents = call(&mut connection, 11, "agents/list");
        let skills = call(&mut connection, 12, "skills/list");
        assert_eq!(runtime["agents"], agents["agents"]);
        assert_eq!(runtime["skills"], skills["skills"]);
        assert_eq!(runtime["activeAgent"], agents["active"]);
        assert!(
            runtime["agents"]
                .as_array()
                .is_some_and(|agents| !agents.is_empty()),
            "the shipped profiles are published: {runtime}"
        );
        assert_eq!(
            runtime["tools"],
            call(&mut connection, 13, "tools/list")["tools"]
        );

        // The accounting and the threshold are the session's own, and the same
        // pair `stats/read` answers with.
        let stats = call(&mut connection, 14, "stats/read");
        assert_eq!(runtime["stats"], stats["stats"]);
        assert_eq!(runtime["contextWindow"], stats["contextWindow"]);
        assert_eq!(
            runtime["stats"]
                .as_object()
                .expect("the snapshot is an object")
                .len(),
            17,
            "the live snapshot is the one the notification carries"
        );

        // The configuration is a real view rather than an empty document, and
        // the hook count is counted rather than hard-coded.
        assert_eq!(runtime["config"], runtime["baseConfig"]);
        assert!(
            runtime["config"]["activeModel"]["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty()),
            "the active model is named: {}",
            runtime["config"]
        );
        assert!(runtime["config"]["transcribeModels"].is_array());
        assert!(runtime["hooksCount"].is_u64());
        assert!(runtime["issues"].is_array());
    }

    /// US-093: the published session names the model and the agent it runs, so
    /// a client renders them without a second call.
    #[test]
    fn the_published_session_names_its_model_and_agent() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        let batch = connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "session-1",
                "workingDirectory": "/workspace",
                "model": "devstral-small",
                "agent": "plan"
            }),
        ));
        assert_eq!(batch.outbound.len(), 2);
        let session = call(&mut connection, 10, "session/read")["state"]["session"].clone();
        assert_eq!(session["model"], json!("devstral-small"));
        assert_eq!(
            session["agent"],
            json!({
                "name": "plan",
                "displayName": "Plan",
                "description": "Read-only agent for exploration and planning",
                "safety": "safe",
                "agentType": "agent",
            })
        );
        // The catalog names the same agent as the one the session runs, rather
        // than the one a fresh session would.
        assert_eq!(
            call(&mut connection, 11, "agents/list")["active"],
            session["agent"]
        );
    }

    /// US-091: the configuration answers carry the two views and the runtime the
    /// reference declares, and nothing else.
    ///
    /// The server runs against a temporary home: the patch below writes a file,
    /// and a test must never write the operator's own configuration.
    #[test]
    fn the_configuration_envelopes_are_the_reference_shapes() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let release3 = Release3Service::new(
            crate::release3::Release3Paths {
                vibe_home: temporary.path().join("home"),
                working_directory: temporary.path().join("workspace"),
                session_root: temporary.path().join("sessions"),
            },
            false,
        )
        .expect("release-3 service");
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);

        let read = call(&mut connection, 10, "config/read");
        assert_eq!(
            read.keys().map(String::as_str).collect::<Vec<_>>(),
            ["baseConfig", "config", "strippedHistoryImages"]
        );
        assert_eq!(read["config"], read["baseConfig"]);
        assert_eq!(
            read["config"]
                .as_object()
                .expect("the view is an object")
                .len(),
            18,
            "the view is the whole `ConfigView`"
        );
        assert!(read["strippedHistoryImages"].is_u64());

        // A reload publishes the runtime rather than the views, which is what
        // separates the two shapes upstream.
        let reload = call(&mut connection, 11, "config/reload");
        assert_eq!(
            reload.keys().map(String::as_str).collect::<Vec<_>>(),
            ["runtime", "strippedHistoryImages"]
        );
        assert!(reload["runtime"]["config"]["activeModel"].is_object());

        let patched = connection.dispatch(&request(
            12,
            "config/patch",
            json!({
                "sessionId": "session-1",
                "ops": [{"op": "set", "path": "/theme", "value": "nord"}],
            }),
        ));
        let Envelope::Success(SuccessResponse { result, .. }) =
            decode_frame(&patched.outbound[0]).expect("patch answer")
        else {
            unreachable!("config/patch answers");
        };
        assert_eq!(
            result.keys().map(String::as_str).collect::<Vec<_>>(),
            ["failures", "rejected", "runtime", "strippedHistoryImages"]
        );
        assert_eq!(result["rejected"], json!(false));
        assert_eq!(result["failures"], json!([]));
        assert_eq!(result["runtime"]["config"]["theme"], json!("nord"));
    }

    /// US-090: the session's logging state, rather than a fixed disabled
    /// summary, across the six fields the wire declares.
    #[test]
    fn runtime_read_reports_the_session_log_summary() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        let answer = call(&mut connection, 10, "runtime/read");
        let log = &answer["sessionLog"];
        assert_eq!(
            log.as_object()
                .expect("the summary is an object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "enabled",
                "needsInitialAutoTitle",
                "path",
                "persisted",
                "sessionId",
                "title",
            ]
        );
        // The default configuration writes sessions, so the switch is reported
        // as on even though this in-memory session is not persisted.
        assert_eq!(log["enabled"], json!(true));
        assert_eq!(log["persisted"], json!(false));
        assert_eq!(log["sessionId"], Value::Null);
        assert_eq!(answer["ready"], json!(true));
    }

    /// US-090: the account is classified from the credential the session runs
    /// under, so a configured key is never reported as missing.
    #[test]
    fn account_read_classifies_the_configured_credential() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        let account = call(&mut connection, 10, "account/read")["account"].clone();
        let status = account["status"].as_str().expect("a status is published");
        assert!(
            ["ready", "missing_key", "unauthorized", "unavailable"].contains(&status),
            "{status} is outside the account vocabulary"
        );
        // The default configuration serves a Mistral model, so the answer turns
        // on whether a key resolves rather than being fixed.
        let expected = if std::env::var("MISTRAL_API_KEY").is_ok_and(|key| !key.trim().is_empty()) {
            "ready"
        } else {
            "missing_key"
        };
        assert_eq!(status, expected);
        assert_eq!(account["teleportAction"]["kind"], json!("upgrade_to_pro"));
    }

    /// The local extensions keep answering the clients already calling them,
    /// while staying outside the advertised contract.
    #[test]
    fn a_local_extension_stays_routable() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        for method in vibe_protocol::LOCAL_EXTENSION_METHODS {
            let batch = connection.dispatch(&request(9, method, json!({"sessionId": "session-1"})));
            let frame = decode_frame(&batch.outbound[0]).expect("extension answer");
            if let Envelope::Error(ErrorResponse { error, .. }) = frame {
                assert_ne!(
                    error.code,
                    ProtocolErrorCode::MethodNotFound,
                    "{method} is routed nowhere: {}",
                    error.message
                );
            }
        }
    }

    /// A client that sent the wrong shape has to be told which value was wrong,
    /// not merely that something was.
    #[test]
    fn invalid_params_names_the_offending_value() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        let batch = connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": 7}]}),
        ));
        let Envelope::Error(ErrorResponse { error, .. }) =
            decode_frame(&batch.outbound[0]).expect("rejection")
        else {
            unreachable!("a malformed turn input was accepted");
        };
        assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
        assert_eq!(error.data["errorCount"], json!(1));
        let issue = &error.data["issues"][0];
        // The path is field names and array indices, not a flattened string.
        // It stops at the content block rather than reaching `/text`: the block
        // is an untagged variant, and serde reports the failure where it gave
        // up on the variant, not inside the one it never selected.
        assert_eq!(
            issue["path"],
            json!(["input", 0]),
            "the path names the field and index that failed: {}",
            error.data
        );
        assert!(
            issue["message"].as_str().is_some_and(|m| !m.is_empty()),
            "the issue carries a message"
        );

        // A rejection that is not a deserialization failure has no path to
        // point at, so `data` stays off the wire rather than serializing null.
        let batch = connection.dispatch(&request(
            4,
            "turn/start",
            json!({"sessionId": "absent", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let Envelope::Error(ErrorResponse { error, .. }) =
            decode_frame(&batch.outbound[0]).expect("rejection")
        else {
            unreachable!("an unknown session was accepted");
        };
        assert_ne!(error.code, ProtocolErrorCode::InvalidParams);
        let encoded = serde_json::to_value(&error).expect("error encodes");
        assert!(
            encoded.get("data").is_none(),
            "a non-deserialization rejection carries no data: {encoded}"
        );
    }

    /// Most methods are answered by the resource, release3 and release4
    /// dispatchers, which check their parameters by hand rather than through a
    /// deserializer. A client reads the same structured detail from those as
    /// from the handful this module parses itself.
    #[test]
    fn a_dispatcher_rejection_carries_the_same_structured_detail() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        // One method per dispatcher family, each missing a required parameter.
        for (method, params) in [
            ("tools/list", json!({})),
            ("session/title/update", json!({"sessionId": "session-1"})),
            ("loops/delete", json!({"sessionId": "session-1"})),
        ] {
            let batch = connection.dispatch(&request(5, method, params));
            let Envelope::Error(ErrorResponse { error, .. }) =
                decode_frame(&batch.outbound[0]).expect("rejection")
            else {
                unreachable!("{method} accepted parameters it should have rejected");
            };
            assert_eq!(
                error.code,
                ProtocolErrorCode::InvalidParams,
                "{method}: {}",
                error.message
            );
            assert_eq!(error.data["errorCount"], json!(1), "{method}");
            let issue = &error.data["issues"][0];
            assert!(
                issue["path"].is_array(),
                "{method} carries no path: {}",
                error.data
            );
            assert!(
                issue["message"].as_str().is_some_and(|m| !m.is_empty()),
                "{method} carries no message: {}",
                error.data
            );
        }
    }

    /// `vibe-core` and `vibe-protocol` sit in the same dependency layer, so the
    /// callback vocabulary is spelled twice. Both spellings cross the wire, so
    /// their JSON forms have to stay identical.
    #[test]
    fn callback_kinds_share_one_wire_form() {
        for (engine, wire) in [
            (EngineCallbackKind::Approval, CallbackKind::Approval),
            (EngineCallbackKind::UserInput, CallbackKind::UserInput),
            (
                EngineCallbackKind::ConnectorAuth,
                CallbackKind::ConnectorAuth,
            ),
        ] {
            assert_eq!(
                serde_json::to_value(engine).expect("engine kind"),
                serde_json::to_value(wire).expect("wire kind"),
                "{engine:?} and {wire:?} must serialize identically"
            );
        }
    }

    /// Registers one tool whose runtime prerequisite never holds, which is what
    /// a session-scoped tool does when the thing it drives is not there.
    struct UnavailablePrerequisiteTools;

    impl SessionToolFactory for UnavailablePrerequisiteTools {
        fn register(&self, _session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
            tools
                .register_conditional(
                    ToolSpec {
                        name: "fixture_probe".to_owned(),
                        description: "fixture".to_owned(),
                        input_schema: vibe_core::schema::ObjectSchema::new().build(),
                        output_schema: None,
                        config: Value::Null,
                        state: Value::Null,
                        availability: ToolAvailability::Available,
                        presentation: ToolPresentationKind::Generic,
                        source: ToolSource::Custom,
                        selection_priority: 0,
                    },
                    Arc::new(
                        |_invocation: &ToolInvocation,
                         _output: ToolOutputSink|
                         -> OwnedToolHandlerFuture {
                            Box::pin(async { Ok(ToolExecutionOutput::text("unreachable")) })
                        },
                    ),
                    Arc::new(|| false),
                )
                .map(drop)
                .map_err(|error| error.to_string())
        }
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
                            signals: crate::resources::ResourceSignals {
                                runtime_updated: true,
                                ..crate::resources::ResourceSignals::default()
                            },
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
                            signals: crate::resources::ResourceSignals::default(),
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

    /// Completes the handshake with the capabilities a client declares,
    /// answering with the `InitializeResponse` the server sent.
    fn initialize_with(connection: &mut ServerConnection, capabilities: Value) -> Value {
        let batch = connection.dispatch(&request(
            1,
            "initialize",
            json!({
                "clientInfo": {"name": "test", "version": "1"},
                "capabilities": capabilities
            }),
        ));
        let response = match decode_frame(&batch.outbound[0]).expect("handshake answer") {
            Envelope::Success(success) => Value::Object(success.result.into_iter().collect()),
            other => unreachable!("the handshake was rejected: {other:?}"),
        };
        let initialized = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .expect("initialized fixture");
        assert!(connection.dispatch(&initialized).outbound.is_empty());
        response
    }

    /// Calls a session-scoped read and returns the result it answered with.
    fn call(connection: &mut ServerConnection, id: i64, method: &str) -> BTreeMap<String, Value> {
        call_for(connection, id, method, "session-1")
    }

    fn call_for(
        connection: &mut ServerConnection,
        id: i64,
        method: &str,
        session_id: &str,
    ) -> BTreeMap<String, Value> {
        let batch = connection.dispatch(&request(id, method, json!({"sessionId": session_id})));
        match decode_frame(&batch.outbound[0]).expect("an answer") {
            Envelope::Success(SuccessResponse { result, .. }) => result,
            other => unreachable!("{method} did not answer: {other:?}"),
        }
    }

    fn start_session(connection: &mut ServerConnection) {
        let batch = connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": "/workspace"}),
        ));
        // The answer, then the snapshot the attachment publishes.
        assert_eq!(batch.outbound.len(), 2);
        assert!(matches!(
            decode_frame(&batch.outbound[1]).expect("attachment frame"),
            Envelope::Notification(Notification { ref method, .. })
                if method == "session/snapshot"
        ));
    }

    #[tokio::test]
    async fn accept_edits_profile_auto_approves_only_mutating_file_tools() {
        let factory = DefaultApprovalFactory;
        let approval = factory.for_agent("session", AgentApproval::Edits, false);
        let edit = approval
            .request(ApprovalRequest {
                tool: "edit".to_owned(),
                input: Value::Null,
                requirements: vec![PermissionRequirement::outside_directory("/workspace/*")],
                rationale: "edit file".to_owned(),
            })
            .await
            .expect("edit decision");
        let read = approval
            .request(ApprovalRequest {
                tool: "read_file".to_owned(),
                input: Value::Null,
                requirements: vec![PermissionRequirement::outside_directory("/workspace/*")],
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
                && rule.pattern.ends_with("/plans/*")
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
            json!({"sessionId": "session-1", "maxTurns": 0, "maxTokens": 64}),
        ));
        assert_eq!(update.outbound.len(), 1);
        // The model is not a reference session setting, so it travels under the
        // local name instead.
        let overridden = connection.dispatch(&request(
            5,
            "session/overrides/write",
            json!({"sessionId": "session-1", "model": "next-model"}),
        ));
        assert_eq!(overridden.outbound.len(), 1);
        let session = server.session("session-1").expect("session");
        assert_eq!(session.intent.max_turns, Some(0));
        assert_eq!(session.intent.max_tokens, Some(64));
        assert_eq!(session.intent.model.as_deref(), Some("next-model"));
        assert_eq!(session.active_turn.as_deref(), Some("turn-1"));

        // US-093: the reference method accepts `sessionId`, `maxTurns` and
        // `maxTokens`, and refuses everything else, including the five fields
        // this port moved to its own method.
        for (id, params) in [
            (6, json!({"sessionId": "session-1"})),
            (7, json!({"sessionId": "session-1", "maxTurns": 1.5})),
            (8, json!({"sessionId": "session-1", "maxTokens": -1})),
            (
                9,
                json!({"sessionId": "session-1", "maxTurns": 1, "future": true}),
            ),
            (10, json!({"sessionId": "session-1", "model": "other"})),
            (11, json!({"sessionId": "session-1", "mode": "plan"})),
            (12, json!({"sessionId": "session-1", "thinking": true})),
            (
                13,
                json!({"sessionId": "session-1", "reasoningEffort": "high"}),
            ),
            (14, json!({"sessionId": "session-1", "autoApprove": true})),
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
            15,
            "session/overrides/write",
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
            16,
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
        // The attachment snapshot opened the sequence at one.
        assert_eq!(scheduled.fire.notice.params["eventId"], 2);
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
            .fail_turn(
                "session-1",
                &turn_id,
                "injected interruption",
                TurnErrorCode::InternalError,
            )
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
            .fail_turn(
                "source-session",
                &first_turn_id,
                "completed fixture turn",
                TurnErrorCode::InternalError,
            )
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
                TurnErrorCode::InternalError,
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

    /// The six review methods, end to end over a real connection, against the
    /// engine the turn boundaries drive. This is the whole point of the epic:
    /// the panel used to be answered from a map production never wrote to, so
    /// `review/state` published an empty file list in every session and the
    /// other five answered `NotFound` for every path.
    #[test]
    fn the_review_surface_answers_the_six_methods_from_the_session_engine() {
        let temporary = tempfile::tempdir().expect("review surface stores");
        let session_root = temporary.path().join("sessions");
        let working_directory = temporary.path().join("workspace");
        fs::create_dir_all(&working_directory).expect("workspace");
        fs::write(working_directory.join("main.txt"), "one\n").expect("workspace fixture");
        let store = vibe_core::storage::SessionStore::new(&session_root);
        store
            .create(
                "review-session",
                &working_directory.to_string_lossy(),
                None,
                1,
            )
            .expect("source session");
        let release3 =
            Release3Service::for_runtime_session_root(session_root, working_directory.clone());
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "review-session", "resume": "review-session"}),
        ));
        let review = server
            .lock_sessions()
            .expect("runtime sessions")
            .get("review-session")
            .and_then(|session| session.review.clone())
            .expect("review manager");
        review.begin_turn_at("turn-1", 1).expect("begin turn");
        review
            .edit(
                "main.txt",
                &[vibe_core::workspace::EditOperation {
                    old_text: "one\n".to_owned(),
                    new_text: "one\ntwo\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");

        let answer = |connection: &mut ServerConnection, id: i64, method: &str, params: Value| {
            let batch = connection.dispatch(&request(id, method, params));
            match decode_frame(&batch.outbound[0]).expect("an answer") {
                Envelope::Success(SuccessResponse { result, .. }) => result,
                other => unreachable!("{method} did not answer: {other:?}"),
            }
        };
        let session = json!({"sessionId": "review-session"});

        let state = answer(&mut connection, 3, "review/state", session.clone());
        let files = state["files"].as_array().expect("a file list");
        assert_eq!(files.len(), 1, "the turn's change is reviewable: {state:?}");
        assert_eq!(files[0]["path"], "main.txt");
        assert_eq!(files[0]["status"], "modified");
        let region = &files[0]["regions"][0];
        assert_eq!(region["kind"], "text");
        assert_eq!(region["owner"], json!({"kind": "agent", "turnId": 1}));
        assert_eq!(region["decision"], "pending");
        assert_eq!(region["dependsOn"], json!([]));
        assert_eq!(
            state["scopes"][0]["owner"],
            json!({"kind": "agent", "turnId": 1}),
            "the turn keeps its own review slot"
        );
        assert_eq!(state["scopes"][0]["files"][0]["regionCount"], 1);

        let baseline = answer(
            &mut connection,
            4,
            "review/baseline",
            json!({"sessionId": "review-session", "path": "main.txt"}),
        );
        assert_eq!(baseline["content"], "one\n");

        let hunks = answer(
            &mut connection,
            5,
            "review/hunks",
            json!({"sessionId": "review-session", "path": "main.txt"}),
        );
        assert_eq!(hunks["hunks"].as_array().expect("anchors").len(), 1);
        assert_eq!(hunks["hunks"][0]["side"], "additions");
        assert_eq!(hunks["hunks"][0]["line"], 1);

        let diff = answer(
            &mut connection,
            6,
            "review/turnDiff",
            json!({
                "sessionId": "review-session",
                "path": "main.txt",
                "owner": {"kind": "agent", "turnId": 1}
            }),
        );
        assert_eq!(diff["status"], "modified");
        assert_eq!(diff["baseline"], "one\n");
        assert_eq!(
            diff["current"], "one\ntwo\n",
            "the owner's own change is what turnDiff answers"
        );

        // The region the panel was shown is the region it sends back.
        let target = json!({
            "kind": "region",
            "path": "main.txt",
            "versionIndex": region["versionIndex"],
            "ordinal": region["ordinal"]
        });
        let approved = answer(
            &mut connection,
            7,
            "review/approve",
            json!({"sessionId": "review-session", "target": target}),
        );
        // `EmptyResponse` declares no field, so an empty object is exactly what
        // the census requires of both mutations. They are asserted here rather
        // than in the surface probe, which only calls read-only methods.
        assert!(
            approved.is_empty(),
            "an approval answers nothing: {approved:?}"
        );
        assert_eq!(
            fs::read_to_string(working_directory.join("main.txt")).expect("read"),
            "one\ntwo\n",
            "an approval leaves disk alone"
        );
        let resolved = answer(&mut connection, 8, "review/state", session.clone());
        assert_eq!(
            resolved["files"].as_array().expect("a file list").len(),
            0,
            "the file is resolved once its one region is decided"
        );
        assert_eq!(
            answer(
                &mut connection,
                9,
                "review/baseline",
                json!({"sessionId": "review-session", "path": "main.txt"})
            )["content"],
            "one\ntwo\n",
            "the accepted baseline now carries the kept region"
        );

        // A second turn, reverted whole, is written back to disk.
        review.begin_turn_at("turn-2", 2).expect("begin turn");
        review
            .edit(
                "main.txt",
                &[vibe_core::workspace::EditOperation {
                    old_text: "two\n".to_owned(),
                    new_text: "two\nthree\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");
        let reverted = answer(
            &mut connection,
            10,
            "review/revert",
            json!({
                "sessionId": "review-session",
                "target": {"kind": "scope", "owner": {"kind": "agent", "turnId": 2}}
            }),
        );
        assert!(reverted.is_empty());
        assert_eq!(
            fs::read_to_string(working_directory.join("main.txt")).expect("read"),
            "one\ntwo\n",
            "a revert is persisted immediately"
        );

        // What the engine refuses is `invalid_params`, which is the code the
        // reference answers a review failure with.
        let refused = connection.dispatch(&request(
            11,
            "review/approve",
            json!({
                "sessionId": "review-session",
                "target": {
                    "kind": "region",
                    "path": "main.txt",
                    "versionIndex": 99,
                    "ordinal": 4
                }
            }),
        ));
        assert!(
            matches!(
                decode_frame(&refused.outbound[0]).expect("a refusal"),
                Envelope::Error(ErrorResponse {
                    error: ProtocolError {
                        code: ProtocolErrorCode::InvalidParams,
                        ..
                    },
                    ..
                })
            ),
            "a region the file does not carry is refused"
        );
        let malformed = connection.dispatch(&request(
            12,
            "review/revert",
            json!({"sessionId": "review-session", "target": {"kind": "nonsense"}}),
        ));
        assert!(matches!(
            decode_frame(&malformed.outbound[0]).expect("a rejection"),
            Envelope::Error(ErrorResponse {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidParams,
                    ..
                },
                ..
            })
        ));
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
        let callback_request = decode_frame(callback_request.last().expect("callback delivery"))
            .expect("callback request frame");
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
        let acknowledgment = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: callback_request_id,
            result: result_map([
                ("callbackId", json!(callback_id)),
                ("accepted", json!(true)),
            ]),
        }));
        let acknowledgment = connection.dispatch(&acknowledgment);
        assert_eq!(acknowledgment, DispatchBatch::empty());
        let first = connection.dispatch(&request(
            5,
            "callback/respond",
            json!({
                "sessionId": "session-1",
                "callbackId": callback_id,
                "output": {
                    "type": "approval",
                    "decision": {"type": "approve"},
                    // The reference approval output declares an operator note,
                    // so a client sending one is answered rather than rejected.
                    "feedback": "keep the scope tight"
                }
            }),
        ));
        // The answer, then the `session/updated` the resumed status publishes.
        assert_eq!(first.outbound.len(), 2);
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
                        state: PublicCallbackState::Answered {
                            output: CallbackOutput::Approval { decision, feedback }
                        },
                        ..
                    } if callback_id == "callback-1"
                        && decision.decision == ApprovalDecisionType::Approve
                        && feedback.as_deref() == Some("keep the scope tight")
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
                    "decision": {"type": "approve"},
                    "feedback": "keep the scope tight"
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
        let callback_request = decode_frame(callback_request.last().expect("callback delivery"))
            .expect("callback request frame");
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
        }));
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
        let request_id = match decode_frame(callback_request.last().expect("callback delivery"))
            .expect("callback frame")
        {
            Envelope::Request(request) => request.id,
            _ => return,
        };
        let acknowledgment = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id,
            result: result_map([
                ("callbackId", json!(callback_id)),
                ("accepted", json!(true)),
            ]),
        }));
        assert_eq!(connection.dispatch(&acknowledgment), DispatchBatch::empty());

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
                        state: PublicCallbackState::Answered {
                            output: CallbackOutput::UserInput { result }
                        },
                        ..
                    } if result.cancelled
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
                        state: PublicCallbackState::Answered {
                            output: CallbackOutput::Approval { decision, .. }
                        },
                        ..
                    } if callback_id == "callback-1"
                        && decision.decision == ApprovalDecisionType::Deny
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
    fn answered_delivery_ignores_a_late_negative_acknowledgment() {
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
        let first_request_id = match decode_frame(first_delivery.last().expect("callback frame"))
            .expect("callback delivery")
        {
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
        }));
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

    /// The reference declares no plan-review field on a callback detail, so the
    /// port's marker becomes the notice entry the reference publishes and the
    /// detail that reaches the client carries only the declared keys.
    #[test]
    fn a_plan_review_becomes_a_notice_and_leaves_the_callback_detail_conformant() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
        ));
        let review = |file_path: Value| {
            json!({
                "kind": "user_input",
                "request": {
                    "questions": [{
                        "question": "Switch to code mode?",
                        "options": [{"label": "Yes"}, {"label": "No"}]
                    }],
                    "footerNote": null,
                },
                "planReview": true,
                "filePath": file_path,
                "relatedEntryId": null,
            })
        };

        assert!(matches!(
            connection.request_callback_with_detail(
                "session-1",
                "turn-1",
                EngineCallbackKind::UserInput,
                "Review plan",
                review(Value::Null),
            ),
            Err(ServerError::InvalidCallbackDetail(_))
        ));

        let (callback_id, _) = connection
            .request_callback_with_detail(
                "session-1",
                "turn-1",
                EngineCallbackKind::UserInput,
                "Review plan",
                review(json!("/workspace/plan.md")),
            )
            .expect("the plan review is accepted");

        let sessions = server.lock_sessions().expect("sessions");
        let history = &sessions
            .get("session-1")
            .expect("session")
            .snapshot
            .as_ref()
            .expect("snapshot")
            .history;
        let notice = history
            .iter()
            .find_map(|entry| match entry {
                PublicHistoryEntry::Notice {
                    metadata, detail, ..
                } => Some((metadata, detail)),
                _ => None,
            })
            .expect("the plan review is published as a notice");
        assert_eq!(
            notice.1,
            &NoticeDetail::PlanReviewStarted {
                file_path: "/workspace/plan.md".to_owned()
            }
        );
        assert_eq!(
            notice.0.related_entry_id.as_deref(),
            Some(format!("callback:{callback_id}").as_str())
        );

        let detail = history
            .iter()
            .find_map(|entry| match entry {
                PublicHistoryEntry::Callback { detail, .. } => Some(detail),
                _ => None,
            })
            .expect("the callback is published");
        let wire = serde_json::to_value(detail).expect("the detail serializes");
        let keys = wire
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(keys, ["kind", "relatedEntryId", "request"]);
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
        let request_id = match decode_frame(callback_request.last().expect("callback delivery"))
            .expect("callback frame")
        {
            Envelope::Request(request) => request.id,
            _ => return,
        };
        let acknowledgment = encode_frame(&Envelope::Success(SuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id,
            result: result_map([
                ("callbackId", json!("callback-1")),
                ("accepted", json!(true)),
            ]),
        }));
        assert_eq!(connection.dispatch(&acknowledgment), DispatchBatch::empty());

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
        // `/workspace` is not a usable root, so the file tools stay unregistered
        // while the universal tools, which need no root, are still published.
        let published = match decode_frame(&tools.outbound[0]).expect("tools response") {
            Envelope::Success(SuccessResponse { result, .. }) => {
                result["tools"].as_array().map(|published| {
                    published
                        .iter()
                        .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
                        .collect::<BTreeSet<_>>()
                })
            }
            _ => None,
        }
        .expect("tools/list answers with the published names");
        for universal in ["skill", "todo", "web_fetch"] {
            assert!(published.contains(universal), "{published:?}");
        }
        for workspace_tool in ["edit", "grep", "read_file", "write_file"] {
            assert!(!published.contains(workspace_tool), "{published:?}");
        }

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
        let Envelope::Success(SuccessResponse { result, .. }) =
            decode_frame(&add.outbound[0]).expect("MCP response")
        else {
            unreachable!("mcp/add answers");
        };
        assert_eq!(
            result.keys().map(String::as_str).collect::<Vec<_>>(),
            ["created", "name", "runtime", "url"]
        );
        assert_eq!(result["created"], json!(true));
        assert_eq!(result["name"], json!("example"));
        assert_eq!(result["url"], json!("https://127.0.0.1:9/mcp"));
        // The source could not be reached, so it is published as unavailable
        // rather than as a switch the operator threw.
        assert_eq!(
            result["runtime"]["mcp"]["sources"][0]["status"],
            json!("unavailable")
        );
        assert!(
            result["runtime"]["mcp"]["discoveryErrors"]["example"]
                .as_str()
                .is_some_and(|message| message.contains("MCP `example`")),
            "the source that would not start is named: {}",
            result["runtime"]["mcp"]
        );
        // The change and the problem cross under their reference names.
        assert!(matches!(
            decode_frame(&add.outbound[1]).expect("MCP notification"),
            Envelope::Notification(Notification { method, .. }) if method == "runtime/updated"
        ));
        assert!(matches!(
            decode_frame(&add.outbound[2]).expect("MCP warning"),
            Envelope::Notification(Notification { method, ref params, .. })
                if method == "warning"
                    && params["warning"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("MCP `example`"))
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
        // The answer, then the snapshot the attachment publishes.
        assert_eq!(started.outbound.len(), 2);
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
            Envelope::Notification(Notification { method, .. }) if method == "runtime/updated"
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

    /// The client's own event stream, which the reference hands to the agent
    /// loop's telemetry client and this port keeps where an operator can read
    /// it. Both gate it on `enable_telemetry`, so a client that records against
    /// a session with telemetry off leaves nothing behind.
    #[test]
    fn a_recorded_client_event_is_kept_only_while_telemetry_is_enabled() {
        for enabled in [true, false] {
            let temporary = tempfile::tempdir().expect("temporary workspace");
            let working_directory = temporary.path().join("workspace");
            let vibe_home = temporary.path().join("home");
            fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
            fs::create_dir_all(&vibe_home).expect("user config directory");
            fs::write(
                working_directory.join(".vibe/config.toml"),
                format!("enable_telemetry = {enabled}\n"),
            )
            .expect("telemetry configuration");
            let release3 = Release3Service::new(
                crate::release3::Release3Paths {
                    vibe_home,
                    working_directory: working_directory.clone(),
                    session_root: temporary.path().join("sessions"),
                },
                true,
            )
            .expect("release-3 service");
            let server = AppServer::with_release3_service(release3);
            let mut connection = server.connect(TransportKind::InProcess);
            initialize(&mut connection);
            connection.dispatch(&request(
                2,
                "session/start",
                json!({"sessionId": "session-1", "workingDirectory": working_directory}),
            ));

            let recorded = connection.dispatch(&request(
                3,
                "telemetry/record",
                json!({
                    "sessionId": "session-1",
                    "name": "vibe.client_ready",
                    "properties": {"surface": "editor"},
                    "correlateLastRequest": true
                }),
            ));
            match decode_frame(&recorded.outbound[0]).expect("a recorded answer") {
                Envelope::Success(SuccessResponse { result, .. }) => {
                    assert!(result.is_empty(), "the answer is empty: {result:?}");
                }
                other => unreachable!("telemetry/record did not answer: {other:?}"),
            }

            let logs = call(&mut connection, 4, "diagnostics/logs/read");
            let entries = logs["logs"]["entries"]
                .as_array()
                .expect("a log page")
                .iter()
                .filter(|entry| {
                    entry["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("vibe.client_ready"))
                })
                .count();
            assert_eq!(
                entries,
                usize::from(enabled),
                "enable_telemetry = {enabled} decides whether the event is kept"
            );
        }
    }

    /// The reference model declares four fields, so a client that sends a fifth
    /// is answered with the pointer to it rather than having it ignored.
    #[test]
    fn a_recorded_client_event_refuses_a_field_the_reference_does_not_declare() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": "/workspace"}),
        ));

        let refused = connection.dispatch(&request(
            3,
            "telemetry/record",
            json!({"sessionId": "session-1", "name": "probe", "surplus": true}),
        ));

        match decode_frame(&refused.outbound[0]).expect("a refusal") {
            Envelope::Error(error) => {
                assert_eq!(error.error.code, ProtocolErrorCode::InvalidParams);
                assert_eq!(error.error.data["errorCount"], json!(1));
                assert_eq!(error.error.data["issues"][0]["path"], json!(["surplus"]));
            }
            other => unreachable!("the surplus field was accepted: {other:?}"),
        }
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

    /// The configuration file is a shared surface with the reference, so the
    /// two filter lists it carries reach the session the same way: the
    /// allowlist stands when the client asks for none, the denylist
    /// concatenates onto what the client sent, and an entry that does not
    /// compile is reported rather than applied.
    #[test]
    fn session_start_reads_the_configured_tool_filters_and_reports_a_broken_entry() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
        fs::create_dir_all(&vibe_home).expect("user config directory");
        fs::write(
            working_directory.join(".vibe/config.toml"),
            "enabled_tools = [\"read_file\", \"serena_*\"]\n\
             disabled_tools = [\"re:web_.*\", \"re:[\"]\n",
        )
        .expect("project tool filters");
        let release3 = Release3Service::new(
            crate::release3::Release3Paths {
                vibe_home,
                working_directory: working_directory.clone(),
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("release-3 service");
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({
                "sessionId": "filtered",
                "workingDirectory": working_directory,
                "trustWorkspace": true,
                "disabledTools": ["exit_plan_mode"]
            }),
        ));

        let intent = server
            .sessions
            .lock()
            .expect("sessions")
            .get("filtered")
            .map(|session| session.intent.clone())
            .expect("the session started");
        assert_eq!(intent.enabled_tools, ["read_file", "serena_*"]);
        assert_eq!(
            intent.disabled_tools,
            ["exit_plan_mode", "re:[", "re:web_.*"]
        );

        let diagnostics = connection.dispatch(&request(
            3,
            "diagnostics/list",
            json!({"sessionId": "filtered"}),
        ));
        let reported = match decode_frame(&diagnostics.outbound[0]).expect("diagnostics response") {
            Envelope::Success(SuccessResponse { result, .. }) => result["issues"]
                .as_array()
                .map(|issues| {
                    issues
                        .iter()
                        .filter_map(|issue| issue["message"].as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        assert!(
            reported
                .iter()
                .any(|message| message.contains("disabled_tools entry `re:[`")),
            "the broken entry must be named: {reported:?}"
        );

        // US-090: the same problem is on the runtime snapshot, named by the file
        // it came from, and the rest of the answer is still whole.
        let runtime = call_for(&mut connection, 4, "runtime/read", "filtered")["runtime"].clone();
        let issues = runtime["issues"].as_array().expect("issues is a list");
        assert!(
            issues.iter().any(|issue| {
                issue["file"] == json!(CONFIG_FILE_LABEL)
                    && issue["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("disabled_tools entry `re:[`"))
            }),
            "the offending file must be named: {issues:?}"
        );
        assert!(issues.iter().all(|issue| {
            issue.as_object().is_some_and(|issue| {
                issue.len() == 2 && issue.contains_key("file") && issue.contains_key("message")
            })
        }));
        assert!(runtime["config"]["activeModel"].is_object());
    }

    /// The path the `vibe` binary and the ACP adapter actually take: they build
    /// a [`crate::client::SessionOptions`] and never a raw params object, so a
    /// run without `--enabled-tools` must leave the configured allowlist
    /// standing, the way the reference passes `None` for an absent flag.
    #[test]
    fn a_client_that_asks_for_no_allowlist_keeps_the_configured_one() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        fs::create_dir_all(working_directory.join(".vibe")).expect("project config directory");
        fs::create_dir_all(&vibe_home).expect("user config directory");
        fs::write(
            working_directory.join(".vibe/config.toml"),
            "enabled_tools = [\"read_file\", \"serena_*\"]\n",
        )
        .expect("project tool filters");
        let release3 = Release3Service::new(
            crate::release3::Release3Paths {
                vibe_home,
                working_directory: working_directory.clone(),
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("release-3 service");
        let server = AppServer::with_release3_service(release3);
        let mut client =
            crate::client::InProcessClient::connect_with_server(server.clone()).expect("client");
        let session_id = client
            .start_session(&crate::client::SessionOptions {
                working_directory: working_directory.to_string_lossy().into_owned(),
                trusted: true,
                ..default_session_options()
            })
            .expect("the session starts");

        let intent = server
            .sessions
            .lock()
            .expect("sessions")
            .get(&session_id)
            .map(|session| session.intent.clone())
            .expect("the session started");
        assert_eq!(intent.enabled_tools, ["read_file", "serena_*"]);
    }

    /// The options a client sends when the operator passed no tool flag.
    fn default_session_options() -> crate::client::SessionOptions {
        crate::client::SessionOptions {
            working_directory: String::new(),
            session_id: None,
            add_directories: Vec::new(),
            trusted: false,
            agent: None,
            tool_filters: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            mcp_servers: Vec::new(),
            model: None,
            max_turns: None,
            max_tokens: None,
            max_price_micros: None,
            mode: None,
            thinking: false,
            reasoning_effort: None,
            auto_approve: false,
            resume: None,
            continue_session: false,
        }
    }

    /// A session attached from persisted state runs under the same
    /// configuration a fresh one does, so the filter lists reach it there too
    /// rather than only on the `session/start` path.
    #[test]
    fn an_attached_runtime_session_carries_the_configured_tool_filters() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let working_directory = temporary.path().join("workspace");
        let session_root = temporary.path().join("sessions");
        let vibe_home = temporary.path().join("home");
        fs::create_dir_all(&working_directory).expect("workspace");
        fs::create_dir_all(&vibe_home).expect("user config directory");
        // The user file, which applies whatever the workspace trust decision is.
        fs::write(
            vibe_home.join("config.toml"),
            "disabled_tools = [\"serena_*\"]\n",
        )
        .expect("user tool filters");
        vibe_core::storage::SessionStore::new(&session_root)
            .create("attached", &working_directory.to_string_lossy(), None, 1)
            .expect("persisted session");
        let release3 = Release3Service::new(
            crate::release3::Release3Paths {
                vibe_home,
                working_directory: working_directory.clone(),
                session_root,
            },
            true,
        )
        .expect("release-3 service");
        let server = AppServer::with_release3_service(release3);
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        // Selecting an agent attaches the persisted session to this connection,
        // which is the path that rebuilds the intent from stored state.
        connection.dispatch(&request(
            2,
            "session/agent/update",
            json!({"sessionId": "attached", "name": "default"}),
        ));

        let intent = server
            .sessions
            .lock()
            .expect("sessions")
            .get("attached")
            .map(|session| session.intent.clone())
            .expect("the session attached");
        // The default agent adds its own entry, and the configured one survives
        // the agent overlay rather than being replaced by it.
        assert_eq!(intent.disabled_tools, ["exit_plan_mode", "serena_*"]);
    }

    /// Reference edge case: a tool whose prerequisite is missing is absent from
    /// the surface rather than published and failed at call time, and the
    /// session says which tool it withheld.
    #[test]
    fn a_tool_whose_prerequisite_is_missing_is_withheld_and_named() {
        let server =
            AppServer::default().using_session_tool_factory(Arc::new(UnavailablePrerequisiteTools));
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);

        let published = server
            .sessions
            .lock()
            .expect("sessions")
            .get("session-1")
            .map(|session| session.tools.clone())
            .expect("the session started")
            .available(&NameFilter::default(), &NameFilter::default())
            .expect("available")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(
            !published.contains(&"fixture_probe".to_owned()),
            "a tool with no prerequisite reached the surface: {published:?}"
        );

        let diagnostics = connection.dispatch(&request(
            3,
            "diagnostics/list",
            json!({"sessionId": "session-1"}),
        ));
        let reported = match decode_frame(&diagnostics.outbound[0]).expect("diagnostics response") {
            Envelope::Success(SuccessResponse { result, .. }) => result["issues"]
                .as_array()
                .map(|issues| {
                    issues
                        .iter()
                        .filter_map(|issue| issue["message"].as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        assert!(
            reported
                .iter()
                .any(|message| message.contains("tool `fixture_probe` is withheld")),
            "the withheld tool must be named: {reported:?}"
        );
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

    /// US-102: the families are published against the resolver, so a
    /// configuration change between two turns reaches the handlers without the
    /// session's surface being registered again.
    #[tokio::test]
    async fn a_configuration_change_between_turns_reaches_the_published_tools() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("visible.txt"), "safe\n").expect("fixture");
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": workspace.path()}),
        ));
        connection.dispatch(&request(
            3,
            "workspace/trust/decision",
            json!({
                "sessionId": "session-1",
                "cwd": workspace.path(),
                "decision": "trust_cwd"
            }),
        ));
        let invocation = || ToolInvocation {
            call_id: "read-1".to_owned(),
            arguments: json!({"file_path": "visible.txt"}),
        };
        assert_eq!(
            server
                .invoke_tool("session-1", "read_file", invocation())
                .await
                .expect("the declared budget carries this file")
                .typed_result["content"],
            "        1\u{2192}safe"
        );

        // No re-registration: the same published surface, a moved budget.
        let config = server.release3.tool_config();
        config.update(
            "[read_file]\nmax_read_bytes = 2\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let refused = server
            .invoke_tool("session-1", "read_file", invocation())
            .await
            .expect_err("the lowered budget refuses the same file");
        assert!(refused.to_string().contains("2-byte budget"), "{refused}");
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
        // The answer, then the snapshot the attachment publishes.
        assert_eq!(started.outbound.len(), 2);

        let invocation = || ToolInvocation {
            call_id: "read-1".to_owned(),
            arguments: json!({"file_path": "visible.txt"}),
        };
        assert!(
            server
                .invoke_tool("session-1", "read_file", invocation())
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
                .invoke_tool("session-1", "read_file", invocation())
                .await
                .expect("trusted read")
                .typed_result["content"],
            "        1\u{2192}safe"
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
                .invoke_tool("session-1", "read_file", invocation())
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
        // The answer, then the snapshot the attachment publishes.
        assert_eq!(started.outbound.len(), 2);
        let invocation = |call_id: &str| ToolInvocation {
            call_id: call_id.to_owned(),
            arguments: json!({
                "file_path": "editable.txt",
                "old_string": "before",
                "new_string": "after",
                "replace_all": false
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
            "session/overrides/write",
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
            .fail_turn(
                "session-1",
                "turn-1",
                "provider failed",
                TurnErrorCode::BackendError,
            )
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
            decode_frame(notification.last().expect("completion frame")).expect("completion notification"),
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
            decode_frame(notification.last().expect("completion frame")).expect("completion notification"),
            Envelope::Notification(Notification {
                method,
                params,
                ..
            }) if method == "turn/completed"
                && params["turn"]["status"] == "failed"
                && params["turn"]["error"]["code"] == "response_too_long"
        ));
    }

    /// A handoff onto the session's own name is refused rather than published:
    /// a reference projection validates a handoff by the two identifiers
    /// differing, so emitting one would break the client it was meant to serve.
    #[test]
    fn a_handoff_onto_the_same_session_is_refused() {
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "clear"}]}),
        ));
        let mut reducer = vibe_core::events::ProjectionReducer::for_turn("session-1", "turn-1");
        reducer
            .apply(&vibe_core::events::EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 1,
                event_id: 1,
                event: vibe_core::events::EngineEvent::UserMessage {
                    content: "clear".to_owned(),
                },
            })
            .expect("the prompt projects");
        let snapshot = reducer.state().clone();
        for notice in [
            HandoffNotice::Compacted { summary_length: 4 },
            HandoffNotice::ContextCleared {
                plan_file_path: None,
            },
        ] {
            let error = server
                .handoff_active_turn(
                    "session-1",
                    "session-1",
                    "turn-1",
                    snapshot.clone(),
                    &notice,
                    1,
                )
                .expect_err("a handoff onto the same identifier is refused");
            assert!(matches!(error, ServerError::SessionConflict(id) if id == "session-1"));
        }
    }

    /// Every failing turn names a reason from the reference vocabulary, whether
    /// the failure arrived with the projection or short-circuited it, so a
    /// client can branch on the code rather than parse the message.
    #[test]
    fn a_failed_turn_publishes_a_reference_error_code() {
        let vocabulary = [
            "rate_limit",
            "context_too_long",
            "response_too_long",
            "refusal",
            "invalid_image_attachment",
            "images_not_supported",
            "compaction_failed",
            "backend_error",
            "internal_error",
        ];
        let server = AppServer::default();
        let mut connection = server.connect(TransportKind::InProcess);
        initialize(&mut connection);
        start_session(&mut connection);
        connection.dispatch(&request(
            3,
            "turn/start",
            json!({"sessionId": "session-1", "input": [{"type": "text", "text": "fail"}]}),
        ));
        let notification = server
            .fail_turn(
                "session-1",
                "turn-1",
                "the provider rejected the request",
                TurnErrorCode::RateLimit,
            )
            .expect("failed turn settles");
        let Envelope::Notification(Notification { method, params, .. }) =
            decode_frame(notification.last().expect("completion frame"))
                .expect("completion notification")
        else {
            return;
        };
        assert_eq!(method, "turn/completed");
        assert_eq!(params["turn"]["error"]["code"], "rate_limit");
        assert!(
            vocabulary.contains(
                &params["turn"]["error"]["code"]
                    .as_str()
                    .expect("a failing turn names its code")
            ),
            "the code must come from the reference vocabulary: {params:?}"
        );
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
                    cause: vibe_core::events::SessionHandoffCause::Compaction,
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
