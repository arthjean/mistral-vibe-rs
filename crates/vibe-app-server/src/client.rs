use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::compaction::CompactionFailure;
use vibe_core::compaction::manager::{
    self as compaction_manager, CompactionPlan, CompactionPromptResolution,
};
use vibe_core::engine::{
    CancellationToken, CompactionResult, Compactor, CompletionProvider, CompositeEventObserver,
    ConversationEngine, EngineError, EngineLimits, NoopEventObserver, SessionStats,
    SessionTranscriptSink, ToolExecutor, ToolFuture, ToolStreamSink, TurnControl,
    TurnControlHandle, TurnOutcome,
};
use vibe_core::events::{
    ApplyOutcome, CallbackKind as EngineCallbackKind, EventEnvelope, LifecycleState, ModelMessage,
    ProjectionReducer,
};
use vibe_core::extensions::{
    AgentKind, AgentProfile, ChildContext, ChildLoggingPolicy, DelegationRequest, DiscoveryRoots,
    ExtensionSource, SubagentFuture, SubagentManager, SubagentRunner, discover_extensions,
};
use vibe_core::matching::NameFilter;
use vibe_core::mcp::{
    McpError, McpFuture, McpServerConfig, SamplingHandler, SamplingRequest, SamplingResponse,
    SamplingRole,
};
use vibe_core::middleware::{CompactionSettings, ContextWarningMiddleware};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PolicyError,
};
use vibe_core::provider::{
    HttpTransport, ProviderBackend, ProviderError, ProviderInput, ProviderStyle, RequestLimits,
    ToolChoice, ToolDefinition, TransportError, Usage,
};
use vibe_core::schema::{ObjectSchema, Property};
use vibe_core::session_id::rotate_session_id;
use vibe_core::storage::{HydratedSession, SessionStore};
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};
use vibe_protocol::{
    CallbackKind as ClientCallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, Envelope,
    ErrorResponse, ProtocolError, RequestId, SuccessResponse, TerminalEmulator, TransportKind,
    decode_frame,
};

pub use crate::images::PreparedImages;
use crate::images::{provider_images, validate_prepared_images};
use crate::server::{
    AppServer, ApprovalAgentFactory, DeferredWork, ServerConnection, ServerError, SessionIntent,
    SessionToolFactory, SessionView,
};

pub use vibe_core::engine::EventObserver;
pub use vibe_core::engine::TurnOutcome as PublicTurnOutcome;
pub use vibe_core::engine::TurnStopReason as PublicTurnStopReason;
pub use vibe_core::events::CallbackKind as PublicCallbackKind;
pub use vibe_core::events::{
    CallbackDetail, CallbackOutput, EffectCallDisplay, EffectDetail, EffectResultDisplay,
    NoticeDetail, PublicCallbackState, PublicContentBlock, PublicEffectState, PublicError,
    PublicHistoryEntry, PublicMessageRole, ToolEffectKind, TurnErrorCode,
};

pub type DriverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TurnOutcome, DriverError>> + Send + 'a>>;
pub type CompactionDriverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SessionCompaction, DriverError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct SessionCompaction {
    pub old_session_id: String,
    pub new_session_id: String,
    pub summary: String,
    pub hydrated: HydratedSession,
}

pub trait TurnDriver: Send + Sync {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a>;

    fn plan_directory(&self) -> Option<PathBuf> {
        None
    }

    fn run_observed<'a>(
        &'a self,
        reservation: &'a TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> DriverFuture<'a> {
        Box::pin(async move {
            let outcome = self.run(reservation).await?;
            for event in &outcome.events {
                observer.observe(event).map_err(DriverError::Observation)?;
            }
            Ok(outcome)
        })
    }

    fn interrupt(&self, _session_id: &str, _turn_id: &str) -> Result<(), DriverError> {
        Ok(())
    }

    fn steer(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _content: &str,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedControl("turn/steer"))
    }

    fn inject_context(
        &self,
        _session_id: &str,
        _content: &str,
        _as_message: bool,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedControl("session/context/inject"))
    }

    fn resolve_callback(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _callback_id: &str,
        _accepted: bool,
        _value: Option<&str>,
    ) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedControl("callback/respond"))
    }

    fn compact<'a>(
        &'a self,
        _session_id: &'a str,
        _extra_instructions: &'a str,
    ) -> CompactionDriverFuture<'a> {
        Box::pin(async { Err(DriverError::UnsupportedControl("session/compact/start")) })
    }

    /// Queues a context clearing on a running turn.
    ///
    /// The turn drops its transcript and rotates onto a fresh session at its
    /// next cycle boundary, continuing from `continuation` alone.
    fn clear_context(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _continuation: &str,
        _plan_file_path: Option<&str>,
    ) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedControl("session/context/clear"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOptions {
    pub working_directory: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub add_directories: Vec<String>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub tool_filters: Vec<String>,
    /// Omitted when empty, because `session/start` reads it as an optional
    /// allowlist: an absent field leaves the configured `enabled_tools`
    /// standing, while an empty array replaces it. The reference draws the same
    /// line, passing `None` for a `--enabled-tools` flag nobody used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_price_micros: Option<u64>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default, rename = "continue")]
    pub continue_session: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReservation {
    pub session_id: String,
    pub turn_id: String,
    pub prompt: String,
    pub input: Vec<PublicContentBlock>,
    /// Stable, provider-ready image snapshots. `None` means the driver must
    /// materialize and validate the public attachment blocks before use.
    pub prepared_images: Option<PreparedImages>,
    pub client_user_message_id: Option<String>,
    pub auto_title: Option<String>,
    pub user_display_content: Option<Value>,
    pub mention_stats: Option<Value>,
    pub working_directory: String,
    pub intent: SessionIntent,
    /// The compaction policy the session read when it opened, which the engine
    /// hands to its policy layer and its reactive recovery.
    pub compaction: CompactionSettings,
    pub tools: ToolRegistry,
}

#[derive(Debug, Clone)]
pub struct ScheduledTurn {
    pub loop_id: String,
    pub reservation: TurnReservation,
    pub notice: PublicNotification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRequest {
    pub prompt: String,
    pub input: Vec<PublicContentBlock>,
    /// Whether the harness is starting this turn rather than the operator,
    /// which v2.24.0 added to `TurnStartParams`
    /// (`vibe/app_server/protocol.py:1112`). It is carried on the wire and
    /// defaults false; no path in this port starts an injected turn yet, and the
    /// first one is the compaction envelope EP-043 writes.
    #[serde(default)]
    pub injected: bool,
    #[serde(default)]
    pub client_user_message_id: Option<String>,
    #[serde(default)]
    pub auto_title: Option<String>,
    #[serde(default)]
    pub user_display_content: Option<Value>,
    #[serde(default)]
    pub mention_stats: Option<Value>,
}

impl TurnRequest {
    #[must_use]
    pub fn text(prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        Self {
            input: vec![PublicContentBlock::Text {
                text: prompt.clone(),
            }],
            prompt,
            injected: false,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub price_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgrammaticTurn {
    pub session_id: String,
    pub turn_id: String,
    pub final_assistant: String,
    pub history: Vec<PublicHistoryEntry>,
    pub events: Vec<Value>,
    pub usage: PublicUsage,
    pub context_tokens: u64,
    pub steps: u32,
    pub checkpoints: u32,
    pub stop_reason: PublicTurnStopReason,
    #[serde(default)]
    pub teleport_events: Vec<ProgrammaticTeleportEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublicNotification {
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublicDispatch {
    pub result: BTreeMap<String, Value>,
    pub notifications: Vec<PublicNotification>,
}

#[derive(Debug)]
pub enum InterruptOutcome {
    Complete,
    DriverOnly { canonical_error: ClientError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProgrammaticTeleportEvent {
    SummarizingContext {
        operation_id: String,
    },
    CheckingGit {
        operation_id: String,
    },
    PushRequired {
        operation_id: String,
        unpushed_count: u64,
        #[serde(default)]
        branch_not_pushed: bool,
    },
    Pushing {
        operation_id: String,
    },
    StartingWorkflow {
        operation_id: String,
    },
    Complete {
        operation_id: String,
        url: String,
    },
    Failed {
        operation_id: String,
        error: vibe_core::events::PublicError,
    },
}

fn teleport_events(
    notifications: &[PublicNotification],
) -> Result<Vec<ProgrammaticTeleportEvent>, ClientError> {
    notifications
        .iter()
        .filter(|notification| notification.method == "vibeCode/teleport/event")
        .map(|notification| {
            let event = notification.params.get("event").ok_or_else(|| {
                ClientError::InvalidResponse("Teleport notification omitted event".to_owned())
            })?;
            let operation_id = event
                .get("operationId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ClientError::InvalidResponse(
                        "Teleport notification omitted operationId".to_owned(),
                    )
                })?
                .to_owned();
            match event.get("kind").and_then(Value::as_str) {
                Some("summarizing_context") => {
                    Ok(ProgrammaticTeleportEvent::SummarizingContext { operation_id })
                }
                Some("checking_git") => Ok(ProgrammaticTeleportEvent::CheckingGit { operation_id }),
                Some("push_required") => Ok(ProgrammaticTeleportEvent::PushRequired {
                    operation_id,
                    unpushed_count: event
                        .get("unpushedCount")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    branch_not_pushed: event
                        .get("branchNotPushed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }),
                Some("pushing") => Ok(ProgrammaticTeleportEvent::Pushing { operation_id }),
                Some("starting_workflow") => {
                    Ok(ProgrammaticTeleportEvent::StartingWorkflow { operation_id })
                }
                Some("complete") => Ok(ProgrammaticTeleportEvent::Complete {
                    operation_id,
                    url: event
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ClientError::InvalidResponse(
                                "completed Teleport notification omitted URL".to_owned(),
                            )
                        })?
                        .to_owned(),
                }),
                Some("failed" | "cancelled") => {
                    let error = event.get("error").cloned().unwrap_or_else(|| {
                        json!({
                            "message": "Teleport was cancelled",
                            "code": "teleport_cancelled",
                            "details": Value::Null,
                        })
                    });
                    Ok(ProgrammaticTeleportEvent::Failed {
                        operation_id,
                        error: serde_json::from_value(error).map_err(ClientError::Json)?,
                    })
                }
                Some(kind) => Err(ClientError::InvalidResponse(format!(
                    "unknown Teleport event kind `{kind}`"
                ))),
                None => Err(ClientError::InvalidResponse(
                    "Teleport notification omitted kind".to_owned(),
                )),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgrammaticUpdate {
    HistoryEntry {
        event_id: u64,
        emitted_at: u64,
        entry: Box<PublicHistoryEntry>,
    },
    Watermark {
        event_id: u64,
        emitted_at: u64,
    },
    /// Usage observed mid-turn, so context pressure is visible before the turn
    /// settles.
    Stats {
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
}

const MAX_PROGRAMMATIC_UPDATES: usize = 1_024;
const MAX_INTERACTIVE_CALLBACKS: usize = 8;
const MAX_INTERACTIVE_QUESTIONS: usize = 16;
const MAX_INTERACTIVE_OPTIONS_PER_QUESTION: usize = 32;
const MAX_INTERACTIVE_REQUEST_BYTES: usize = 64 * 1_024;

static NEXT_CLOUD_OPERATION: AtomicU64 = AtomicU64::new(1);

pub fn programmatic_update_channel(
    session_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(MAX_PROGRAMMATIC_UPDATES);
    (
        Arc::new(ProgrammaticEventObserver {
            reducer: Mutex::new(ProjectionReducer::new(session_id)),
            emitted: Mutex::new(BTreeMap::new()),
            sender,
            completed_only: true,
            next_update_id: AtomicU64::new(1),
        }),
        receiver,
    )
}

pub fn interactive_update_channel(
    session_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
) {
    interactive_update_channel_after(session_id, 0)
}

pub fn interactive_update_channel_after(
    session_id: impl Into<String>,
    event_id: u64,
) -> (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(MAX_PROGRAMMATIC_UPDATES);
    (
        Arc::new(ProgrammaticEventObserver {
            reducer: Mutex::new(ProjectionReducer::new(session_id)),
            emitted: Mutex::new(BTreeMap::new()),
            sender,
            completed_only: false,
            next_update_id: AtomicU64::new(event_id.saturating_add(1)),
        }),
        receiver,
    )
}

pub fn programmatic_update_channel_for_turn(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(MAX_PROGRAMMATIC_UPDATES);
    (
        Arc::new(ProgrammaticEventObserver {
            reducer: Mutex::new(ProjectionReducer::for_turn(session_id, turn_id)),
            emitted: Mutex::new(BTreeMap::new()),
            sender,
            completed_only: true,
            next_update_id: AtomicU64::new(1),
        }),
        receiver,
    )
}

struct ProgrammaticEventObserver {
    reducer: Mutex<ProjectionReducer>,
    emitted: Mutex<BTreeMap<String, Value>>,
    sender: tokio::sync::mpsc::Sender<ProgrammaticUpdate>,
    completed_only: bool,
    next_update_id: AtomicU64,
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
fn forward_stats(
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

fn public_history_entry_identity(entry: &PublicHistoryEntry) -> String {
    let metadata = entry.metadata();
    format!(
        "{}:{}",
        metadata.turn_id.as_deref().unwrap_or("session"),
        metadata.id
    )
}

struct ServerProjectionObserver {
    server: AppServer,
    session_id: String,
    turn_id: String,
    reducer: Mutex<ProjectionReducer>,
    emitted: Mutex<BTreeMap<String, Value>>,
    sender: tokio::sync::mpsc::Sender<ProgrammaticUpdate>,
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
fn decode_mcp_warnings(frames: &[Vec<u8>]) -> Result<Vec<String>, ClientError> {
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
    server: AppServer,
    connection: ServerConnection,
    next_request: i64,
    pending_mcp: BTreeMap<String, Vec<McpServerConfig>>,
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

    pub async fn configure_pending_mcp(&mut self, session_id: &str) -> Result<(), ClientError> {
        self.configure_pending_mcp_with_diagnostics(session_id)
            .await
            .map(drop)
    }

    async fn configure_pending_mcp_with_diagnostics(
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
        match batch.deferred.as_slice() {
            [
                DeferredWork::RunTurn {
                    session_id: deferred_session,
                    turn_id: deferred_turn,
                    prompt: deferred_prompt,
                    ..
                },
            ] if deferred_session == session_id
                && deferred_turn == &turn_id
                && deferred_prompt == &turn.prompt => {}
            _ => {
                return Err(ClientError::InvalidResponse(
                    "turn reservation omitted deferred work".to_owned(),
                ));
            }
        }
        let session = self.server.session(session_id)?;
        let tools = self.server.tool_registry(session_id)?;
        Ok(TurnReservation {
            session_id: session_id.to_owned(),
            turn_id,
            prompt: turn.prompt.clone(),
            input: turn.input.clone(),
            prepared_images: None,
            client_user_message_id: turn.client_user_message_id.clone(),
            auto_title: turn.auto_title.clone(),
            user_display_content: turn.user_display_content.clone(),
            mention_stats: turn.mention_stats.clone(),
            working_directory: session.working_directory,
            intent: session.intent,
            compaction: session.compaction,
            tools,
        })
    }

    fn reserve_due_loop(
        &mut self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledTurn>, ClientError> {
        let Some(scheduled) = self.server.reserve_due_loop(session_id, now_seconds)? else {
            return Ok(None);
        };
        let DeferredWork::RunTurn {
            session_id,
            turn_id,
            prompt,
            input,
            client_user_message_id,
            auto_title,
            user_display_content,
            mention_stats,
        } = scheduled.work
        else {
            return Err(ClientError::InvalidResponse(
                "scheduled loop did not reserve a turn".to_owned(),
            ));
        };
        let session = self.server.session(&session_id)?;
        let tools = self.server.tool_registry(&session_id)?;
        Ok(Some(ScheduledTurn {
            loop_id: scheduled.fire.loop_id,
            reservation: TurnReservation {
                session_id,
                turn_id,
                prompt,
                input,
                prepared_images: None,
                client_user_message_id,
                auto_title,
                user_display_content,
                mention_stats,
                working_directory: session.working_directory,
                intent: session.intent,
                compaction: session.compaction,
                tools,
            },
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

    fn reserve_compaction(
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

    fn finish_compaction(
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

    fn take_request_id(&mut self) -> RequestId {
        let id = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        RequestId::Integer(id)
    }
}

pub(crate) enum InteractiveCallbackRequest {
    Approval {
        session_id: String,
        request: ApprovalRequest,
        response: tokio::sync::oneshot::Sender<ApprovalDecision>,
    },
    Tool {
        session_id: String,
        title: String,
        detail: Value,
        response: tokio::sync::oneshot::Sender<Result<Value, String>>,
    },
    /// A tool asking for the transcript to be dropped and the session rotated
    /// at the running turn's next cycle boundary.
    ///
    /// It crosses the same channel as a callback because it needs the same
    /// thing a callback does: the identifier of the turn the server reserved,
    /// which a tool handler knows nothing about.
    ClearContext {
        session_id: String,
        continuation: String,
        plan_file_path: Option<String>,
        response: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

enum InteractiveCallbackResponse {
    Approval(tokio::sync::oneshot::Sender<ApprovalDecision>),
    Tool(tokio::sync::oneshot::Sender<Result<Value, String>>),
}

struct PendingInteractiveCallback {
    session_id: String,
    turn_id: String,
    response: InteractiveCallbackResponse,
}

struct InteractiveApprovalFactory {
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
}

impl ApprovalAgentFactory for InteractiveApprovalFactory {
    fn for_session(&self, session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveInteractiveRequest)
        } else {
            Arc::new(InteractiveApprovalAgent {
                session_id: session_id.to_owned(),
                sender: self.sender.clone(),
            })
        }
    }
}

struct ApproveInteractiveRequest;

impl ApprovalAgent for ApproveInteractiveRequest {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

struct InteractiveApprovalAgent {
    session_id: String,
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
}

impl ApprovalAgent for InteractiveApprovalAgent {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let (response, receiver) = tokio::sync::oneshot::channel();
            self.sender
                .send(InteractiveCallbackRequest::Approval {
                    session_id: self.session_id.clone(),
                    request,
                    response,
                })
                .await
                .map_err(|_| PolicyError::TurnCancelled)?;
            receiver.await.map_err(|_| PolicyError::TurnCancelled)
        })
    }
}

pub(crate) struct InteractiveSessionToolFactory {
    pub(crate) sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    pub(crate) plan_directory: Option<PathBuf>,
}

impl SessionToolFactory for InteractiveSessionToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        let question_sender = self.sender.clone();
        let question_session_id = session_id.to_owned();
        let question_handler: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let sender = question_sender.clone();
                let session_id = question_session_id.clone();
                let arguments = invocation.arguments.clone();
                Box::pin(
                    async move { run_interactive_questions(sender, session_id, arguments).await },
                )
            },
        );
        tools
            .register(interactive_question_spec(), question_handler)
            .map_err(|error| error.to_string())?;

        let Some(plan_directory) = self.plan_directory.as_deref() else {
            return Ok(());
        };
        let plan_sender = self.sender.clone();
        let plan_session_id = session_id.to_owned();
        let plan_path = plan_file_path(plan_directory, session_id);
        let plan_handler: Arc<dyn ToolHandler> = Arc::new(
            move |_invocation: &ToolInvocation,
                  _output: ToolOutputSink|
                  -> OwnedToolHandlerFuture {
                let sender = plan_sender.clone();
                let session_id = plan_session_id.clone();
                let plan_path = plan_path.clone();
                Box::pin(
                    async move { run_interactive_plan_review(sender, session_id, plan_path).await },
                )
            },
        );
        tools
            .register(interactive_plan_review_spec(), plan_handler)
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractiveQuestionRequest {
    questions: Vec<InteractiveQuestion>,
    #[serde(default)]
    footer_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractiveQuestion {
    question: String,
    #[serde(default)]
    header: String,
    options: Vec<InteractiveQuestionOption>,
    #[serde(default)]
    multi_select: bool,
    #[serde(default)]
    hide_other: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractiveQuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub struct HeadlessService<D> {
    client: InProcessClient,
    driver: Arc<D>,
    interactive_callbacks: Option<tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>>,
    interactive_backlog: VecDeque<InteractiveCallbackRequest>,
    pending_interactive_callbacks: HashMap<String, PendingInteractiveCallback>,
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
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT_CLOUD_OPERATION.fetch_add(1, Ordering::Relaxed);
    format!(
        "teleport-programmatic-{}-{timestamp}-{sequence}",
        std::process::id()
    )
}

fn approval_callback_detail(request: &ApprovalRequest) -> Value {
    // The approval presents the effect it is gating, so the detail carries the
    // same typed shape the effect entry will publish once the call is allowed.
    let mut effect = EffectDetail::for_call(&request.tool, &request.input);
    effect.display.content = Some(request.rationale.clone());
    json!({
        "kind": "approval",
        "effect": effect,
        // Reference `ApprovalCallbackDetail.required_permissions` is a list of
        // the requirement model itself, not of its labels: the client needs the
        // scope and the two patterns to render what an "allow always" would
        // grant.
        "requiredPermissions": request.requirements,
        "choices": [
            "approve",
            "approve_for_session",
            "approve_permanently",
            "deny",
            "cancel_turn",
        ],
        "relatedEntryId": null,
    })
}

fn approval_decision_from_output(output: &Value) -> Result<ApprovalDecision, ClientError> {
    if output.get("type").and_then(Value::as_str) != Some("approval") {
        return Err(ClientError::InvalidResponse(
            "approval callback returned a non-approval output".to_owned(),
        ));
    }
    match output.pointer("/decision/type").and_then(Value::as_str) {
        Some("approve") => Ok(ApprovalDecision::ApproveOnce),
        Some("approve_for_session") => Ok(ApprovalDecision::ApproveForSession),
        Some("approve_permanently") => Ok(ApprovalDecision::ApprovePermanently),
        Some("deny") => Ok(ApprovalDecision::Deny),
        Some("cancel_turn") => Ok(ApprovalDecision::CancelTurn),
        Some(decision) => Err(ClientError::InvalidResponse(format!(
            "unknown approval decision `{decision}`"
        ))),
        None => Err(ClientError::InvalidResponse(
            "approval callback omitted its decision".to_owned(),
        )),
    }
}

fn interactive_request_session_id(request: &InteractiveCallbackRequest) -> &str {
    match request {
        InteractiveCallbackRequest::Approval { session_id, .. }
        | InteractiveCallbackRequest::Tool { session_id, .. }
        | InteractiveCallbackRequest::ClearContext { session_id, .. } => session_id,
    }
}

fn reject_interactive_request(request: InteractiveCallbackRequest, message: &str) {
    match request {
        InteractiveCallbackRequest::Approval { response, .. } => {
            let _ = response.send(ApprovalDecision::CancelTurn);
        }
        InteractiveCallbackRequest::Tool { response, .. } => {
            let _ = response.send(Err(message.to_owned()));
        }
        InteractiveCallbackRequest::ClearContext { response, .. } => {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

fn retain_interactive_request(
    backlog: &mut VecDeque<InteractiveCallbackRequest>,
    request: InteractiveCallbackRequest,
) {
    if backlog.len() < MAX_INTERACTIVE_CALLBACKS {
        backlog.push_back(request);
    } else {
        reject_interactive_request(request, "interactive callback backlog is full");
    }
}

fn fail_pending_interactive_callback(pending: Option<PendingInteractiveCallback>, message: &str) {
    if let Some(pending) = pending {
        fail_interactive_response(pending.response, message);
    }
}

fn fail_interactive_response(response: InteractiveCallbackResponse, message: &str) {
    match response {
        InteractiveCallbackResponse::Approval(response) => {
            let _ = response.send(ApprovalDecision::CancelTurn);
        }
        InteractiveCallbackResponse::Tool(response) => {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

async fn run_interactive_questions(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    arguments: Value,
) -> Result<ToolExecutionOutput, ToolError> {
    let request_bytes = serde_json::to_vec(&arguments)
        .map_err(|error| ToolError::Execution(format!("invalid question request: {error}")))?
        .len();
    if request_bytes > MAX_INTERACTIVE_REQUEST_BYTES {
        return Err(ToolError::Execution(format!(
            "question request exceeds {MAX_INTERACTIVE_REQUEST_BYTES} bytes"
        )));
    }
    let request = serde_json::from_value::<InteractiveQuestionRequest>(arguments)
        .map_err(|error| ToolError::Execution(format!("invalid question request: {error}")))?;
    validate_interactive_question_request(&request)?;
    let questions = request.questions.clone();
    let title = request
        .questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            if question.header.is_empty() {
                format!("{}. {}", index + 1, question.question)
            } else {
                format!("{}. {}: {}", index + 1, question.header, question.question)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let detail = json!({
        "kind": "user_input",
        "request": {
            "questions": questions,
            "footerNote": request.footer_note.clone(),
        },
        "relatedEntryId": null,
    });
    let output = request_interactive_tool_callback(sender, session_id, title, detail).await?;
    question_tool_output(&output, &request.questions)
}

fn validate_interactive_question_request(
    request: &InteractiveQuestionRequest,
) -> Result<(), ToolError> {
    if request.questions.is_empty() || request.questions.len() > MAX_INTERACTIVE_QUESTIONS {
        return Err(ToolError::Execution(format!(
            "question count must be between 1 and {MAX_INTERACTIVE_QUESTIONS}"
        )));
    }
    for question in &request.questions {
        if question.question.trim().is_empty() {
            return Err(ToolError::Execution(
                "question text must not be empty".to_owned(),
            ));
        }
        if question.header.chars().count() > 20 {
            return Err(ToolError::Execution(
                "question header exceeds 20 characters".to_owned(),
            ));
        }
        if question.options.len() < 2
            || question.options.len() > MAX_INTERACTIVE_OPTIONS_PER_QUESTION
        {
            return Err(ToolError::Execution(format!(
                "each question must have between 2 and \
                 {MAX_INTERACTIVE_OPTIONS_PER_QUESTION} options"
            )));
        }
        if question
            .options
            .iter()
            .any(|option| option.label.trim().is_empty())
        {
            return Err(ToolError::Execution(
                "question option labels must not be empty".to_owned(),
            ));
        }
    }
    Ok(())
}

async fn request_interactive_tool_callback(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    title: String,
    detail: Value,
) -> Result<Value, ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::Tool {
            session_id,
            title,
            detail,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("interactive callback was abandoned".to_owned()))?
        .map_err(ToolError::Execution)
}

/// Asks the surface driving the turn to clear the transcript and rotate the
/// session, and waits for it to be queued.
///
/// Waiting is what makes the answer honest: the tool reports a cleared context
/// only once the turn actually holds the control that clears it.
async fn request_context_clearing(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    continuation: String,
    plan_file_path: Option<String>,
) -> Result<(), ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::ClearContext {
            session_id,
            continuation,
            plan_file_path,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("context clearing was abandoned".to_owned()))?
        .map_err(ToolError::Execution)
}

fn question_tool_output(
    output: &Value,
    questions: &[InteractiveQuestion],
) -> Result<ToolExecutionOutput, ToolError> {
    let (answers, cancelled) = user_input_result(output)?;
    if cancelled {
        if !answers.is_empty() {
            return Err(ToolError::Execution(
                "cancelled question response included answers".to_owned(),
            ));
        }
        return Ok(ToolExecutionOutput {
            typed_result: json!({"answers": [], "cancelled": true}),
            model_text: "The user cancelled the question.".to_owned(),
            display: json!({"kind": "user_question"}),
            chunks: Vec::new(),
        });
    }
    if answers.len() != questions.len() {
        return Err(ToolError::Execution(format!(
            "expected {} question answers, received {}",
            questions.len(),
            answers.len()
        )));
    }
    let mut model_lines = Vec::with_capacity(answers.len());
    for (answer, question) in answers.iter().zip(questions) {
        let returned_question = required_output_string(answer, "question")?;
        let answer_text = required_output_string(answer, "answer")?;
        let is_other = answer
            .get("isOther")
            .and_then(Value::as_bool)
            .ok_or_else(|| ToolError::Execution("question answer omitted isOther".to_owned()))?;
        if returned_question != question.question {
            return Err(ToolError::Execution(
                "question answer does not match the request".to_owned(),
            ));
        }
        validate_interactive_answer(question, answer_text, is_other)?;
        model_lines.push(format!("{}: {answer_text}", question.question));
    }
    Ok(ToolExecutionOutput {
        typed_result: output
            .get("result")
            .cloned()
            .ok_or_else(|| ToolError::Execution("question output omitted result".to_owned()))?,
        model_text: model_lines.join("\n"),
        display: json!({"kind": "user_question"}),
        chunks: Vec::new(),
    })
}

fn validate_interactive_answer(
    question: &InteractiveQuestion,
    answer: &str,
    is_other: bool,
) -> Result<(), ToolError> {
    if answer.trim().is_empty() {
        return Err(ToolError::Execution(
            "question answer must not be empty".to_owned(),
        ));
    }
    if is_other {
        return if question.hide_other {
            Err(ToolError::Execution(
                "question does not allow a free-text answer".to_owned(),
            ))
        } else {
            Ok(())
        };
    }
    let labels = answer.split(", ").collect::<Vec<_>>();
    if (!question.multi_select && labels.len() != 1)
        || labels
            .iter()
            .any(|label| !question.options.iter().any(|option| option.label == *label))
        || labels
            .iter()
            .enumerate()
            .any(|(index, label)| labels[..index].contains(label))
    {
        return Err(ToolError::Execution(
            "question answer selected an invalid option".to_owned(),
        ));
    }
    Ok(())
}

async fn run_interactive_plan_review(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    plan_path: PathBuf,
) -> Result<ToolExecutionOutput, ToolError> {
    const QUESTION: &str = "Plan is complete. Switch to code mode and start implementing?";
    let detail = json!({
        "kind": "user_input",
        "request": {
            "questions": [{
                "question": QUESTION,
                "header": "Plan ready",
                "options": interactive_plan_options(),
                "multiSelect": false,
                "hideOther": false,
            }],
            "footerNote": null,
        },
        "planReview": true,
        "filePath": plan_path,
        "relatedEntryId": null,
    });
    let output = request_interactive_tool_callback(
        sender.clone(),
        session_id.clone(),
        QUESTION.to_owned(),
        detail,
    )
    .await?;
    let (answers, cancelled) = user_input_result(&output)?;
    let (switched, clear_context, message) = if cancelled {
        if !answers.is_empty() {
            return Err(ToolError::Execution(
                "cancelled plan review included an answer".to_owned(),
            ));
        }
        (
            false,
            false,
            "User cancelled. Staying in plan mode.".to_owned(),
        )
    } else {
        if answers.len() != 1 {
            return Err(ToolError::Execution(
                "plan review requires exactly one answer".to_owned(),
            ));
        }
        let answer = &answers[0];
        if required_output_string(answer, "question")? != QUESTION {
            return Err(ToolError::Execution(
                "plan review answer does not match the request".to_owned(),
            ));
        }
        let value = required_output_string(answer, "answer")?;
        if answer.get("isOther").and_then(Value::as_bool) == Some(true) {
            if value.trim().is_empty() {
                return Err(ToolError::Execution(
                    "plan feedback must not be empty".to_owned(),
                ));
            }
            (
                false,
                false,
                format!("Stay in plan mode and incorporate this feedback: {value}"),
            )
        } else {
            match value {
                "Yes, clear context and auto approve edits" => (
                    true,
                    true,
                    "Plan approved. Switch to code mode, clear planning context, and auto approve \
                     edits."
                        .to_owned(),
                ),
                "Yes, and auto approve edits" => (
                    true,
                    false,
                    "Plan approved. Switch to code mode and auto approve edits.".to_owned(),
                ),
                "Yes, and request approval for edits" => (
                    true,
                    false,
                    "Plan approved. Switch to code mode and request approval for edits.".to_owned(),
                ),
                "No" => (
                    false,
                    false,
                    "Plan rejected. Stay in plan mode and continue refining it.".to_owned(),
                ),
                _ => {
                    return Err(ToolError::Execution(
                        "plan review selected an invalid option".to_owned(),
                    ));
                }
            }
        }
    };
    if clear_context {
        // The clearing lands at the next cycle boundary, so the message this
        // tool answers with is also the only instruction that survives it.
        request_context_clearing(
            sender,
            session_id,
            message.clone(),
            plan_path.to_str().map(str::to_owned),
        )
        .await?;
    }
    Ok(ToolExecutionOutput {
        typed_result: json!({"switched": switched, "message": message}),
        model_text: message,
        display: json!({"kind": "plan_review", "switched": switched}),
        chunks: Vec::new(),
    })
}

fn plan_file_path(plan_directory: &Path, session_id: &str) -> PathBuf {
    let safe_session = session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    plan_directory.join(format!("{safe_session}.md"))
}

fn user_input_result(output: &Value) -> Result<(&[Value], bool), ToolError> {
    if output.get("type").and_then(Value::as_str) != Some("user_input") {
        return Err(ToolError::Execution(
            "interactive tool returned a non-user-input output".to_owned(),
        ));
    }
    let result = output
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| ToolError::Execution("user-input output omitted result".to_owned()))?;
    let answers = result
        .get("answers")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::Execution("user-input output omitted answers".to_owned()))?;
    let cancelled = result
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ToolError::Execution("user-input output omitted cancelled".to_owned()))?;
    Ok((answers, cancelled))
}

fn required_output_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Execution(format!("interactive answer omitted string `{key}`")))
}

fn interactive_plan_options() -> Vec<Value> {
    [
        (
            "Yes, clear context and auto approve edits",
            "Clear planning context, switch to code mode, and auto approve edits",
        ),
        (
            "Yes, and auto approve edits",
            "Switch to code mode with auto-approved edits",
        ),
        (
            "Yes, and request approval for edits",
            "Switch to code mode and keep edit approvals",
        ),
        ("No", "Stay in plan mode and continue planning"),
    ]
    .into_iter()
    .map(|(label, description)| json!({"label": label, "description": description}))
    .collect()
}

/// Directive coverage for `ask_user_question`, whose reference description
/// this port must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The question carries structured options rather than free prose | "structured options" in the description |
/// | One to four questions per call | "one to four questions" |
/// | Each question has a header, a question text, and its options | the property descriptions plus `minItems` |
/// | Two to four options per question | "two to four options", `minItems: 2` |
/// | A free-text "Other" option is appended by the client | "an Other free-text choice is appended" |
///
/// The argument shape comes from the reference `UserQuestionRequest`, which is
/// the one reference model configuring `alias_generator=to_camel` alongside
/// `extra="forbid"`: its properties are camelCase where every other reference
/// tool stays snake_case.
fn interactive_question_spec() -> ToolSpec {
    let choice = ObjectSchema::new()
        .required(
            "label",
            Property::string().described("A one to five word label for the choice"),
        )
        .optional(
            "description",
            Property::string()
                .described("An optional expansion of what the choice means")
                .with_default(""),
        )
        .forbid_extra_properties();
    let question = ObjectSchema::new()
        .required(
            "question",
            Property::string().described("The text of the question"),
        )
        .required(
            "options",
            Property::array(Property::reference("QuestionChoice"))
                .constrained("minItems", 2)
                .described(
                    "The choices offered, two to four of them, not counting the Other free-text \
                     choice the client appends.",
                ),
        )
        .optional(
            "header",
            Property::string()
                .constrained("maxLength", 20)
                .described("A short chip label for the question, at most 20 characters")
                .with_default(""),
        )
        .optional(
            "hideOther",
            Property::boolean()
                .described("When true, the Other free-text choice is withheld")
                .with_default(false),
        )
        .optional(
            "multiSelect",
            Property::boolean()
                .described("When true, several choices may be selected at once")
                .with_default(false),
        )
        .forbid_extra_properties();
    ToolSpec {
        name: "ask_user_question".to_owned(),
        description: "Put a question to the user with structured options. Send one to four \
                      questions, each carrying a header, its question text, and two to four \
                      options; an Other free-text choice is appended for you."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .define("QuestionChoice", choice)
            .define("UserQuestion", question)
            .required(
                "questions",
                Property::array(Property::reference("UserQuestion"))
                    .constrained("minItems", 1)
                    .described(
                        "The questions to put, one to four of them. Several questions render as \
                         tabs.",
                    ),
            )
            .optional(
                "footerNote",
                Property::string()
                    .described("An optional quiet note rendered under the question widget.")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .forbid_extra_properties()
            .build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 50,
    }
}

/// Directive coverage for `exit_plan_mode`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The call announces a finished plan ready to implement | "the plan is finished and ready to implement" |
/// | Call it only once the plan is final | "Call it only once the plan is final" |
/// | Do not call it while planning continues | "never while planning is still under way or the user wants to keep planning" |
fn interactive_plan_review_spec() -> ToolSpec {
    ToolSpec {
        name: "exit_plan_mode".to_owned(),
        description: "Announce that the plan is finished and ready to implement. Call it only \
                      once the plan is final, never while planning is still under way or the \
                      user wants to keep planning."
            .to_owned(),
        input_schema: ObjectSchema::new().build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 50,
    }
}

#[derive(Debug, Clone)]
pub struct LiveDriverConfig {
    pub style: String,
    pub endpoint: String,
    pub model: String,
    pub credential_environment: String,
    pub system_prompt: String,
    pub session_root: Option<PathBuf>,
    pub input_price_per_million_micros: u64,
    pub output_price_per_million_micros: u64,
    /// The three compaction texts this process summarizes under, already
    /// resolved through `compaction_prompt_id`.
    pub compaction_prompts: CompactionPromptResolution,
}

/// One `context/inject` entry waiting for the next turn, carrying the wire
/// flag that decides whether a skill invocation in it appends the synthetic
/// pair when the entry becomes a message.
#[derive(Debug, Clone)]
struct PendingContext {
    content: String,
    inject_invoked_skill: bool,
}

pub struct LiveTurnDriver {
    provider: Arc<dyn CompletionProvider>,
    compactor: ProviderSessionCompactor,
    system_prompt: String,
    session_root: Option<PathBuf>,
    input_price_per_million_micros: u64,
    output_price_per_million_micros: u64,
    controls: Mutex<HashMap<(String, String), LiveTurnControl>>,
    pending_context: Mutex<HashMap<String, Vec<PendingContext>>>,
    /// The context-warning policy each session latches on.
    ///
    /// The engine is rebuilt for every turn, so a policy that speaks once per
    /// session cannot live on it. It lives here, where it outlives the turns,
    /// and the engine borrows it for the length of each one.
    context_warnings: Mutex<HashMap<String, Arc<ContextWarningMiddleware>>>,
    event_observer: Arc<dyn EventObserver>,
}

/// The provider-bound half of compaction: it mints the identifier the compacted
/// session continues under and hands everything else to the core manager.
///
/// The summarization itself lives one layer down, in
/// [`vibe_core::compaction::manager`], because it is provider-neutral: the call
/// shape, the failure taxonomy, the fallback and the retry ladder are the same
/// whichever backend answers, and keeping them there is what lets the compaction
/// corpus drive them with a scripted provider.
#[derive(Clone)]
struct ProviderSessionCompactor {
    provider: Arc<dyn CompletionProvider>,
    /// The prompts, the model, the tools and the strict flag this session
    /// summarizes under.
    plan: Arc<CompactionPlan>,
}

impl ProviderSessionCompactor {
    fn new(provider: Arc<dyn CompletionProvider>) -> Self {
        Self {
            provider,
            plan: Arc::new(CompactionPlan::default()),
        }
    }

    /// The same compactor, summarizing under `plan`.
    fn with_plan(&self, plan: CompactionPlan) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            plan: Arc::new(plan),
        }
    }

    async fn compact_with_instructions(
        &self,
        current_session_id: &str,
        messages: &[ModelMessage],
        extra_instructions: &str,
    ) -> Result<CompactionResult, CompactionFailure> {
        let summarized = compaction_manager::compact(
            self.provider.as_ref(),
            &self.plan,
            messages,
            extra_instructions.trim(),
        )
        .await?;
        Ok(CompactionResult {
            new_session_id: rotate_session_id(current_session_id),
            summary: summarized.summary,
            messages: summarized.messages,
            usage: summarized.usage,
            failure: summarized.failure,
        })
    }

    /// The plan one session summarizes under: this process's resolved prompts,
    /// with the model, the strict flag and the live tool surface the session
    /// itself carries.
    fn session_plan(
        &self,
        settings: &CompactionSettings,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
        thinking: bool,
    ) -> CompactionPlan {
        CompactionPlan {
            prompts: self.plan.prompts.clone(),
            model: settings.compaction_model.clone(),
            thinking,
            tools,
            tool_choice,
            strict: settings.raise_on_compaction_failure,
            ..CompactionPlan::default()
        }
    }
}

impl Compactor for ProviderSessionCompactor {
    fn compact<'a>(
        &'a self,
        current_session_id: &'a str,
        messages: &'a [ModelMessage],
    ) -> vibe_core::engine::CompactionFuture<'a> {
        Box::pin(async move {
            self.compact_with_instructions(current_session_id, messages, "")
                .await
        })
    }

    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
        Ok(rotate_session_id(current_session_id))
    }
}

/// The session's own view of the registry: the two configured filters, plus the
/// exact names a subagent is confined to.
///
/// The filters are compiled once here rather than per call, because the
/// reference matching rules cover globs and regular expressions and a turn
/// consults them for every published name and again for every call.
#[derive(Clone)]
struct SessionToolExecutor {
    tools: ToolRegistry,
    enabled: NameFilter,
    disabled: NameFilter,
    allowed: Option<BTreeSet<String>>,
}

impl SessionToolExecutor {
    fn new(tools: ToolRegistry, intent: &SessionIntent) -> Self {
        Self {
            tools,
            enabled: NameFilter::new(&intent.enabled_tools),
            disabled: NameFilter::new(&intent.disabled_tools),
            allowed: None,
        }
    }

    fn with_allowed_tools(mut self, allowed: BTreeSet<String>) -> Self {
        self.allowed = Some(allowed);
        self
    }

    fn permits(&self, name: &str) -> bool {
        (self.enabled.is_empty() || self.enabled.matches(name))
            && !self.disabled.matches(name)
            && self
                .allowed
                .as_ref()
                .is_none_or(|allowed| allowed.contains(name))
    }

    fn definitions(&self) -> Result<Vec<ToolDefinition>, DriverError> {
        self.tools
            .available(&self.enabled, &self.disabled)
            .map_err(|error| DriverError::Tool(error.to_string()))
            .map(|definitions| {
                definitions
                    .into_iter()
                    .filter(|definition| self.permits(&definition.name))
                    .map(|spec| ToolDefinition {
                        name: spec.name,
                        description: spec.description,
                        input_schema: spec.input_schema,
                    })
                    .collect()
            })
    }
}

impl ToolExecutor for SessionToolExecutor {
    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a> {
        if !self.permits(name) {
            return Box::pin(
                async move { Err(format!("tool `{name}` is disabled for this session")) },
            );
        }
        self.tools.execute(name, arguments)
    }

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        if !self.permits(name) {
            return Box::pin(
                async move { Err(format!("tool `{name}` is disabled for this session")) },
            );
        }
        self.tools.execute_stream(name, arguments, output)
    }
}

#[derive(Debug, Clone, Default)]
struct LiveTurnControl {
    cancellation: CancellationToken,
    engine: TurnControlHandle,
}

impl LiveTurnDriver {
    #[cfg(any(test, feature = "test-fixtures"))]
    #[must_use]
    pub fn from_provider_for_tests(
        provider: Arc<dyn CompletionProvider>,
        system_prompt: impl Into<String>,
    ) -> Self {
        let compactor = ProviderSessionCompactor::new(provider.clone());
        Self {
            provider,
            compactor,
            system_prompt: system_prompt.into(),
            session_root: None,
            input_price_per_million_micros: 0,
            output_price_per_million_micros: 0,
            controls: Mutex::new(HashMap::new()),
            pending_context: Mutex::new(HashMap::new()),
            context_warnings: Mutex::new(HashMap::new()),
            event_observer: Arc::new(NoopEventObserver),
        }
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    #[must_use]
    pub fn with_session_root_for_tests(mut self, session_root: Option<PathBuf>) -> Self {
        self.session_root = session_root;
        self
    }

    /// Builds the driver from the ambient credential: the process environment
    /// first, then the variables `dotenv` read from the global file.
    pub fn from_environment(
        config: LiveDriverConfig,
        dotenv: &vibe_core::config::DotenvValues,
    ) -> Result<Self, DriverError> {
        let credential = dotenv
            .variable(&config.credential_environment)
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                DriverError::MissingCredentialEnvironment(config.credential_environment.clone())
            })?;
        Self::from_credential(config, credential)
    }

    pub fn from_credential(
        config: LiveDriverConfig,
        credential: String,
    ) -> Result<Self, DriverError> {
        let style = ProviderStyle::parse(&config.style).map_err(DriverError::Provider)?;
        if credential.is_empty() {
            return Err(DriverError::MissingCredentialEnvironment(
                config.credential_environment,
            ));
        }
        let transport = HttpTransport::new().map_err(DriverError::Transport)?;
        let provider = ProviderBackend::new(
            style,
            config.endpoint,
            config.model,
            SecretString::from(credential),
            transport,
        );
        let provider: Arc<dyn CompletionProvider> = Arc::new(provider);
        let compactor = ProviderSessionCompactor::new(provider.clone()).with_plan(CompactionPlan {
            prompts: config.compaction_prompts,
            ..CompactionPlan::default()
        });
        let session_root = config.session_root.or_else(default_session_root);
        Ok(Self {
            provider,
            compactor,
            system_prompt: config.system_prompt,
            session_root,
            input_price_per_million_micros: config.input_price_per_million_micros,
            output_price_per_million_micros: config.output_price_per_million_micros,
            controls: Mutex::new(HashMap::new()),
            pending_context: Mutex::new(HashMap::new()),
            context_warnings: Mutex::new(HashMap::new()),
            event_observer: Arc::new(NoopEventObserver),
        })
    }

    #[must_use]
    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = observer;
        self
    }

    /// Serves the completions MCP servers ask this client for.
    ///
    /// Reference `_create_sampling_handler` builds one from the loop's backend
    /// and the active model, so a server that asks is answered by the same
    /// provider the turn itself uses rather than by a second one configured
    /// beside it.
    #[must_use]
    pub fn sampling_handler(&self, model: impl Into<String>) -> Arc<dyn SamplingHandler> {
        Arc::new(ProviderSamplingHandler {
            provider: Arc::clone(&self.provider),
            model: model.into(),
        })
    }

    async fn run_engine(
        &self,
        reservation: &TurnReservation,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
        observer: Arc<dyn EventObserver>,
    ) -> Result<TurnOutcome, DriverError> {
        let observer: Arc<dyn EventObserver> = Arc::new(CompositeEventObserver::new(
            observer,
            Arc::clone(&self.event_observer),
        ));
        // Reference `self.agent_profile.name`, which every request and tool
        // event reports and which defaults to the built-in profile.
        let agent_profile = reservation
            .intent
            .agent
            .clone()
            .unwrap_or_else(|| vibe_core::engine::DEFAULT_AGENT_PROFILE.to_owned());
        let limits = EngineLimits {
            max_steps: reservation.intent.max_turns.unwrap_or(20),
            max_total_tokens: reservation.intent.max_tokens.unwrap_or(200_000),
            max_price_micros: reservation.intent.max_price_micros.unwrap_or(u64::MAX),
            input_price_per_million_micros: self.input_price_per_million_micros,
            output_price_per_million_micros: self.output_price_per_million_micros,
            ..EngineLimits::default()
        };
        let mut input = ProviderInput {
            turn_id: Some(reservation.turn_id.clone()),
            model_override: reservation.intent.model.clone(),
            messages: vec![ModelMessage::System {
                content: self.system_prompt.clone(),
            }],
            stream: true,
            images: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            thinking: reservation.intent.thinking,
            reasoning_effort: reservation.intent.reasoning_effort.clone(),
            headers: BTreeMap::new(),
            limits: RequestLimits {
                max_tokens: reservation
                    .intent
                    .max_tokens
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(4096),
                temperature_millis: None,
                max_response_bytes: limits.max_response_bytes,
            },
            metadata: BTreeMap::new(),
        };
        if reservation.intent.mode.as_deref() == Some("plan") {
            let plan_path = self
                .plan_directory()
                .map(|directory| plan_file_path(&directory, &reservation.session_id))
                .ok_or_else(|| DriverError::Tool("plan file root is unavailable".to_owned()))?;
            if let Some(parent) = plan_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| DriverError::Tool(error.to_string()))?;
            }
            tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&plan_path)
                .await
                .map_err(|error| DriverError::Tool(error.to_string()))?;
            input.messages.push(ModelMessage::System {
                content: format!(
                    "Plan mode is active. Inspect and reason, but do not mutate the workspace. \
                     Keep the live plan at {} updated as you plan. That plan file is the only \
                     file you may write while plan mode is active.",
                    plan_path.display()
                ),
            });
        }
        if let Some(profile_prompt) = reservation
            .intent
            .system_prompt_id
            .as_deref()
            .and_then(crate::builtin_agents::system_prompt)
        {
            input.messages.push(ModelMessage::System {
                content: profile_prompt.to_owned(),
            });
        }
        let session_tools =
            SessionToolExecutor::new(reservation.tools.clone(), &reservation.intent);
        for (key, value) in [
            (
                "client_user_message_id",
                reservation
                    .client_user_message_id
                    .as_ref()
                    .map(|value| json!(value)),
            ),
            (
                "auto_title",
                reservation.auto_title.as_ref().map(|value| json!(value)),
            ),
            (
                "user_display_content",
                reservation.user_display_content.clone(),
            ),
            ("mention_stats", reservation.mention_stats.clone()),
        ] {
            if let Some(value) = value {
                input.metadata.insert(key.to_owned(), value.to_string());
            }
        }
        input.images = match &reservation.prepared_images {
            Some(images) => images.as_slice().to_vec(),
            None => provider_images(&reservation.input)
                .await?
                .as_slice()
                .to_vec(),
        };
        let mut pending_context = self
            .pending_context
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?
            .remove(&reservation.session_id);
        // Registered after automatic compaction and before nothing, which is
        // where `_setup_middleware` puts it: a cycle that reached the threshold
        // compacts instead of warning about a window it is about to replace.
        let context_warning =
            self.context_warning(&reservation.session_id, &reservation.compaction)?;
        if let Some(root) = &self.session_root {
            let store = SessionStore::new(root);
            let hydrated = if let Some(selector) = &reservation.intent.resume {
                Some(
                    store
                        .resume(selector, &self.system_prompt, BTreeMap::new())
                        .map_err(DriverError::Storage)?,
                )
            } else if reservation.intent.continue_session {
                Some(
                    store
                        .continue_session(
                            &reservation.working_directory,
                            &self.system_prompt,
                            BTreeMap::new(),
                        )
                        .map_err(DriverError::Storage)?,
                )
            } else {
                match store.resume(
                    &reservation.session_id,
                    &self.system_prompt,
                    BTreeMap::new(),
                ) {
                    Ok(hydrated) => Some(hydrated),
                    Err(vibe_core::storage::StorageError::SessionNotFound(_)) => None,
                    Err(error) => return Err(DriverError::Storage(error)),
                }
            };
            let (metadata, engine_session_id) = match hydrated {
                Some(hydrated) => {
                    input.messages = hydrated.messages;
                    let session_id = hydrated.metadata.id.clone();
                    (hydrated.metadata, session_id)
                }
                None => {
                    let metadata = store
                        .create(
                            &reservation.session_id,
                            &reservation.working_directory,
                            None,
                            now_millis()?,
                        )
                        .map_err(DriverError::Storage)?;
                    (metadata, reservation.session_id.clone())
                }
            };
            self.register_task_tool(reservation, store.clone(), engine_session_id.clone())?;
            input.tools = session_tools.definitions()?;
            let baseline = session_stats(&metadata);
            if let Some(context) = pending_context.take() {
                for entry in context {
                    input
                        .messages
                        .push(ModelMessage::user(entry.content.clone()));
                    if entry.inject_invoked_skill
                        && let Some(resolver) = reservation.tools.invoked_skills()
                        && let Some(invoked) = resolver.resolve(&entry.content)
                    {
                        vibe_core::skills::append_invoked_skill(&mut input.messages, &invoked);
                    }
                }
            }
            input.messages.extend(
                resource_contexts(reservation)
                    .into_iter()
                    .map(ModelMessage::user),
            );
            let mut engine = ConversationEngine::new(Arc::clone(&self.provider))
                .with_tools(session_tools.clone())
                .with_compactor(self.compactor.with_plan(self.compactor.session_plan(
                    &reservation.compaction,
                    input.tools.clone(),
                    input.tool_choice.clone(),
                    input.thinking,
                )))
                .with_sink(SessionTranscriptSink::new(store, metadata))
                .with_limits(limits)
                .with_baseline(baseline)
                .with_compaction_settings(reservation.compaction.clone())
                .with_agent_profile(agent_profile.clone())
                .with_observer(observer);
            if let Some(warning) = context_warning {
                engine = engine.with_middleware(warning);
            }
            if let Some(resolver) = reservation.tools.invoked_skills() {
                engine = engine.with_invoked_skills(resolver);
            }
            engine
                .run_turn_controlled(
                    engine_session_id,
                    input,
                    &reservation.prompt,
                    cancellation,
                    controls,
                )
                .await
                .map_err(DriverError::Engine)
        } else {
            input.tools = session_tools.definitions()?;
            if let Some(context) = pending_context.take() {
                for entry in context {
                    input
                        .messages
                        .push(ModelMessage::user(entry.content.clone()));
                    if entry.inject_invoked_skill
                        && let Some(resolver) = reservation.tools.invoked_skills()
                        && let Some(invoked) = resolver.resolve(&entry.content)
                    {
                        vibe_core::skills::append_invoked_skill(&mut input.messages, &invoked);
                    }
                }
            }
            input.messages.extend(
                resource_contexts(reservation)
                    .into_iter()
                    .map(ModelMessage::user),
            );
            let mut engine = ConversationEngine::new(Arc::clone(&self.provider))
                .with_tools(session_tools)
                .with_compactor(self.compactor.with_plan(self.compactor.session_plan(
                    &reservation.compaction,
                    input.tools.clone(),
                    input.tool_choice.clone(),
                    input.thinking,
                )))
                .with_limits(limits)
                .with_compaction_settings(reservation.compaction.clone())
                .with_agent_profile(agent_profile.clone())
                .with_observer(observer);
            if let Some(warning) = context_warning {
                engine = engine.with_middleware(warning);
            }
            if let Some(resolver) = reservation.tools.invoked_skills() {
                engine = engine.with_invoked_skills(resolver);
            }
            engine
                .run_turn_controlled(
                    &reservation.session_id,
                    input,
                    &reservation.prompt,
                    cancellation,
                    controls,
                )
                .await
                .map_err(DriverError::Engine)
        }
    }

    fn register_task_tool(
        &self,
        reservation: &TurnReservation,
        store: SessionStore,
        parent_session_id: String,
    ) -> Result<(), DriverError> {
        let built_in = AgentProfile {
            name: "explore".to_owned(),
            display_name: "Explore".to_owned(),
            description: "Inspect a bounded task in an independent child session".to_owned(),
            kind: AgentKind::Subagent,
            safety: "read_only".to_owned(),
            overrides: toml::Table::new(),
            source: ExtensionSource::Builtin,
            path: None,
        };
        let vibe_home = crate::host::vibe_home();
        let catalog = discover_extensions(
            &DiscoveryRoots {
                configured: Vec::new(),
                project: vec![PathBuf::from(&reservation.working_directory).join(".vibe")],
                user: vec![vibe_home.join("extensions")],
                project_trusted: reservation.intent.trusted,
                // Only the agent profiles are read here, so no skill root is
                // resolved and no skill is walked.
                ..DiscoveryRoots::default()
            },
            BTreeMap::from([(built_in.name.clone(), built_in)]),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let subagents = catalog
            .agents
            .into_iter()
            .filter(|(_, profile)| profile.kind == AgentKind::Subagent)
            .collect::<BTreeMap<_, _>>();
        let subagent_names = Arc::new(subagents.keys().cloned().collect::<Vec<_>>());
        let runner = Arc::new(ProviderSubagentRunner {
            provider: self.provider.clone(),
            system_prompt: self.system_prompt.clone(),
            store: store.clone(),
            tools: reservation.tools.clone(),
            input_price_per_million_micros: self.input_price_per_million_micros,
            output_price_per_million_micros: self.output_price_per_million_micros,
            parent_intent: reservation.intent.clone(),
        });
        let manager = Arc::new(SubagentManager::new(store, runner));
        let handler: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &vibe_core::tools::ToolInvocation,
                  _output: vibe_core::tools::ToolOutputSink|
                  -> OwnedToolHandlerFuture {
                let manager = manager.clone();
                let subagents = subagents.clone();
                let subagent_names = subagent_names.clone();
                let parent_session_id = parent_session_id.clone();
                let arguments = invocation.arguments.clone();
                Box::pin(async move {
                    // `agent` carries the reference default, applied before the
                    // handler runs, so an absent key still names `explore`.
                    let agent_name = arguments
                        .get("agent")
                        .and_then(Value::as_str)
                        .unwrap_or(DEFAULT_SUBAGENT);
                    let task = arguments
                        .get("task")
                        .and_then(Value::as_str)
                        .filter(|task| !task.is_empty())
                        .ok_or_else(|| vibe_core::tools::ToolError::SchemaViolation {
                            path: "/task".to_owned(),
                            message: "must be a non-empty string".to_owned(),
                        })?;
                    let agent = subagents.get(agent_name).cloned().ok_or_else(|| {
                        // A model that guessed the name corrects itself from
                        // the list rather than from a bare refusal.
                        vibe_core::tools::ToolError::Unavailable(format!(
                            "subagent `{agent_name}` is unavailable; available agents: {}",
                            if subagent_names.is_empty() {
                                "none".to_owned()
                            } else {
                                subagent_names.join(", ")
                            }
                        ))
                    })?;
                    let effect = manager
                        .delegate(
                            DelegationRequest {
                                parent_session_id,
                                agent,
                                prompt: task.to_owned(),
                                logging: ChildLoggingPolicy::SummaryOnly,
                            },
                            now_millis().map_err(|error| {
                                vibe_core::tools::ToolError::Execution(error.to_string())
                            })?,
                        )
                        .await
                        .map_err(|error| {
                            vibe_core::tools::ToolError::Execution(error.to_string())
                        })?;
                    let typed_result = serde_json::to_value(&effect).map_err(|error| {
                        vibe_core::tools::ToolError::InvalidResult(error.to_string())
                    })?;
                    Ok(ToolExecutionOutput {
                        model_text: effect.result.clone(),
                        typed_result,
                        display: json!({"kind": "subagent", "effect": effect}),
                        chunks: Vec::new(),
                    })
                })
            },
        );
        reservation
            .tools
            .register(task_spec(), handler)
            .map(drop)
            .map_err(|error| DriverError::Tool(error.to_string()))
    }
}

/// Reference `TaskArgs.agent` default.
pub(crate) const DEFAULT_SUBAGENT: &str = "explore";

/// Directive coverage for `task`, whose reference description this port must
/// cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The work is handed to a specialized subagent | "Hand a bounded task to a subagent" |
/// | The subagent runs in its own session and reports back once | "runs in its own session and reports back once" |
/// | The task text is self-contained, because the subagent sees no history | "state it self-contained: the subagent sees none of this conversation" |
/// | The agent name selects which specialization runs | the `agent` description |
///
/// The argument shape comes from the reference `TaskArgs`, which configures
/// `extra="forbid"`: `agent` is a plain string carrying a default rather than
/// an enum of the discovered names, so a schema built here never depends on
/// what the local catalog happens to hold.
pub(crate) fn task_spec() -> ToolSpec {
    ToolSpec {
        name: "task".to_owned(),
        description: "Hand a bounded task to a subagent, which runs in its own session and \
                      reports back once. State the task self-contained: the subagent sees none \
                      of this conversation."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "task",
                Property::string().described("The task for the subagent to perform"),
            )
            .optional(
                "agent",
                Property::string()
                    .described("Which specialized subagent runs the task")
                    .with_default(DEFAULT_SUBAGENT),
            )
            .forbid_extra_properties()
            .build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: vibe_core::tools::ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 40,
    }
}

fn resource_contexts(reservation: &TurnReservation) -> Vec<String> {
    reservation
        .input
        .iter()
        .filter_map(|block| {
            let PublicContentBlock::Resource { resource } = block else {
                return None;
            };
            let embedded = resource.get("resource").unwrap_or(resource);
            let uri = embedded
                .get("uri")
                .or_else(|| resource.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("attached resource");
            let name = embedded
                .get("name")
                .or_else(|| resource.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(uri);
            let text = embedded
                .get("text")
                .or_else(|| resource.get("text"))
                .and_then(Value::as_str);
            Some(text.map_or_else(
                || format!("Attached resource `{name}` is available at {uri}."),
                |text| format!("Attached resource `{name}` ({uri}):\n{text}"),
            ))
        })
        .collect()
}

struct ProviderSubagentRunner {
    provider: Arc<dyn CompletionProvider>,
    system_prompt: String,
    store: SessionStore,
    tools: ToolRegistry,
    input_price_per_million_micros: u64,
    output_price_per_million_micros: u64,
    parent_intent: SessionIntent,
}

impl SubagentRunner for ProviderSubagentRunner {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move {
            let metadata = self
                .store
                .load(&context.child_session_id)
                .map_err(|error| error.to_string())?
                .metadata;
            let parent_executor = SessionToolExecutor::new(self.tools.clone(), &self.parent_intent);
            let settings = context.agent.runtime_settings();
            // An agent declares its two lists in the same form the session does,
            // so they are matched by the same reference rules rather than by
            // exact name.
            let enabled_by_agent = context
                .agent
                .overrides
                .contains_key("enabled_tools")
                .then(|| NameFilter::new(&settings.enabled_tools));
            let disabled_by_agent = NameFilter::new(&settings.disabled_tools);
            let policy_restricted_tools = settings
                .permission_rules
                .iter()
                .map(|rule| rule.tool.clone())
                .collect::<BTreeSet<_>>();
            let allowed = self
                .tools
                .available(&NameFilter::default(), &NameFilter::default())
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|spec| {
                    parent_executor.permits(&spec.name)
                        && spec.name != "task"
                        && !disabled_by_agent.matches(&spec.name)
                        && !policy_restricted_tools.contains(&spec.name)
                        && enabled_by_agent
                            .as_ref()
                            .is_none_or(|enabled| enabled.matches(&spec.name))
                        && (context.agent.safety != "read_only"
                            || matches!(
                                spec.presentation,
                                ToolPresentationKind::Read | ToolPresentationKind::Search
                            ))
                })
                .map(|spec| spec.name)
                .collect();
            let executor = parent_executor.with_allowed_tools(allowed);
            let definitions = executor.definitions().map_err(|error| error.to_string())?;
            let mut messages = vec![ModelMessage::System {
                content: self.system_prompt.clone(),
            }];
            if let Some(prompt_id) = settings.system_prompt_id.as_deref() {
                let prompt = crate::builtin_agents::system_prompt(prompt_id).ok_or_else(|| {
                    format!(
                        "agent `{}` references unsupported system prompt `{prompt_id}`",
                        context.agent.name
                    )
                })?;
                messages.push(ModelMessage::System {
                    content: prompt.to_owned(),
                });
            }
            let input = ProviderInput {
                turn_id: Some(format!("{}-turn", context.child_session_id)),
                model_override: settings.model,
                messages,
                stream: true,
                images: Vec::new(),
                tools: definitions,
                tool_choice: None,
                thinking: settings.thinking.unwrap_or(false),
                reasoning_effort: settings.reasoning_effort,
                headers: BTreeMap::new(),
                limits: RequestLimits {
                    max_tokens: 4096,
                    temperature_millis: None,
                    max_response_bytes: 2 * 1024 * 1024,
                },
                metadata: BTreeMap::from([
                    ("parent_session_id".to_owned(), context.parent_session_id),
                    ("agent".to_owned(), context.agent.name),
                    ("working_directory".to_owned(), context.working_directory),
                ]),
            };
            let outcome = ConversationEngine::new(self.provider.clone())
                .with_tools(executor)
                .with_sink(SessionTranscriptSink::new(self.store.clone(), metadata))
                .with_limits(EngineLimits {
                    input_price_per_million_micros: self.input_price_per_million_micros,
                    output_price_per_million_micros: self.output_price_per_million_micros,
                    ..EngineLimits::default()
                })
                .run_turn(
                    context.child_session_id,
                    input,
                    context.prompt,
                    cancellation,
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(outcome
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    ModelMessage::Assistant { content, .. } if !content.is_empty() => {
                        Some(content.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "Subagent completed without a text response".to_owned()))
        })
    }
}

impl TurnDriver for LiveTurnDriver {
    fn plan_directory(&self) -> Option<PathBuf> {
        self.session_root
            .as_deref()
            .map(|root| root.parent().unwrap_or(root).join("plans"))
    }

    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        self.run_observed(reservation, Arc::new(NoopEventObserver))
    }

    fn run_observed<'a>(
        &'a self,
        reservation: &'a TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> DriverFuture<'a> {
        Box::pin(async move {
            let key = (reservation.session_id.clone(), reservation.turn_id.clone());
            let control = self
                .controls
                .lock()
                .map_err(|_| DriverError::StatePoisoned)?
                .entry(key.clone())
                .or_default()
                .clone();
            let _registration = ControlRegistration {
                controls: &self.controls,
                key,
            };
            self.run_engine(reservation, control.cancellation, control.engine, observer)
                .await
        })
    }

    fn compact<'a>(
        &'a self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> CompactionDriverFuture<'a> {
        Box::pin(async move {
            let root = self
                .session_root
                .as_ref()
                .ok_or(DriverError::UnsupportedControl("session/compact/start"))?;
            let store = SessionStore::new(root);
            let hydrated = store
                .resume(
                    session_id,
                    &self.system_prompt,
                    BTreeMap::<String, Value>::new(),
                )
                .map_err(DriverError::Storage)?;
            let compaction = self
                .compactor
                .compact_with_instructions(
                    &hydrated.metadata.id,
                    &hydrated.messages,
                    extra_instructions,
                )
                .await
                .map_err(|failure| DriverError::Compaction(failure.message))?;
            store
                .handoff_messages(
                    &hydrated.metadata,
                    &compaction.new_session_id,
                    &compaction.messages,
                    now_millis()?,
                    // A manual compaction is still a compaction, so the session
                    // it came from stays its parent.
                    true,
                )
                .map_err(DriverError::Storage)?;
            let compacted = store
                .load(&compaction.new_session_id)
                .map_err(DriverError::Storage)?;
            Ok(SessionCompaction {
                old_session_id: hydrated.metadata.id,
                new_session_id: compaction.new_session_id,
                summary: compaction.summary,
                hydrated: compacted,
            })
        })
    }

    fn interrupt(&self, session_id: &str, turn_id: &str) -> Result<(), DriverError> {
        let mut controls = self
            .controls
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        let control = controls
            .entry((session_id.to_owned(), turn_id.to_owned()))
            .or_default();
        control.cancellation.cancel();
        Ok(())
    }

    fn steer(
        &self,
        session_id: &str,
        turn_id: &str,
        content: &str,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::Steer {
                content: content.to_owned(),
                inject_invoked_skill,
            },
        )
    }

    fn inject_context(
        &self,
        session_id: &str,
        content: &str,
        as_message: bool,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.pending_context
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?
            .entry(session_id.to_owned())
            .or_default()
            .push(PendingContext {
                content: content.to_owned(),
                // The reference injects a skill only into a real user turn, so
                // the flag is honored when the entry is one.
                inject_invoked_skill: as_message && inject_invoked_skill,
            });
        Ok(())
    }

    fn resolve_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        callback_id: &str,
        accepted: bool,
        value: Option<&str>,
    ) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::ResolveCallback {
                callback_id: callback_id.to_owned(),
                accepted,
                value: value.map(str::to_owned),
            },
        )
    }

    fn clear_context(
        &self,
        session_id: &str,
        turn_id: &str,
        continuation: &str,
        plan_file_path: Option<&str>,
    ) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::ClearContext {
                continuation: continuation.to_owned(),
                plan_file_path: plan_file_path.map(str::to_owned),
            },
        )
    }
}

impl LiveTurnDriver {
    /// The warning policy this session speaks through, created on its first
    /// turn and kept afterward.
    ///
    /// `context_warnings` decides whether the policy is registered at all, the
    /// way the reference's `_setup_middleware` does, rather than registering a
    /// silent one: an unregistered policy cannot latch, so turning the key off
    /// mid-session leaves nothing behind.
    fn context_warning(
        &self,
        session_id: &str,
        settings: &CompactionSettings,
    ) -> Result<Option<Arc<ContextWarningMiddleware>>, DriverError> {
        if !settings.context_warnings {
            return Ok(None);
        }
        let mut warnings = self
            .context_warnings
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        Ok(Some(Arc::clone(
            warnings.entry(session_id.to_owned()).or_default(),
        )))
    }

    fn send_control(
        &self,
        session_id: &str,
        turn_id: &str,
        command: TurnControl,
    ) -> Result<(), DriverError> {
        let mut controls = self
            .controls
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        controls
            .entry((session_id.to_owned(), turn_id.to_owned()))
            .or_default()
            .engine
            .send(command)
            .map_err(DriverError::Engine)
    }
}

struct ControlRegistration<'a> {
    controls: &'a Mutex<HashMap<(String, String), LiveTurnControl>>,
    key: (String, String),
}

impl Drop for ControlRegistration<'_> {
    fn drop(&mut self) {
        if let Ok(mut controls) = self.controls.lock() {
            controls.remove(&self.key);
        }
    }
}

#[derive(Debug, Clone)]
pub struct EchoTurnDriver {
    response: String,
    session_root: Option<PathBuf>,
}

impl EchoTurnDriver {
    #[must_use]
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            session_root: None,
        }
    }

    #[must_use]
    pub fn with_session_root(mut self, session_root: impl Into<PathBuf>) -> Self {
        self.session_root = Some(session_root.into());
        self
    }
}

impl TurnDriver for EchoTurnDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async move {
            let mut reducer = vibe_core::events::ProjectionReducer::for_turn(
                &reservation.session_id,
                &reservation.turn_id,
            );
            let mut events = Vec::new();
            for (event_id, event) in [
                vibe_core::events::EngineEvent::UserMessage {
                    content: reservation.prompt.clone(),
                },
                vibe_core::events::EngineEvent::ModelText {
                    text: self.response.clone(),
                },
                vibe_core::events::EngineEvent::Lifecycle {
                    state: LifecycleState::Completed,
                    message: Some("Turn completed".to_owned()),
                },
            ]
            .into_iter()
            .enumerate()
            {
                let envelope = vibe_core::events::EventEnvelope {
                    session_id: reservation.session_id.clone(),
                    turn_id: Some(reservation.turn_id.clone()),
                    emitted_at: now_millis().unwrap_or_default(),
                    event_id: u64::try_from(event_id).unwrap_or(0).saturating_add(1),
                    event,
                };
                reducer
                    .apply(&envelope)
                    .map_err(vibe_core::engine::EngineError::Projection)
                    .map_err(DriverError::Engine)?;
                events.push(envelope);
            }
            let outcome = TurnOutcome {
                session_id: reservation.session_id.clone(),
                events,
                snapshot: reducer.state().clone(),
                messages: vec![
                    ModelMessage::user(reservation.prompt.clone()),
                    ModelMessage::Assistant {
                        content: self.response.clone(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                    },
                ],
                usage: vibe_core::provider::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                context_tokens: 2,
                price_micros: 0,
                steps: 1,
                checkpoints: 1,
                stop_reason: PublicTurnStopReason::Complete,
            };
            if let Some(session_root) = &self.session_root {
                let store = SessionStore::new(session_root);
                let timestamp = now_millis()?;
                let mut metadata = match store.load(&reservation.session_id) {
                    Ok(hydrated) => hydrated.metadata,
                    Err(vibe_core::storage::StorageError::SessionNotFound(_)) => store
                        .create(
                            &reservation.session_id,
                            &reservation.working_directory,
                            None,
                            timestamp,
                        )
                        .map_err(DriverError::Storage)?,
                    Err(error) => return Err(DriverError::Storage(error)),
                };
                for message in &outcome.messages {
                    store
                        .append_message(&mut metadata, message, timestamp)
                        .map_err(DriverError::Storage)?;
                }
            }
            Ok(outcome)
        })
    }

    fn steer(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _content: &str,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        Ok(())
    }

    fn inject_context(
        &self,
        _session_id: &str,
        _content: &str,
        _as_message: bool,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        Ok(())
    }

    fn resolve_callback(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _callback_id: &str,
        _accepted: bool,
        _value: Option<&str>,
    ) -> Result<(), DriverError> {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid app-server response: {0}")]
    InvalidResponse(String),
    #[error("app-server error {0:?}: {1}")]
    Protocol(vibe_protocol::ProtocolErrorCode, String),
    #[error(transparent)]
    Json(serde_json::Error),
    #[error(transparent)]
    Server(#[from] ServerError),
    #[error(transparent)]
    Driver(#[from] DriverError),
}

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("credential environment `{0}` is unavailable")]
    MissingCredentialEnvironment(String),
    #[error("turn `{0}` is stale")]
    StaleTurn(String),
    #[error("driver state lock is poisoned")]
    StatePoisoned,
    #[error("turn driver does not support `{0}`")]
    UnsupportedControl(&'static str),
    #[error("image attachment is invalid: {0}")]
    ImageAttachment(String),
    #[error("event observer failed: {0}")]
    Observation(String),
    #[error("compaction failed: {0}")]
    Compaction(String),
    #[error("tool registry failed: {0}")]
    Tool(String),
    #[error("system clock precedes UNIX epoch")]
    InvalidSystemTime,
    #[error(transparent)]
    Transport(vibe_core::provider::TransportError),
    #[error(transparent)]
    Provider(vibe_core::provider::ProviderError),
    #[error(transparent)]
    Storage(vibe_core::storage::StorageError),
    #[error(transparent)]
    Engine(vibe_core::engine::EngineError),
}

fn request_bytes(id: RequestId, method: &str, params: Value) -> Result<Vec<u8>, ClientError> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(ClientError::Json)
}

fn session_stats(metadata: &vibe_core::storage::SessionMetadata) -> SessionStats {
    let value = |key: &str| {
        metadata
            .statistics
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    SessionStats {
        usage: Usage {
            input_tokens: value("session_prompt_tokens"),
            output_tokens: value("session_completion_tokens"),
        },
        context_tokens: value("context_tokens"),
        steps: u32::try_from(value("steps")).unwrap_or(u32::MAX),
    }
}

pub(crate) fn public_turn_error(reason: &PublicTurnStopReason) -> Option<PublicError> {
    match reason {
        PublicTurnStopReason::Refusal => Some(public_turn_failure(
            TurnErrorCode::Refusal,
            "Provider refused the request",
        )),
        PublicTurnStopReason::ResponseLength => Some(public_turn_failure(
            TurnErrorCode::ResponseTooLong,
            "The model's response exceeded the maximum output token limit.",
        )),
        PublicTurnStopReason::Failed => Some(public_turn_failure(
            TurnErrorCode::BackendError,
            "Turn failed",
        )),
        PublicTurnStopReason::Complete
        | PublicTurnStopReason::MaxSteps
        | PublicTurnStopReason::TokenLimit
        | PublicTurnStopReason::PriceLimit
        | PublicTurnStopReason::Cancelled => None,
    }
}

/// The published form of a turn failure: a message for the reader, a code from
/// the reference vocabulary for the client.
pub(crate) fn public_turn_failure(code: TurnErrorCode, message: &str) -> PublicError {
    PublicError {
        message: message.to_owned(),
        code: serde_json::to_value(code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned)),
        details: Value::Null,
    }
}

/// Classifies a driver failure into the code a client branches on.
///
/// Classification reads the failure's type, never its rendered text: a message
/// is prose that may be reworded, and a code a client acts on must not move
/// with it.
#[must_use]
pub fn turn_error_code(error: &DriverError) -> TurnErrorCode {
    match error {
        DriverError::ImageAttachment(_) => TurnErrorCode::InvalidImageAttachment,
        DriverError::Compaction(_) => TurnErrorCode::CompactionFailed,
        DriverError::Transport(_) | DriverError::MissingCredentialEnvironment(_) => {
            TurnErrorCode::BackendError
        }
        DriverError::Provider(provider) => provider_error_code(provider),
        DriverError::Engine(EngineError::Provider(provider)) => provider_error_code(provider),
        DriverError::Engine(EngineError::Compaction(_)) => TurnErrorCode::CompactionFailed,
        DriverError::StaleTurn(_)
        | DriverError::StatePoisoned
        | DriverError::UnsupportedControl(_)
        | DriverError::Observation(_)
        | DriverError::Tool(_)
        | DriverError::InvalidSystemTime
        | DriverError::Storage(_)
        | DriverError::Engine(_) => TurnErrorCode::InternalError,
    }
}

fn provider_error_code(error: &ProviderError) -> TurnErrorCode {
    match error {
        ProviderError::ContextOverflow => TurnErrorCode::ContextTooLong,
        ProviderError::Refusal(_) => TurnErrorCode::Refusal,
        ProviderError::UnsupportedContentBlock(_) => TurnErrorCode::ImagesNotSupported,
        ProviderError::Transport(TransportError::ResponseTooLarge { .. }) => {
            TurnErrorCode::ResponseTooLong
        }
        // 429 is the rate limit; every other answered status, exhausted budget
        // or broken stream is the backend failing rather than this port.
        ProviderError::HttpStatus { status } | ProviderError::RetryExhausted { status } => {
            if *status == 429 {
                TurnErrorCode::RateLimit
            } else {
                TurnErrorCode::BackendError
            }
        }
        ProviderError::Transport(_)
        | ProviderError::Authentication { .. }
        | ProviderError::ElapsedTimeout
        | ProviderError::MalformedStream(_)
        | ProviderError::MissingUsage => TurnErrorCode::BackendError,
        ProviderError::UnknownStyle(_) | ProviderError::InvalidRequest(_) => {
            TurnErrorCode::InternalError
        }
    }
}

/// The frame answering a request, out of everything the dispatch produced.
///
/// A dispatch may also carry sequenced notifications, which an in-process
/// caller reads through its observer channel rather than here. Picking the
/// answer by envelope kind keeps this client working whether or not the request
/// happened to publish a session event.
fn response_frame(outbound: Vec<Vec<u8>>) -> Result<Vec<u8>, ClientError> {
    let received = outbound.len();
    outbound
        .into_iter()
        .find(|frame| {
            matches!(
                decode_frame(frame),
                Ok(Envelope::Success(_) | Envelope::Error(_))
            )
        })
        .ok_or_else(|| {
            ClientError::InvalidResponse(format!(
                "no response among the {received} frames the request produced"
            ))
        })
}

fn response_result(
    bytes: Vec<u8>,
    expected_id: &RequestId,
) -> Result<BTreeMap<String, Value>, ClientError> {
    match decode_frame(&bytes).map_err(|error| ClientError::InvalidResponse(error.to_string()))? {
        Envelope::Success(SuccessResponse { id, result, .. }) if &id == expected_id => Ok(result),
        Envelope::Error(ErrorResponse {
            id,
            error: ProtocolError { code, message, .. },
            ..
        }) if &id == expected_id => Err(ClientError::Protocol(code, message)),
        _ => Err(ClientError::InvalidResponse(
            "response ID or shape does not match request".to_owned(),
        )),
    }
}

fn decode_public_dispatch(
    outbound: Vec<Vec<u8>>,
    request_id: &RequestId,
    method: &str,
) -> Result<PublicDispatch, ClientError> {
    let mut result = None;
    let mut notifications = Vec::new();
    for bytes in outbound {
        match decode_frame(&bytes)
            .map_err(|error| ClientError::InvalidResponse(error.to_string()))?
        {
            Envelope::Success(SuccessResponse {
                id,
                result: response,
                ..
            }) if &id == request_id => result = Some(response),
            Envelope::Error(ErrorResponse {
                id,
                error: ProtocolError { code, message, .. },
                ..
            }) if &id == request_id => return Err(ClientError::Protocol(code, message)),
            Envelope::Notification(notification) => {
                notifications.push(PublicNotification {
                    method: notification.method,
                    params: notification.params,
                });
            }
            _ => {
                return Err(ClientError::InvalidResponse(format!(
                    "unexpected frame returned by `{method}`"
                )));
            }
        }
    }
    Ok(PublicDispatch {
        result: result.ok_or_else(|| {
            ClientError::InvalidResponse(format!("missing response for `{method}`"))
        })?,
        notifications,
    })
}

fn now_millis() -> Result<u64, DriverError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DriverError::InvalidSystemTime)?
        .as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn default_session_root() -> Option<PathBuf> {
    Some(crate::host::vibe_home().join("sessions"))
}

/// Answers an MCP sampling request with the provider this driver already runs
/// turns on.
///
/// Reference `MCPSamplingHandler` returns a structured error rather than a
/// partial completion when the backend fails, and names the model it answered
/// with, so a server can tell which one produced the text it received.
struct ProviderSamplingHandler {
    provider: Arc<dyn CompletionProvider>,
    model: String,
}

impl SamplingHandler for ProviderSamplingHandler {
    fn complete<'a>(&'a self, request: SamplingRequest) -> McpFuture<'a, SamplingResponse> {
        Box::pin(async move {
            let input = ProviderInput {
                turn_id: None,
                model_override: None,
                messages: request
                    .messages
                    .into_iter()
                    .map(|message| match message.role {
                        SamplingRole::System => ModelMessage::System {
                            content: message.content,
                        },
                        SamplingRole::User => ModelMessage::user(message.content),
                        SamplingRole::Assistant => ModelMessage::Assistant {
                            content: message.content,
                            reasoning: None,
                            reasoning_signature: None,
                            reasoning_state: Vec::new(),
                            tool_calls: Vec::new(),
                        },
                    })
                    .collect(),
                stream: false,
                images: Vec::new(),
                tools: Vec::new(),
                tool_choice: None,
                thinking: false,
                reasoning_effort: None,
                headers: BTreeMap::new(),
                limits: RequestLimits {
                    max_tokens: request.max_tokens.unwrap_or(4096),
                    temperature_millis: request.temperature_millis,
                    max_response_bytes: 2 * 1024 * 1024,
                },
                metadata: BTreeMap::from([("operation".to_owned(), "mcp_sampling".to_owned())]),
            };
            let message = self
                .provider
                .complete(&input)
                .await
                .map_err(|error| McpError::Tool(error.to_string()))?;
            Ok(SamplingResponse {
                text: message.text,
                model: self.model.clone(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use vibe_core::mcp::SamplingMessage;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::release3::{Release3Paths, Release3Service};
    use crate::release4::{
        CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectPage, ProjectRepository,
        Release4Service, TeleportCloud, TeleportStartRequest,
    };
    use crate::server::SessionStatus;
    use vibe_core::compaction::CompactionFailureReason;
    use vibe_core::compaction::manager::PLACEHOLDER_SUMMARY;
    use vibe_core::events::ModelToolCall;
    use vibe_core::provider::{AssistantMessage, ImageInput, Usage};
    use vibe_core::schema::{ObjectSchema, Property};
    use vibe_core::tools::{
        ToolAvailability, ToolExecutionOutput, ToolPresentationKind, ToolSource, ToolSpec,
    };

    use super::*;

    struct RejectingInterruptDriver;

    impl TurnDriver for RejectingInterruptDriver {
        fn run<'a>(&'a self, _reservation: &'a TurnReservation) -> DriverFuture<'a> {
            Box::pin(async { Err(DriverError::Tool("not executed".to_owned())) })
        }

        fn interrupt(&self, _session_id: &str, _turn_id: &str) -> Result<(), DriverError> {
            Err(DriverError::Tool("interrupt rejected".to_owned()))
        }
    }

    #[tokio::test]
    async fn driver_rejection_prevents_canonical_interrupt_commit() {
        let mut service = HeadlessService::new(RejectingInterruptDriver).expect("service starts");
        let session_id = service.start_session(&options()).expect("session starts");
        let reservation = service
            .reserve_prompt(&session_id, &TurnRequest::text("pending"))
            .await
            .expect("turn reserves");

        assert!(
            service
                .interrupt(&session_id, &reservation.turn_id)
                .is_err()
        );
        let session = service
            .client
            .server
            .session(&session_id)
            .expect("session remains readable");
        assert_eq!(
            session.active_turn.as_deref(),
            Some(reservation.turn_id.as_str())
        );

        service
            .fail_reserved(&reservation, "test cleanup", TurnErrorCode::InternalError)
            .expect("reservation settles");
    }

    #[tokio::test]
    async fn canonical_interrupt_rejection_is_reported_as_driver_only() {
        let mut service =
            HeadlessService::new(EchoTurnDriver::new("unused")).expect("service starts");
        let session_id = service.start_session(&options()).expect("session starts");
        let reservation = service
            .reserve_prompt(&session_id, &TurnRequest::text("pending"))
            .await
            .expect("turn reserves");

        assert!(matches!(
            service.interrupt(&session_id, "wrong-turn"),
            Ok(InterruptOutcome::DriverOnly { .. })
        ));
        let session = service
            .client
            .server
            .session(&session_id)
            .expect("session remains readable");
        assert_eq!(
            session.active_turn.as_deref(),
            Some(reservation.turn_id.as_str())
        );

        service
            .fail_reserved(&reservation, "test cleanup", TurnErrorCode::InternalError)
            .expect("reservation settles");
    }

    #[tokio::test]
    async fn complete_interrupt_releases_the_canonical_turn_reservation() {
        let mut service =
            HeadlessService::new(EchoTurnDriver::new("unused")).expect("service starts");
        let session_id = service.start_session(&options()).expect("session starts");
        let reservation = service
            .reserve_prompt(&session_id, &TurnRequest::text("pending"))
            .await
            .expect("turn reserves");

        assert!(matches!(
            service.interrupt(&session_id, &reservation.turn_id),
            Ok(InterruptOutcome::Complete)
        ));
        let session = service
            .client
            .server
            .session(&session_id)
            .expect("session remains readable");
        assert_eq!(session.status, SessionStatus::Cancelled);
        assert!(session.active_turn.is_none());
        let state = service
            .public_call("session/read", json!({"sessionId": session_id}))
            .expect("public state remains readable");
        assert_eq!(
            state["state"]
                .pointer("/latestTurn/status")
                .and_then(Value::as_str),
            Some("interrupted")
        );
    }

    #[test]
    fn plan_file_names_cannot_escape_the_plan_directory() {
        let path = plan_file_path(Path::new("/runtime/plans"), "session/../../outside");
        assert_eq!(path, Path::new("/runtime/plans/session_______outside.md"));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("session_______outside.md")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("plans")
        );
    }

    #[tokio::test]
    async fn plan_review_callback_exposes_the_live_driver_path() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
        let plan_path = PathBuf::from("/runtime/plans/session.md");
        let task = tokio::spawn(run_interactive_plan_review(
            sender,
            "session".to_owned(),
            plan_path.clone(),
        ));
        let request = receiver.recv().await.expect("plan review request");
        assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
        let InteractiveCallbackRequest::Tool {
            detail, response, ..
        } = request
        else {
            return;
        };
        assert_eq!(detail["filePath"], json!(plan_path));
        response
            .send(Ok(json!({
                "type": "user_input",
                "result": {
                    "answers": [],
                    "cancelled": true,
                },
            })))
            .expect("plan review response");
        task.await
            .expect("plan review task")
            .expect("plan review completes");
    }

    /// Accepting a plan with the clearing option raises the clearing on the
    /// running turn, and the tool only answers once the turn holds it.
    #[tokio::test]
    async fn accepting_a_plan_with_clearing_raises_it_before_the_tool_answers() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
        let plan_path = PathBuf::from("/runtime/plans/session.md");
        let task = tokio::spawn(run_interactive_plan_review(
            sender,
            "session".to_owned(),
            plan_path.clone(),
        ));
        let request = receiver.recv().await.expect("plan review request");
        assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
        let InteractiveCallbackRequest::Tool { response, .. } = request else {
            return;
        };
        response
            .send(Ok(json!({
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "Plan is complete. Switch to code mode and start implementing?",
                        "answer": "Yes, clear context and auto approve edits",
                        "isOther": false,
                    }],
                    "cancelled": false,
                },
            })))
            .expect("plan review response");

        let raised = receiver.recv().await.expect("clearing request");
        assert!(
            matches!(raised, InteractiveCallbackRequest::ClearContext { .. }),
            "accepting with clearing raises a context clearing"
        );
        let InteractiveCallbackRequest::ClearContext {
            session_id,
            continuation,
            plan_file_path,
            response,
        } = raised
        else {
            return;
        };
        assert_eq!(session_id, "session");
        assert_eq!(plan_file_path.as_deref(), plan_path.to_str());
        assert!(
            continuation.contains("clear planning context"),
            "the continuation is the instruction the cleared turn restarts from: {continuation}"
        );
        response.send(Ok(())).expect("clearing acknowledgment");

        let output = task
            .await
            .expect("plan review task")
            .expect("plan review completes");
        assert_eq!(output.typed_result["switched"], true);
    }

    /// The other accepting option changes the session settings without touching
    /// the transcript, so no clearing crosses the channel.
    #[tokio::test]
    async fn accepting_a_plan_without_clearing_raises_no_clearing() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
        let task = tokio::spawn(run_interactive_plan_review(
            sender,
            "session".to_owned(),
            PathBuf::from("/runtime/plans/session.md"),
        ));
        let request = receiver.recv().await.expect("plan review request");
        assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
        let InteractiveCallbackRequest::Tool { response, .. } = request else {
            return;
        };
        response
            .send(Ok(json!({
                "type": "user_input",
                "result": {
                    "answers": [{
                        "question": "Plan is complete. Switch to code mode and start implementing?",
                        "answer": "Yes, and auto approve edits",
                        "isOther": false,
                    }],
                    "cancelled": false,
                },
            })))
            .expect("plan review response");
        task.await
            .expect("plan review task")
            .expect("plan review completes");
        assert!(
            receiver.try_recv().is_err(),
            "only the clearing option clears the context"
        );
    }

    /// A turn error is classified from the failure's type, so rewording a
    /// message never moves the code a client branches on.
    #[test]
    fn driver_failures_classify_into_the_reference_error_vocabulary() {
        for (error, expected) in [
            (
                DriverError::Provider(ProviderError::ContextOverflow),
                TurnErrorCode::ContextTooLong,
            ),
            (
                DriverError::Provider(ProviderError::Refusal("no".to_owned())),
                TurnErrorCode::Refusal,
            ),
            (
                DriverError::Provider(ProviderError::HttpStatus { status: 429 }),
                TurnErrorCode::RateLimit,
            ),
            (
                DriverError::Provider(ProviderError::HttpStatus { status: 503 }),
                TurnErrorCode::BackendError,
            ),
            (
                DriverError::Provider(ProviderError::Transport(TransportError::ResponseTooLarge {
                    limit: 8,
                })),
                TurnErrorCode::ResponseTooLong,
            ),
            (
                DriverError::ImageAttachment("not an image".to_owned()),
                TurnErrorCode::InvalidImageAttachment,
            ),
            (
                DriverError::Compaction("no summary".to_owned()),
                TurnErrorCode::CompactionFailed,
            ),
            (
                DriverError::Engine(EngineError::Compaction("no summary".to_owned())),
                TurnErrorCode::CompactionFailed,
            ),
            (DriverError::StatePoisoned, TurnErrorCode::InternalError),
        ] {
            assert_eq!(
                turn_error_code(&error),
                expected,
                "`{error}` classified wrongly"
            );
        }
    }

    /// The two argument conventions the reference publishes side by side.
    ///
    /// `ask_user_question` takes its model from `UserQuestionRequest`, the one
    /// reference argument model configuring `alias_generator=to_camel`, so its
    /// properties are camelCase. Every other reference tool stays snake_case,
    /// and both conventions must coexist in one published surface.
    #[test]
    fn the_interactive_schema_is_camel_case_while_the_file_tools_stay_snake_case() {
        let questions = interactive_question_spec().input_schema;
        let question = &questions["$defs"]["UserQuestion"]["properties"];
        for camel in ["multiSelect", "hideOther"] {
            assert!(question.get(camel).is_some(), "missing `{camel}`");
        }
        assert!(questions["properties"].get("footerNote").is_some());

        // `footerNote` is nullable through `anyOf`, never an array-form type.
        let footer = &questions["properties"]["footerNote"];
        assert_eq!(
            footer["anyOf"],
            json!([{"type": "string"}, {"type": "null"}])
        );
        assert_eq!(footer["default"], Value::Null);
        assert!(footer.get("type").is_none());

        // No `minLength` survives: the reference publishes none.
        assert!(
            !questions.to_string().contains("minLength"),
            "the reference publishes no minLength on this schema"
        );

        // The reference defaults reach the model as published values.
        assert_eq!(question["header"]["default"], "");
        assert_eq!(question["multiSelect"]["default"], false);
        assert_eq!(question["hideOther"]["default"], false);
        assert_eq!(
            questions["$defs"]["QuestionChoice"]["properties"]["description"]["default"],
            ""
        );

        // The same session publishes snake_case argument keys elsewhere.
        let directory = tempfile::tempdir().expect("workspace");
        let workspace =
            Arc::new(vibe_core::workspace::Workspace::open(directory.path()).expect("workspace"));
        let review = Arc::new(vibe_core::workspace::ReviewManager::new(workspace.clone()));
        let tools = ToolRegistry::default();
        vibe_core::workspace::WorkspaceTools::new(workspace, review)
            .register(
                &tools,
                &vibe_core::policy::ToolGuard::new(
                    vibe_core::policy::PermissionStore::default(),
                    Arc::new(DenyEveryApproval),
                ),
            )
            .expect("workspace tools register");
        let edit = tools
            .list()
            .expect("tools list")
            .into_iter()
            .find(|spec| spec.name == "edit")
            .expect("edit is published");
        for snake in ["file_path", "old_string", "new_string", "replace_all"] {
            assert!(
                edit.input_schema["properties"].get(snake).is_some(),
                "missing `{snake}`"
            );
        }
    }

    /// `exit_plan_mode` takes no arguments, and the reference publishes that
    /// as two keys: no `required`, no `additionalProperties`.
    #[test]
    fn the_plan_review_schema_is_the_bare_reference_object() {
        assert_eq!(
            interactive_plan_review_spec().input_schema,
            json!({"type": "object", "properties": {}})
        );
    }

    /// `minItems: 2` on the options array is what makes an under-specified
    /// question fail, and the failure names the question that caused it.
    #[tokio::test]
    async fn a_question_with_a_single_option_fails_naming_its_index() {
        let (sender, _receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
        let tools = ToolRegistry::default();
        InteractiveSessionToolFactory {
            sender,
            plan_directory: None,
        }
        .register("session", &tools)
        .expect("question tool registers");

        let error = tools
            .invoke(
                "ask_user_question",
                ToolInvocation {
                    call_id: "question-1".to_owned(),
                    arguments: json!({
                        "questions": [
                            {"question": "ok?", "options": [{"label": "a"}, {"label": "b"}]},
                            {"question": "which?", "options": [{"label": "only"}]},
                        ]
                    }),
                },
            )
            .await
            .expect_err("a single-option question is under-specified");

        assert!(
            error.to_string().contains("$.questions[1].options"),
            "the failure must name the offending question: {error}"
        );
    }

    struct DenyEveryApproval;

    impl vibe_core::policy::ApprovalAgent for DenyEveryApproval {
        fn request<'a>(
            &'a self,
            _request: vibe_core::policy::ApprovalRequest,
        ) -> vibe_core::policy::ApprovalFuture<'a> {
            Box::pin(async { Ok(vibe_core::policy::ApprovalDecision::Deny) })
        }
    }

    #[test]
    fn plan_review_tool_is_absent_without_a_canonical_plan_directory() {
        let (sender, _receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
        let tools = ToolRegistry::default();
        InteractiveSessionToolFactory {
            sender,
            plan_directory: None,
        }
        .register("session", &tools)
        .expect("question tool registers");

        let names = tools
            .list()
            .expect("tools list")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["ask_user_question"]);
    }

    #[tokio::test]
    async fn explicit_session_initialization_reports_mcp_diagnostics_once() {
        let temporary = tempfile::tempdir().expect("runtime home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".vibe")).expect("project config directory");
        std::fs::write(
            workspace.join(".vibe/config.toml"),
            r#"
[[mcp_servers]]
name = "broken"
transport = "stdio"
command = "/must-not-run"
"#,
        )
        .expect("project MCP config");
        let release3 = Release3Service::new(
            Release3Paths {
                vibe_home: temporary.path().join("home"),
                working_directory: workspace.clone(),
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("release-3 service");
        let mut service = HeadlessService::new_shared_with_server(
            Arc::new(EchoTurnDriver::new("unused")),
            AppServer::with_release3_service(release3),
        )
        .expect("service starts");
        let mut session_options = options();
        session_options.session_id = Some("mcp-failure".to_owned());
        session_options.working_directory = workspace.to_string_lossy().into_owned();
        let session_id = service
            .start_session(&session_options)
            .expect("session starts before deferred MCP initialization");
        let diagnostics = service
            .initialize_pending_mcp(&session_id)
            .await
            .expect("MCP discovery failure is recoverable");
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("broken"));
        assert!(
            service
                .initialize_pending_mcp(&session_id)
                .await
                .expect("initialization is consumed once")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn trust_transition_rebinds_session_scoped_project_config_writes() {
        let temporary = tempfile::tempdir().expect("runtime home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".vibe")).expect("project config directory");
        std::fs::write(workspace.join(".vibe/config.toml"), "").expect("project config fixture");
        let release3 = Release3Service::new(
            Release3Paths {
                vibe_home: temporary.path().join("home"),
                working_directory: workspace.clone(),
                session_root: temporary.path().join("sessions"),
            },
            false,
        )
        .expect("release-3 service");
        let mut service = HeadlessService::new_shared_with_server(
            Arc::new(EchoTurnDriver::new("unused")),
            AppServer::with_release3_service(release3),
        )
        .expect("service starts");
        let mut session_options = options();
        session_options.session_id = Some("trust-config".to_owned());
        session_options.working_directory = workspace.to_string_lossy().into_owned();
        session_options.trusted = false;
        let session_id = service
            .start_session(&session_options)
            .expect("untrusted session starts");

        let untrusted_write = service.public_call(
            "config/batchWrite",
            json!({
                "sessionId": session_id,
                "writes": [{
                    "target": "project",
                    "expectedFingerprint": null,
                    "mutations": [{"path": ["theme"], "value": "dark"}],
                }],
            }),
        );
        assert!(
            untrusted_write.is_err(),
            "untrusted project write is rejected"
        );

        service
            .public_call_async(
                "workspace/trust/decision",
                json!({
                    "sessionId": session_id,
                    "cwd": workspace,
                    "decision": "trust_cwd",
                }),
            )
            .await
            .expect("workspace trust commits");
        // Trust moved the selected target onto the project file, which the
        // published field surface reports as the first writable target.
        let trusted = service
            .public_call("config/fields/read", json!({"sessionId": session_id}))
            .expect("trusted config reads");
        assert_eq!(trusted["targets"][0], json!("project"));
        // The write names no fingerprint: the server takes the one on disk
        // inside the transaction that compares it.
        service
            .public_call(
                "config/batchWrite",
                json!({
                    "sessionId": session_id,
                    "writes": [{
                        "target": "project",
                        "mutations": [{"path": ["theme"], "value": "dark"}],
                    }],
                }),
            )
            .expect("trusted project write commits");
        assert!(
            std::fs::read_to_string(workspace.join(".vibe/config.toml"))
                .expect("project config persisted")
                .contains("theme = \"dark\"")
        );
    }

    #[test]
    fn mcp_initialization_reads_warnings_and_rejects_anything_else() {
        let notification = |method: &str, params: serde_json::Value| {
            serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
                .expect("notification frame")
        };
        let frames = vec![
            notification("runtime/updated", json!({"sessionId": "s", "runtime": {}})),
            notification(
                "warning",
                json!({"warning": {"message": "connection failed"}}),
            ),
            notification(
                "warning",
                json!({"warning": {"message": "connection failed"}}),
            ),
        ];
        assert_eq!(
            decode_mcp_warnings(&frames).expect("typed warnings"),
            vec!["connection failed"],
            "a repeated diagnostic is reported once"
        );

        for malformed in [
            vec![notification("warning", json!({"warning": {"code": "x"}}))],
            vec![notification("warning", json!({"warning": {"message": 7}}))],
            vec![notification("mcp/updated", json!({"mcp": {}}))],
        ] {
            assert!(matches!(
                decode_mcp_warnings(&malformed),
                Err(ClientError::InvalidResponse(_))
            ));
        }
    }

    #[tokio::test]
    async fn prepared_prompt_reserves_already_validated_images() {
        let mut service = HeadlessService::new_shared(Arc::new(EchoTurnDriver::new("unused")))
            .expect("service starts");
        let session_id = service.start_session(&options()).expect("session starts");
        let image = ImageInput {
            media_type: "image/png".to_owned(),
            data: "aW1hZ2U=".to_owned(),
        };
        let turn = TurnRequest {
            prompt: "inspect @image.png".to_owned(),
            input: vec![
                PublicContentBlock::Text {
                    text: "inspect @image.png".to_owned(),
                },
                PublicContentBlock::Image {
                    attachment: json!({
                        "source": {"kind": "file", "path": "/missing/image.png"},
                        "alias": "image.png",
                        "mimeType": "image/png",
                    }),
                },
            ],
            injected: false,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
        };

        for invalid in [
            ImageInput {
                media_type: "image/png".to_owned(),
                data: "not-base64".to_owned(),
            },
            ImageInput {
                media_type: "image/png".to_owned(),
                data: "A".repeat(
                    usize::try_from(vibe_core::images::MAX_IMAGE_BYTES)
                        .expect("image limit fits usize")
                        .saturating_add(2)
                        / 3
                        * 4
                        + 1,
                ),
            },
        ] {
            assert!(PreparedImages::try_new(vec![invalid]).is_err());
        }
        let no_images = PreparedImages::try_new(Vec::new()).expect("empty prepared image set");
        let error = service
            .reserve_prepared_prompt(&session_id, &turn, no_images)
            .await
            .expect_err("mismatched prepared images fail before reservation");
        assert!(error.to_string().contains("provider images"));
        let prepared_images =
            PreparedImages::try_new(vec![image.clone()]).expect("valid prepared image");
        let reservation = service
            .reserve_prepared_prompt(&session_id, &turn, prepared_images.clone())
            .await
            .expect("prepared prompt reserves without rereading its public file source");

        assert_eq!(reservation.prepared_images, Some(prepared_images));
        service
            .fail_reserved(&reservation, "test cleanup", TurnErrorCode::InternalError)
            .expect("reservation cleanup");
    }

    struct ProgrammaticProjects;

    impl ProjectCloud for ProgrammaticProjects {
        fn create(
            &self,
            _name: &str,
            _repo_url: &str,
            _default_branch: &str,
        ) -> Result<Project, CloudError> {
            Err(CloudError::Unavailable(
                "project creation is not used by this fixture".to_owned(),
            ))
        }

        fn list(&self, _cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
            Ok(ProjectPage {
                projects: vec![Project {
                    project_id: "project-public-dispatch".to_owned(),
                    name: "Public dispatch".to_owned(),
                    repositories: vec![ProjectRepository {
                        repo_url: "https://git.example/public-dispatch".to_owned(),
                        default_branch: Some("main".to_owned()),
                    }],
                    is_read_only: false,
                }],
                next_cursor: None,
            })
        }
    }

    struct ProgrammaticTeleport;

    impl TeleportCloud for ProgrammaticTeleport {
        fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
            Ok(format!("https://cloud.example/{}", request.idempotency_key))
        }
    }

    struct ProgrammaticGit;

    impl GitProbe for ProgrammaticGit {
        fn inspect(&self, _working_directory: &std::path::Path) -> Result<GitSnapshot, CloudError> {
            Ok(GitSnapshot {
                repository: "https://git.example/public-dispatch".to_owned(),
                dirty: false,
                unpushed: false,
            })
        }

        fn push(&self, _working_directory: &std::path::Path) -> Result<(), CloudError> {
            Ok(())
        }
    }

    /// A sampling request reaches the provider as an engine turn: the system
    /// prompt leads, the roles map across, and the request's own budget and
    /// temperature travel with it.
    #[tokio::test]
    async fn a_sampling_request_reaches_the_provider_as_a_completion() {
        struct SamplingProbe {
            seen: Arc<Mutex<Option<ProviderInput>>>,
        }

        impl CompletionProvider for SamplingProbe {
            fn complete<'a>(
                &'a self,
                input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async move {
                    *self.seen.lock().map_err(|_| {
                        vibe_core::provider::ProviderError::MalformedStream(
                            "test lock poisoned".to_owned(),
                        )
                    })? = Some(input.clone());
                    Ok(AssistantMessage {
                        text: "sampled answer".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                        },
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                })
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let driver = LiveTurnDriver::from_provider_for_tests(
            Arc::new(SamplingProbe {
                seen: Arc::clone(&seen),
            }),
            "system",
        );
        let handler = driver.sampling_handler("probe-model");
        let answer = handler
            .complete(SamplingRequest {
                messages: vec![
                    SamplingMessage {
                        role: SamplingRole::System,
                        content: "be brief".to_owned(),
                    },
                    SamplingMessage {
                        role: SamplingRole::User,
                        content: "ping".to_owned(),
                    },
                    SamplingMessage {
                        role: SamplingRole::Assistant,
                        content: "pong".to_owned(),
                    },
                ],
                max_tokens: Some(64),
                temperature_millis: Some(250),
            })
            .await
            .expect("the completion answers");
        assert_eq!(answer.text, "sampled answer");
        assert_eq!(answer.model, "probe-model");

        let input = seen
            .lock()
            .expect("probe lock")
            .clone()
            .expect("the provider was asked");
        assert_eq!(
            input.messages,
            vec![
                ModelMessage::System {
                    content: "be brief".to_owned(),
                },
                ModelMessage::user("ping".to_owned()),
                ModelMessage::Assistant {
                    content: "pong".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                },
            ]
        );
        assert_eq!(input.limits.max_tokens, 64);
        assert_eq!(input.limits.temperature_millis, Some(250));
        assert!(!input.stream, "a sampling request is not streamed");
        assert!(
            input.tools.is_empty(),
            "a sampling request carries no tools"
        );
    }

    /// A backend failure is reported as an error rather than as an empty
    /// completion, so no partial answer reaches the server that asked.
    #[tokio::test]
    async fn a_failing_provider_fails_the_sampling_request() {
        struct FailingProvider;

        impl CompletionProvider for FailingProvider {
            fn complete<'a>(
                &'a self,
                _input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async {
                    Err(vibe_core::provider::ProviderError::MalformedStream(
                        "the backend refused".to_owned(),
                    ))
                })
            }
        }

        let driver = LiveTurnDriver::from_provider_for_tests(Arc::new(FailingProvider), "system");
        let failure = driver
            .sampling_handler("probe-model")
            .complete(SamplingRequest {
                messages: Vec::new(),
                max_tokens: None,
                temperature_millis: None,
            })
            .await
            .expect_err("a failing backend fails the request");
        assert!(
            failure.to_string().contains("the backend refused"),
            "{failure}"
        );
    }

    struct RecordingProvider {
        seen: Arc<Mutex<Vec<ModelMessage>>>,
    }

    impl CompletionProvider for RecordingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                *self.seen.lock().map_err(|_| {
                    vibe_core::provider::ProviderError::MalformedStream(
                        "test lock poisoned".to_owned(),
                    )
                })? = input.messages.clone();
                Ok(AssistantMessage {
                    text: "resumed answer".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    },
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    struct ToolSelectingProvider {
        calls: AtomicUsize,
        saw_definition: Arc<AtomicBool>,
    }

    struct SubagentSelectingProvider {
        root_calls: AtomicUsize,
        child_calls: AtomicUsize,
        saw_task_definition: AtomicBool,
        /// What the parent turn actually publishes for `task`, captured so the
        /// reference argument shape is asserted from the live registration
        /// rather than from the spec function in isolation.
        published_task_parameters: std::sync::Mutex<Option<Value>>,
        child_hid_task_definition: AtomicBool,
        child_inherited_restrictions: AtomicBool,
    }

    impl CompletionProvider for SubagentSelectingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                if input.metadata.contains_key("parent_session_id") {
                    let child_call = self.child_calls.fetch_add(1, Ordering::AcqRel);
                    self.child_hid_task_definition.store(
                        !input.tools.iter().any(|tool| tool.name == "task"),
                        Ordering::Release,
                    );
                    self.child_inherited_restrictions.store(
                        input
                            .tools
                            .iter()
                            .map(|tool| tool.name.as_str())
                            .eq(["read"]),
                        Ordering::Release,
                    );
                    if child_call == 0 {
                        return Ok(AssistantMessage {
                            text: String::new(),
                            reasoning: None,
                            reasoning_signature: None,
                            reasoning_state: Vec::new(),
                            tool_calls: vec![
                                ModelToolCall {
                                    id: "child-edit".to_owned(),
                                    name: "edit".to_owned(),
                                    arguments: "{}".to_owned(),
                                },
                                ModelToolCall {
                                    id: "child-shell".to_owned(),
                                    name: "shell".to_owned(),
                                    arguments: "{}".to_owned(),
                                },
                            ],
                            usage: Usage::default(),
                            refusal: None,
                            stop_reason: "tool_calls".to_owned(),
                            correlation_id: None,
                        });
                    }
                    for call_id in ["child-edit", "child-shell"] {
                        if !input.messages.iter().any(|message| {
                            matches!(
                                message,
                                ModelMessage::Tool {
                                    call_id: actual,
                                    is_error: true,
                                    ..
                                } if actual == call_id
                            )
                        }) {
                            return Err(vibe_core::provider::ProviderError::MalformedStream(
                                format!("restricted child tool `{call_id}` was not rejected"),
                            ));
                        }
                    }
                    return Ok(AssistantMessage {
                        text: "child answer".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    });
                }
                let call = self.root_calls.fetch_add(1, Ordering::AcqRel);
                if call == 0 {
                    self.saw_task_definition.store(
                        input.tools.iter().any(|tool| tool.name == "task"),
                        Ordering::Release,
                    );
                    if let Some(task) = input.tools.iter().find(|tool| tool.name == "task")
                        && let Ok(mut published) = self.published_task_parameters.lock()
                    {
                        *published = Some(task.input_schema.clone());
                    }
                    Ok(AssistantMessage {
                        text: String::new(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: vec![ModelToolCall {
                            id: "delegate-1".to_owned(),
                            name: "task".to_owned(),
                            // `agent` is omitted so the reference default has
                            // to reach the handler for the delegation to run.
                            arguments: r#"{"task":"inspect"}"#.to_owned(),
                        }],
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "tool_calls".to_owned(),
                        correlation_id: None,
                    })
                } else if input.messages.iter().any(|message| {
                    matches!(
                        message,
                        ModelMessage::Tool {
                            call_id,
                            content,
                            is_error: false,
                        } if call_id == "delegate-1" && content == "child answer"
                    )
                }) {
                    Ok(AssistantMessage {
                        text: "root done".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                } else {
                    Err(vibe_core::provider::ProviderError::MalformedStream(
                        "subagent result did not return to the parent".to_owned(),
                    ))
                }
            })
        }
    }

    impl CompletionProvider for ToolSelectingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::AcqRel);
                if call == 0 {
                    self.saw_definition.store(
                        input
                            .tools
                            .iter()
                            .any(|tool| tool.name == "mcp_fixture_echo"),
                        Ordering::Release,
                    );
                    Ok(AssistantMessage {
                        text: String::new(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: vec![ModelToolCall {
                            id: "call-1".to_owned(),
                            name: "mcp_fixture_echo".to_owned(),
                            arguments: r#"{"message":"rust"}"#.to_owned(),
                        }],
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "tool_calls".to_owned(),
                        correlation_id: None,
                    })
                } else {
                    let returned = input.messages.iter().any(|message| {
                        matches!(
                            message,
                            ModelMessage::Tool {
                                call_id,
                                content,
                                is_error: false,
                            } if call_id == "call-1" && content == "hello rust"
                        )
                    });
                    if !returned {
                        return Err(vibe_core::provider::ProviderError::MalformedStream(
                            "tool result did not return through the live driver".to_owned(),
                        ));
                    }
                    Ok(AssistantMessage {
                        text: "done".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                }
            })
        }
    }

    fn options() -> SessionOptions {
        SessionOptions {
            working_directory: "/workspace".to_owned(),
            session_id: Some("session-1".to_owned()),
            add_directories: vec!["/shared".to_owned()],
            trusted: true,
            agent: Some("coder".to_owned()),
            tool_filters: vec!["read".to_owned()],
            enabled_tools: vec!["read".to_owned()],
            disabled_tools: vec!["shell".to_owned()],
            mcp_servers: Vec::new(),
            model: None,
            max_turns: Some(4),
            max_tokens: Some(1000),
            max_price_micros: Some(500),
            mode: None,
            thinking: false,
            reasoning_effort: None,
            auto_approve: true,
            resume: None,
            continue_session: false,
        }
    }

    #[tokio::test]
    async fn thin_client_uses_only_serialized_app_server_contracts() {
        let mut service =
            HeadlessService::new(EchoTurnDriver::new("hello back")).expect("service starts");
        let session_id = service.start_session(&options()).expect("session starts");
        let (observer, mut updates) = programmatic_update_channel(&session_id);
        let turn = service
            .prompt_observed(&session_id, "hello", observer)
            .await
            .expect("turn completes");
        assert_eq!(turn.final_assistant, "hello back");
        assert_eq!(turn.history.len(), 2);
        assert_eq!(turn.events.len(), 3);
        assert_eq!(turn.stop_reason, PublicTurnStopReason::Complete);
        let mut update_count = 0;
        while let Ok(update) = updates.try_recv() {
            let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
                continue;
            };
            assert_eq!(entry.metadata().turn_id.as_deref(), Some("turn-1"));
            update_count += 1;
        }
        assert_eq!(update_count, 2);
        service
            .close_session(&session_id)
            .await
            .expect("session closes");
        service.shutdown().expect("connection shuts down");
    }

    #[tokio::test]
    async fn public_calls_preserve_notifications_and_execute_resource_work() {
        let workspace = tempfile::tempdir().expect("workspace");
        let release4 = Release4Service::with_backends(
            Arc::new(ProgrammaticProjects),
            Arc::new(ProgrammaticTeleport),
            Arc::new(ProgrammaticGit),
        )
        .with_loop_store(workspace.path().join("loops.json"))
        .expect("loop store");
        let mut service = HeadlessService::new_shared_with_server(
            Arc::new(EchoTurnDriver::new("unused")),
            AppServer::with_release4_service(release4),
        )
        .expect("service starts");
        let mut session_options = options();
        session_options.session_id = Some("public-dispatch".to_owned());
        session_options.working_directory = workspace.path().to_string_lossy().into_owned();
        session_options.trusted = false;
        let session_id = service
            .start_session(&session_options)
            .expect("session starts");

        let picker = service
            .public_call_async(
                "vibeCode/projects/open",
                json!({
                    "sessionId": session_id,
                    "workingDirectory": workspace.path(),
                    "purpose": "configure",
                }),
            )
            .await
            .expect("project picker opens")
            .result;
        let picker_id = picker["pickerId"].as_str().expect("picker ID");
        service
            .public_call(
                "vibeCode/projects/select",
                json!({
                    "sessionId": session_id,
                    "pickerId": picker_id,
                    "projectId": "project-public-dispatch",
                }),
            )
            .expect("project selects");
        let programmatic = service
            .teleport(
                &session_id,
                &workspace.path().to_string_lossy(),
                "continue",
                false,
            )
            .await
            .expect("programmatic Teleport completes");
        assert!(matches!(
            programmatic.as_slice(),
            [
                ProgrammaticTeleportEvent::SummarizingContext { .. },
                ProgrammaticTeleportEvent::CheckingGit { .. },
                ProgrammaticTeleportEvent::StartingWorkflow { .. },
                ProgrammaticTeleportEvent::Complete { .. },
            ]
        ));
        let teleport = service
            .public_call_async(
                "vibeCode/teleport/start",
                json!({
                    "sessionId": session_id,
                    "pickerId": picker_id,
                    "projectId": "project-public-dispatch",
                    "operationId": "teleport-public-dispatch",
                    "workingDirectory": workspace.path(),
                }),
            )
            .await
            .expect("response and notifications decode together");
        assert_eq!(
            teleport.result["operationId"],
            json!("teleport-public-dispatch")
        );
        assert_eq!(teleport.notifications.len(), 4);
        assert_eq!(
            teleport
                .notifications
                .last()
                .map(|event| event.method.as_str()),
            Some("vibeCode/teleport/event")
        );
        assert_eq!(
            teleport
                .notifications
                .last()
                .map(|event| &event.params["event"]["kind"]),
            Some(&json!("complete"))
        );

        let trusted = service
            .public_call_async(
                "workspace/trust/decision",
                json!({
                    "sessionId": session_id,
                    "cwd": workspace.path(),
                    "decision": "trust_cwd",
                }),
            )
            .await
            .expect("deferred resource response");
        assert!(trusted.result.is_empty());
        assert_eq!(
            trusted
                .notifications
                .first()
                .map(|event| event.method.as_str()),
            Some("runtime/updated")
        );
        let integrations = service
            .public_call_async(
                "mcp/read",
                json!({
                    "sessionId": session_id,
                }),
            )
            .await
            .expect("deferred MCP resource response");
        assert!(integrations.result["mcp"]["sources"].is_array());
    }

    /// Answers "done" with no tool calls and records every request's
    /// transcript, so a test proves what the model was shown.
    struct TranscriptProbeProvider {
        transcripts: std::sync::Mutex<Vec<Vec<ModelMessage>>>,
    }

    impl CompletionProvider for TranscriptProbeProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                if let Ok(mut transcripts) = self.transcripts.lock() {
                    transcripts.push(input.messages.clone());
                }
                Ok(AssistantMessage {
                    text: "done".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    /// The one-skill resolver the driver tests install on the registry, the
    /// way `BuiltinTools::register` installs the real one.
    struct ProbeSkillResolver;

    impl vibe_core::skills::InvokedSkillResolver for ProbeSkillResolver {
        fn resolve(&self, prompt: &str) -> Option<vibe_core::skills::InvokedSkill> {
            let name = prompt
                .trim()
                .strip_prefix('/')?
                .split_whitespace()
                .next()?
                .to_ascii_lowercase();
            (name == "probe").then(|| vibe_core::skills::InvokedSkill {
                name: "probe".to_owned(),
                loaded: ToolExecutionOutput {
                    model_text: format!(
                        "name: probe\ncontent: {}\nDo the probing.\n</skill_content>\nskill_dir: None",
                        vibe_core::skills::skill_content_marker("probe")
                    ),
                    typed_result: json!({"name": "probe"}),
                    display: json!({"kind": "skill", "name": "probe"}),
                    chunks: Vec::new(),
                },
                already_loaded: ToolExecutionOutput {
                    model_text: "name: probe\ncontent: already loaded\nskill_dir: None".to_owned(),
                    typed_result: json!({"name": "probe"}),
                    display: json!({"kind": "skill", "name": "probe"}),
                    chunks: Vec::new(),
                },
            })
        }
    }

    fn probe_reservation(prompt: &str, tools: ToolRegistry) -> TurnReservation {
        TurnReservation {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: prompt.to_owned(),
            input: vec![PublicContentBlock::Text {
                text: prompt.to_owned(),
            }],
            prepared_images: None,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: "/workspace".to_owned(),
            compaction: CompactionSettings::default(),
            intent: SessionIntent::default(),
            tools,
        }
    }

    /// US-172: a turn whose prompt is `/name` shows the model the synthetic
    /// pair right after the user message, resolved through the registry the
    /// session registered its tools into.
    #[tokio::test]
    async fn a_slash_turn_reaches_the_model_as_the_synthetic_pair() {
        let provider = Arc::new(TranscriptProbeProvider {
            transcripts: std::sync::Mutex::new(Vec::new()),
        });
        let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system");
        let tools = ToolRegistry::default();
        tools.set_invoked_skills(Arc::new(ProbeSkillResolver));

        driver
            .run(&probe_reservation("/probe do it", tools))
            .await
            .expect("turn completes");

        let transcripts = provider.transcripts.lock().expect("transcripts");
        let seen = transcripts.first().expect("one request");
        let user = seen
            .iter()
            .position(|message| {
                matches!(message, ModelMessage::User { content, .. } if content == "/probe do it")
            })
            .expect("the prompt stays the user's message");
        assert!(
            matches!(
                &seen[user + 1],
                ModelMessage::Assistant { tool_calls, .. }
                    if tool_calls.len() == 1 && tool_calls[0].name == "skill"
            ),
            "the pair follows the user message: {seen:?}"
        );
        assert!(
            matches!(
                &seen[user + 2],
                ModelMessage::Tool { content, is_error: false, .. }
                    if content.contains(&vibe_core::skills::skill_content_marker("probe"))
            ),
            "the call is answered before the model speaks: {seen:?}"
        );
    }

    /// US-173: a context injection carrying the flag appends the pair after
    /// its message at the next turn, and one without the flag stays a plain
    /// message.
    #[tokio::test]
    async fn a_flagged_context_injection_appends_the_pair_before_the_turn() {
        for (inject, expected_pairs) in [(true, 1_usize), (false, 0_usize)] {
            let provider = Arc::new(TranscriptProbeProvider {
                transcripts: std::sync::Mutex::new(Vec::new()),
            });
            let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system");
            let tools = ToolRegistry::default();
            tools.set_invoked_skills(Arc::new(ProbeSkillResolver));
            driver
                .inject_context("session-1", "/probe", true, inject)
                .expect("injection queues");

            driver
                .run(&probe_reservation("hello", tools))
                .await
                .expect("turn completes");

            let transcripts = provider.transcripts.lock().expect("transcripts");
            let seen = transcripts.first().expect("one request");
            let pairs = seen
                .iter()
                .filter(|message| matches!(message, ModelMessage::Tool { .. }))
                .count();
            assert_eq!(
                pairs, expected_pairs,
                "injectInvokedSkill={inject}: {seen:?}"
            );
            let injected = seen
                .iter()
                .position(|message| {
                    matches!(message, ModelMessage::User { content, .. } if content == "/probe")
                })
                .expect("the injected message is carried either way");
            if inject {
                assert!(
                    matches!(
                        &seen[injected + 1],
                        ModelMessage::Assistant { tool_calls, .. }
                            if tool_calls.len() == 1 && tool_calls[0].name == "skill"
                    ),
                    "the pair follows the injected message: {seen:?}"
                );
            }
        }
    }

    /// Captures the tools one turn publishes and, when `task` is among them,
    /// calls it with an agent that does not exist.
    struct TaskProbeProvider {
        calls: AtomicUsize,
        published: std::sync::Mutex<Vec<String>>,
        delegation_error: std::sync::Mutex<Option<String>>,
    }

    impl CompletionProvider for TaskProbeProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::AcqRel);
                if call == 0 {
                    if let Ok(mut published) = self.published.lock() {
                        *published = input.tools.iter().map(|tool| tool.name.clone()).collect();
                    }
                    if input.tools.iter().any(|tool| tool.name == "task") {
                        return Ok(AssistantMessage {
                            text: String::new(),
                            reasoning: None,
                            reasoning_signature: None,
                            reasoning_state: Vec::new(),
                            tool_calls: vec![ModelToolCall {
                                id: "delegate-1".to_owned(),
                                name: "task".to_owned(),
                                arguments: r#"{"task":"inspect","agent":"ghost"}"#.to_owned(),
                            }],
                            usage: Usage::default(),
                            refusal: None,
                            stop_reason: "tool_calls".to_owned(),
                            correlation_id: None,
                        });
                    }
                }
                if let Ok(mut observed) = self.delegation_error.lock() {
                    *observed = input.messages.iter().find_map(|message| match message {
                        ModelMessage::Tool {
                            call_id,
                            content,
                            is_error: true,
                        } if call_id == "delegate-1" => Some(content.clone()),
                        _ => None,
                    });
                }
                Ok(AssistantMessage {
                    text: "done".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    async fn run_task_probe(session_root: Option<PathBuf>) -> Arc<TaskProbeProvider> {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let provider = Arc::new(TaskProbeProvider {
            calls: AtomicUsize::new(0),
            published: std::sync::Mutex::new(Vec::new()),
            delegation_error: std::sync::Mutex::new(None),
        });
        let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system")
            .with_session_root_for_tests(session_root);
        driver
            .run(&TurnReservation {
                session_id: "probe".to_owned(),
                turn_id: "probe-turn".to_owned(),
                prompt: "delegate".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "delegate".to_owned(),
                }],
                prepared_images: None,
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
                working_directory: temporary.path().to_string_lossy().into_owned(),
                compaction: CompactionSettings::default(),
                intent: SessionIntent {
                    trusted: true,
                    ..SessionIntent::default()
                },
                tools: ToolRegistry::default(),
            })
            .await
            .expect("the turn completes");
        provider
    }

    /// Without a session store there is no subagent runner, and the reference
    /// rule is that an unavailable tool is withheld rather than published and
    /// failed at call time.
    #[tokio::test]
    async fn task_is_withheld_when_no_subagent_runner_backs_the_session() {
        let provider = run_task_probe(None).await;
        assert!(
            !provider
                .published
                .lock()
                .expect("published")
                .contains(&"task".to_owned()),
            "task must not be published without a runner"
        );
    }

    /// An agent name nothing answers to is refused with the names that do
    /// exist, so a model that guessed can correct itself.
    #[tokio::test]
    async fn an_unknown_subagent_is_refused_with_the_available_names() {
        let temporary = tempfile::tempdir().expect("temporary sessions");
        let provider = run_task_probe(Some(temporary.path().to_path_buf())).await;
        assert!(
            provider
                .published
                .lock()
                .expect("published")
                .contains(&"task".to_owned()),
            "task is published once a runner backs the session"
        );
        let refused = provider
            .delegation_error
            .lock()
            .expect("observed")
            .clone()
            .expect("the delegation failed back to the model");
        assert!(refused.contains("ghost"), "{refused}");
        assert!(refused.contains("explore"), "{refused}");
    }

    #[tokio::test]
    async fn live_task_tool_runs_a_durable_child_session_through_the_provider() {
        let temporary = tempfile::tempdir().expect("temporary sessions");
        let provider = Arc::new(SubagentSelectingProvider {
            root_calls: AtomicUsize::new(0),
            child_calls: AtomicUsize::new(0),
            saw_task_definition: AtomicBool::new(false),
            published_task_parameters: std::sync::Mutex::new(None),
            child_hid_task_definition: AtomicBool::new(false),
            child_inherited_restrictions: AtomicBool::new(false),
        });
        let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system")
            .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
        let store = SessionStore::new(temporary.path());
        store
            .create(
                "persisted-root",
                &temporary.path().to_string_lossy(),
                None,
                1,
            )
            .expect("durable parent");
        let tools = ToolRegistry::default();
        for (name, presentation) in [
            ("read", ToolPresentationKind::Read),
            ("edit", ToolPresentationKind::Diff),
            ("shell", ToolPresentationKind::Shell),
        ] {
            tools
                .register(
                    ToolSpec {
                        name: name.to_owned(),
                        description: format!("{name} test tool"),
                        input_schema: json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }),
                        output_schema: None,
                        config: Value::Null,
                        state: Value::Null,
                        availability: ToolAvailability::Available,
                        presentation,
                        source: ToolSource::BuiltIn,
                        selection_priority: 10,
                    },
                    Arc::new(
                        |_invocation: &vibe_core::tools::ToolInvocation,
                         _output: vibe_core::tools::ToolOutputSink|
                         -> vibe_core::tools::OwnedToolHandlerFuture {
                            Box::pin(async { Ok(ToolExecutionOutput::text("unexpected")) })
                        },
                    ),
                )
                .expect("test tool");
        }
        let outcome = driver
            .run(&TurnReservation {
                session_id: "runtime-alias".to_owned(),
                turn_id: "root-turn".to_owned(),
                prompt: "delegate".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "delegate".to_owned(),
                }],
                prepared_images: None,
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
                working_directory: temporary.path().to_string_lossy().into_owned(),
                compaction: CompactionSettings::default(),
                intent: SessionIntent {
                    trusted: true,
                    enabled_tools: vec!["task".to_owned(), "read".to_owned(), "edit".to_owned()],
                    disabled_tools: vec!["shell".to_owned()],
                    resume: Some("persisted-root".to_owned()),
                    ..SessionIntent::default()
                },
                tools,
            })
            .await
            .expect("root and child complete");
        assert_eq!(outcome.stop_reason, PublicTurnStopReason::Complete);
        assert!(provider.saw_task_definition.load(Ordering::Acquire));
        assert_eq!(
            provider
                .published_task_parameters
                .lock()
                .expect("published schema")
                .clone()
                .expect("the parent turn published `task`"),
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The task for the subagent to perform"},
                    "agent": {
                        "type": "string",
                        "description": "Which specialized subagent runs the task",
                        "default": "explore",
                    },
                },
                "required": ["task"],
                "additionalProperties": false,
            })
        );
        assert!(provider.child_hid_task_definition.load(Ordering::Acquire));
        assert!(
            provider
                .child_inherited_restrictions
                .load(Ordering::Acquire)
        );
        let page = store.list(None, 0, 10).expect("sessions list");
        assert_eq!(page.sessions.len(), 2);
        let child = page
            .sessions
            .iter()
            .find(|session| session.parent_session_id.as_deref() == Some("persisted-root"))
            .expect("child session");
        assert_eq!(
            store
                .load(&child.id)
                .expect("child hydrates")
                .messages
                .iter()
                .filter_map(|message| match message {
                    ModelMessage::Assistant { content, .. } if !content.is_empty() => {
                        Some(content.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["child answer"]
        );
        assert_eq!(
            store
                .continue_session(
                    &temporary.path().to_string_lossy(),
                    "system",
                    BTreeMap::new(),
                )
                .expect("root pointer remains authoritative")
                .metadata
                .id,
            "persisted-root"
        );
    }

    #[test]
    fn all_programmatic_intent_crosses_the_json_boundary_unchanged() {
        let mut client = InProcessClient::connect().expect("client connects");
        let options = options();
        let session_id = client.start_session(&options).expect("session starts");
        let view = client.session(&session_id).expect("session reads");
        assert_eq!(view.working_directory, options.working_directory);
        assert_eq!(view.intent.add_directories, options.add_directories);
        assert_eq!(view.intent.agent, options.agent);
        assert_eq!(view.intent.max_turns, options.max_turns);
        assert_eq!(view.intent.max_tokens, options.max_tokens);
        assert!(view.intent.trusted);
        assert!(view.intent.auto_approve);
    }

    #[tokio::test]
    async fn live_driver_hydrates_and_extends_a_durable_resume() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let mut metadata = store
            .create("session-resume", "/workspace", None, 1)
            .expect("session creates");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::System {
                    content: "old system".to_owned(),
                },
                2,
            )
            .expect("old system persists");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::user("prior question".to_owned()),
                3,
            )
            .expect("prior message persists");
        metadata.statistics.insert(
            "session_prompt_tokens".to_owned(),
            serde_json::Value::from(10),
        );
        metadata.statistics.insert(
            "session_completion_tokens".to_owned(),
            serde_json::Value::from(4),
        );
        metadata
            .statistics
            .insert("context_tokens".to_owned(), serde_json::Value::from(8));
        metadata
            .statistics
            .insert("steps".to_owned(), serde_json::Value::from(2));
        store
            .update_metadata(&metadata)
            .expect("baseline stats persist");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let driver = LiveTurnDriver::from_provider_for_tests(
            Arc::new(RecordingProvider {
                seen: Arc::clone(&seen),
            }),
            "current system",
        )
        .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
        let outcome = driver
            .run(&TurnReservation {
                session_id: "session-resume".to_owned(),
                turn_id: "turn-1".to_owned(),
                prompt: "new question".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "new question".to_owned(),
                }],
                prepared_images: None,
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
                working_directory: "/workspace".to_owned(),
                compaction: CompactionSettings::default(),
                intent: SessionIntent {
                    resume: Some("session-resume".to_owned()),
                    ..SessionIntent::default()
                },
                tools: ToolRegistry::default(),
            })
            .await
            .expect("resumed turn completes");
        assert_eq!(outcome.session_id, "session-resume");
        assert_eq!(outcome.usage.input_tokens, 13);
        assert_eq!(outcome.usage.output_tokens, 6);
        assert_eq!(outcome.context_tokens, 5);
        assert_eq!(outcome.steps, 3);
        let seen = seen.lock().expect("seen messages");
        assert!(matches!(
            seen.first(),
            Some(ModelMessage::System { content }) if content == "current system"
        ));
        assert!(seen.iter().any(|message| matches!(
            message,
            ModelMessage::User { content, .. } if content == "prior question"
        )));
        drop(seen);
        let persisted = store.load("session-resume").expect("extended transcript");
        assert!(persisted.messages.iter().any(|message| matches!(
            message,
            ModelMessage::Assistant { content, .. } if content == "resumed answer"
        )));
        assert_eq!(persisted.metadata.statistics["session_prompt_tokens"], 13);
        assert_eq!(
            persisted.metadata.statistics["session_completion_tokens"],
            6
        );
        assert_eq!(persisted.metadata.statistics["context_tokens"], 5);
        assert_eq!(persisted.metadata.statistics["steps"], 3);
    }

    /// US-157: the warning reaches the model itself, once, on the turn that
    /// crosses half the window, and never again while the session lives.
    ///
    /// The proof starts at the driver rather than at the pipeline, because the
    /// latch is what the wiring has to get right: the engine is rebuilt for
    /// every turn, so a policy owned by the engine would warn on each of them.
    #[tokio::test]
    async fn the_context_warning_reaches_the_model_once_per_session() {
        struct TranscriptRecordingProvider {
            turns: Arc<Mutex<Vec<Vec<ModelMessage>>>>,
        }

        impl CompletionProvider for TranscriptRecordingProvider {
            fn complete<'a>(
                &'a self,
                input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async move {
                    self.turns
                        .lock()
                        .map_err(|_| {
                            vibe_core::provider::ProviderError::MalformedStream(
                                "test lock poisoned".to_owned(),
                            )
                        })?
                        .push(input.messages.clone());
                    Ok(AssistantMessage {
                        text: "answered".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        // The context stays above half the window and below it,
                        // so the second turn is a real chance to warn again.
                        usage: Usage {
                            input_tokens: 150,
                            output_tokens: 10,
                        },
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                })
            }
        }

        async fn run_two_turns(context_warnings: bool) -> Vec<Vec<ModelMessage>> {
            let temporary = tempfile::tempdir().expect("temporary session root");
            let store = SessionStore::new(temporary.path());
            let mut metadata = store
                .create("warned", "/workspace", None, 1)
                .expect("session creates");
            metadata
                .statistics
                .insert("context_tokens".to_owned(), serde_json::Value::from(100));
            store
                .update_metadata(&metadata)
                .expect("baseline stats persist");
            let turns = Arc::new(Mutex::new(Vec::new()));
            let driver = LiveTurnDriver::from_provider_for_tests(
                Arc::new(TranscriptRecordingProvider {
                    turns: Arc::clone(&turns),
                }),
                "system",
            )
            .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
            let compaction = CompactionSettings {
                auto_compact_threshold: 200,
                context_warnings,
                ..CompactionSettings::default()
            };
            for turn in ["turn-1", "turn-2"] {
                driver
                    .run(&TurnReservation {
                        session_id: "warned".to_owned(),
                        turn_id: turn.to_owned(),
                        prompt: "question".to_owned(),
                        input: vec![PublicContentBlock::Text {
                            text: "question".to_owned(),
                        }],
                        prepared_images: None,
                        client_user_message_id: None,
                        auto_title: None,
                        user_display_content: None,
                        mention_stats: None,
                        working_directory: "/workspace".to_owned(),
                        compaction: compaction.clone(),
                        intent: SessionIntent {
                            resume: Some("warned".to_owned()),
                            ..SessionIntent::default()
                        },
                        tools: ToolRegistry::default(),
                    })
                    .await
                    .expect("the turn completes");
            }
            let recorded = turns.lock().expect("recorded turns");
            recorded.clone()
        }

        // The second request replays the first one's transcript, warning
        // included, so the falsifier is the count rather than the presence.
        let warnings = |messages: &[ModelMessage]| {
            messages
                .iter()
                .filter(|message| {
                    matches!(message, ModelMessage::User { content, injected }
                        if *injected && content.contains("<vibe_warning>"))
                })
                .count()
        };

        let enabled = run_two_turns(true).await;
        assert_eq!(enabled.len(), 2, "both turns reached the provider");
        assert_eq!(
            warnings(&enabled[0]),
            1,
            "the first turn past half the window carries the warning: {:?}",
            enabled[0]
        );
        assert_eq!(
            warnings(&enabled[1]),
            1,
            "the second turn replays the first warning and adds none: {:?}",
            enabled[1]
        );

        let disabled = run_two_turns(false).await;
        assert!(
            disabled.iter().all(|messages| warnings(messages) == 0),
            "context_warnings off registers no policy, so nothing is injected"
        );
    }

    /// US-148: `compaction_model` reaches the summarization request as the
    /// model it overrides the provider's own with, which is what
    /// `get_compaction_model` selects upstream.
    #[tokio::test]
    async fn the_configured_compaction_model_overrides_the_summarization_request() {
        struct ModelRecordingProvider {
            models: Mutex<Vec<Option<String>>>,
        }

        impl CompletionProvider for ModelRecordingProvider {
            fn complete<'a>(
                &'a self,
                input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async move {
                    self.models
                        .lock()
                        .map_err(|_| {
                            vibe_core::provider::ProviderError::MalformedStream(
                                "test lock poisoned".to_owned(),
                            )
                        })?
                        .push(input.model_override.clone());
                    Ok(AssistantMessage {
                        text: "<summary>a summary</summary>".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                })
            }
        }

        let provider = Arc::new(ModelRecordingProvider {
            models: Mutex::new(Vec::new()),
        });
        let compactor = ProviderSessionCompactor::new(Arc::clone(&provider) as Arc<_>);
        let messages = [ModelMessage::System {
            content: "system".to_owned(),
        }];

        compactor
            .compact("session-1", &messages)
            .await
            .expect("the default compaction summarizes");
        compactor
            .with_plan(CompactionPlan {
                model: Some("devstral-small-latest".to_owned()),
                ..CompactionPlan::default()
            })
            .compact("session-1", &messages)
            .await
            .expect("the configured compaction summarizes");

        assert_eq!(
            provider.models.lock().expect("model log").clone(),
            vec![None, Some("devstral-small-latest".to_owned())],
            "an unset key leaves the provider's model, and a set one overrides it"
        );
    }

    /// US-152, US-153: an answer with no summary element is classified as the
    /// empty-summary failure, the fallback gets its one attempt, and outside
    /// strict mode the conversation still compacts under the placeholder while
    /// the classified reason is reported. Strict mode fails instead.
    #[tokio::test]
    async fn an_empty_summary_is_reported_as_the_classified_failure() {
        struct SilentProvider;

        impl CompletionProvider for SilentProvider {
            fn complete<'a>(
                &'a self,
                _input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async {
                    Ok(AssistantMessage {
                        text: "   ".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                })
            }
        }

        let messages = [ModelMessage::System {
            content: "system".to_owned(),
        }];
        let compactor = ProviderSessionCompactor::new(Arc::new(SilentProvider) as Arc<_>);
        let degraded = compactor
            .compact("session-1", &messages)
            .await
            .expect("outside strict mode the conversation still compacts");
        assert_eq!(
            degraded.failure,
            Some(CompactionFailureReason::EmptySummary),
            "the placeholder still reports what it degraded from"
        );
        assert_eq!(degraded.summary, PLACEHOLDER_SUMMARY);

        let failure = compactor
            .with_plan(CompactionPlan {
                strict: true,
                ..CompactionPlan::default()
            })
            .compact("session-1", &messages)
            .await
            .expect_err("strict mode fails the compaction");
        assert_eq!(failure.reason, Some(CompactionFailureReason::EmptySummary));
    }

    #[tokio::test]
    async fn manual_compaction_uses_provider_summary_and_durable_handoff() {
        /// Answers with the summary element the summarizer reads, which is what
        /// a model that followed the compaction request returns.
        struct SummarizingProvider {
            seen: Arc<Mutex<Vec<ModelMessage>>>,
        }

        impl CompletionProvider for SummarizingProvider {
            fn complete<'a>(
                &'a self,
                input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async move {
                    *self.seen.lock().map_err(|_| {
                        vibe_core::provider::ProviderError::MalformedStream(
                            "test lock poisoned".to_owned(),
                        )
                    })? = input.messages.clone();
                    Ok(AssistantMessage {
                        text: "<summary>resumed answer</summary>".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage {
                            input_tokens: 3,
                            output_tokens: 2,
                        },
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                })
            }
        }

        let temporary = tempfile::tempdir().expect("temporary session root");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let driver = LiveTurnDriver::from_provider_for_tests(
            Arc::new(SummarizingProvider {
                seen: Arc::clone(&seen),
            }),
            "current system",
        )
        .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
        let mut service = HeadlessService::new(driver).expect("service");
        let mut compact_options = options();
        compact_options.working_directory = temporary.path().to_string_lossy().into_owned();
        compact_options.session_id = Some("manual-compact".to_owned());
        compact_options.add_directories.clear();
        compact_options.tool_filters.clear();
        compact_options.enabled_tools.clear();
        compact_options.disabled_tools.clear();
        compact_options.agent = None;
        let session_id = service.start_session(&compact_options).expect("session");
        service
            .prompt(&session_id, "retain this decision")
            .await
            .expect("turn");

        let result = service
            .compact(&session_id, "Keep exact file paths")
            .await
            .expect("compaction");
        assert_eq!(result["summary"], "resumed answer");
        let new_session_id = result["state"]["session"]["id"]
            .as_str()
            .expect("new session id");
        assert_ne!(new_session_id, session_id);
        let compacted = SessionStore::new(temporary.path())
            .load(new_session_id)
            .expect("durable compacted session");
        assert_eq!(
            compacted.metadata.parent_session_id.as_deref(),
            Some(session_id.as_str())
        );
        // US-152, US-156: the manual method's response shape is unchanged, and
        // what it now leaves on disk is the envelope, which carries the
        // operator's own turn instead of discarding it.
        assert!(compacted.messages.iter().any(|message| {
            matches!(
                message,
                ModelMessage::User { content, injected: true }
                    if content.contains("<compaction_summary>")
                        && content.contains("resumed answer")
                        && content.contains("retain this decision")
            )
        }));
        assert_eq!(
            service.session(&session_id).expect("old alias resolves").id,
            new_session_id
        );
        assert!(seen.lock().expect("provider input").iter().any(|message| {
            matches!(
                message,
                ModelMessage::User { content, .. } if content.contains("Keep exact file paths")
            )
        }));
    }

    /// US-158: a compaction mints the reference's identity, a UUID shape whose
    /// trailing segment is the one it replaces, and the sessions it leaves
    /// behind keep resolving under the identifiers they were written with.
    #[tokio::test]
    async fn a_compacted_session_keeps_its_stable_identity_suffix() {
        struct SummarizingProvider;

        impl CompletionProvider for SummarizingProvider {
            fn complete<'a>(
                &'a self,
                _input: &'a ProviderInput,
            ) -> vibe_core::engine::ProviderFuture<'a> {
                Box::pin(async move {
                    Ok(AssistantMessage {
                        text: "<summary>the state so far</summary>".to_owned(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: Vec::new(),
                        usage: Usage {
                            input_tokens: 3,
                            output_tokens: 2,
                        },
                        refusal: None,
                        stop_reason: "stop".to_owned(),
                        correlation_id: None,
                    })
                })
            }
        }

        let temporary = tempfile::tempdir().expect("temporary session root");
        let driver = LiveTurnDriver::from_provider_for_tests(Arc::new(SummarizingProvider), "sys")
            .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
        let mut service = HeadlessService::new(driver).expect("service");
        let original = "11111111-2222-3333-4444-abcdefabcdef";
        let mut compact_options = options();
        compact_options.working_directory = temporary.path().to_string_lossy().into_owned();
        compact_options.session_id = Some(original.to_owned());
        compact_options.add_directories.clear();
        compact_options.tool_filters.clear();
        compact_options.enabled_tools.clear();
        compact_options.disabled_tools.clear();
        compact_options.agent = None;
        let session_id = service.start_session(&compact_options).expect("session");
        assert_eq!(session_id, original);

        let mut minted: Vec<String> = Vec::new();
        for _ in 0..2 {
            let current = minted.last().cloned().unwrap_or_else(|| session_id.clone());
            service.prompt(&current, "a decision").await.expect("turn");
            let result = service.compact(&current, "").await.expect("compaction");
            minted.push(
                result["state"]["session"]["id"]
                    .as_str()
                    .expect("new session id")
                    .to_owned(),
            );
        }

        let store = SessionStore::new(temporary.path());
        for identifier in &minted {
            let segments: Vec<usize> = identifier.split('-').map(str::len).collect();
            assert_eq!(segments, vec![8, 4, 4, 4, 12], "{identifier}");
            assert!(
                identifier.ends_with("-abcdefabcdef"),
                "the stable suffix survives: {identifier}"
            );
        }
        assert_ne!(minted[0], minted[1], "each compaction mints a fresh head");
        assert_eq!(
            store
                .load(&minted[0])
                .expect("first compacted session")
                .metadata
                .parent_session_id
                .as_deref(),
            Some(original),
        );
        assert_eq!(
            store
                .load(&minted[1])
                .expect("second compacted session")
                .metadata
                .parent_session_id
                .as_deref(),
            Some(minted[0].as_str()),
        );
        // Nothing on disk was renamed: every identifier this session ever wore
        // still reads, and the client's original handle still resolves.
        for identifier in std::iter::once(original.to_owned()).chain(minted.iter().cloned()) {
            assert_eq!(
                store.load(&identifier).expect("session loads").metadata.id,
                identifier
            );
        }
        assert_eq!(
            service.session(original).expect("old alias resolves").id,
            minted[1]
        );
    }

    #[tokio::test]
    async fn live_driver_exposes_and_executes_the_session_tool_registry() {
        let tools = ToolRegistry::default();
        tools
            .register(
                ToolSpec {
                    name: "mcp_fixture_echo".to_owned(),
                    description: "Echo through MCP".to_owned(),
                    input_schema: ObjectSchema::new()
                        .required("message", Property::string())
                        .build(),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Mcp,
                    source: ToolSource::Mcp,
                    selection_priority: 50,
                },
                Arc::new(
                    |_invocation: &vibe_core::tools::ToolInvocation,
                     _output: vibe_core::tools::ToolOutputSink|
                     -> vibe_core::tools::OwnedToolHandlerFuture {
                        Box::pin(async {
                            Ok(ToolExecutionOutput {
                                typed_result: json!({"echo": "rust"}),
                                model_text: "hello rust".to_owned(),
                                display: Value::Null,
                                chunks: Vec::new(),
                            })
                        })
                    },
                ),
            )
            .expect("register test MCP tool");
        let saw_definition = Arc::new(AtomicBool::new(false));
        let driver = LiveTurnDriver::from_provider_for_tests(
            Arc::new(ToolSelectingProvider {
                calls: AtomicUsize::new(0),
                saw_definition: Arc::clone(&saw_definition),
            }),
            "system",
        );
        let outcome = driver
            .run(&TurnReservation {
                session_id: "session-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                prompt: "use MCP".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "use MCP".to_owned(),
                }],
                prepared_images: None,
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
                working_directory: "/workspace".to_owned(),
                compaction: CompactionSettings::default(),
                intent: SessionIntent::default(),
                tools,
            })
            .await
            .expect("live turn completes");
        assert_eq!(outcome.stop_reason, PublicTurnStopReason::Complete);
        assert!(saw_definition.load(Ordering::Acquire));
    }

    /// A registry carrying the names a filtering test needs to tell apart.
    fn filtering_registry() -> ToolRegistry {
        let tools = ToolRegistry::default();
        for name in [
            "read_file",
            "serena_find",
            "serena_replace",
            "web_fetch",
            "web_search",
        ] {
            tools
                .register(
                    ToolSpec {
                        name: name.to_owned(),
                        description: "fixture".to_owned(),
                        input_schema: ObjectSchema::new().build(),
                        output_schema: None,
                        config: Value::Null,
                        state: Value::Null,
                        availability: ToolAvailability::Available,
                        presentation: ToolPresentationKind::Generic,
                        source: ToolSource::BuiltIn,
                        selection_priority: 0,
                    },
                    Arc::new(
                        |_invocation: &vibe_core::tools::ToolInvocation,
                         _output: vibe_core::tools::ToolOutputSink|
                         -> vibe_core::tools::OwnedToolHandlerFuture {
                            Box::pin(async { Ok(ToolExecutionOutput::text("fixture")) })
                        },
                    ),
                )
                .expect("fixture tool registers");
        }
        tools
    }

    /// The names a session publishes to the model under one pair of filters,
    /// taken from the definitions the turn actually sends.
    fn published_under(enabled: &[&str], disabled: &[&str]) -> Vec<String> {
        let executor = SessionToolExecutor::new(
            filtering_registry(),
            &SessionIntent {
                enabled_tools: enabled.iter().map(|entry| (*entry).to_owned()).collect(),
                disabled_tools: disabled.iter().map(|entry| (*entry).to_owned()).collect(),
                ..SessionIntent::default()
            },
        );
        executor
            .definitions()
            .expect("definitions")
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    }

    /// Reference `available_tools` matches both filter lists with `name_matches`
    /// rather than by exact name, so a shared configuration file selects the
    /// same surface in both clients.
    #[test]
    fn configured_tool_filters_match_by_glob_regular_expression_and_case() {
        assert_eq!(
            published_under(&[], &["serena_*"]),
            ["read_file", "web_fetch", "web_search"]
        );
        assert_eq!(
            published_under(&[], &["re:web_.*"]),
            ["read_file", "serena_find", "serena_replace"]
        );
        assert_eq!(
            published_under(&[], &["SERENA_FIND"]),
            ["read_file", "serena_replace", "web_fetch", "web_search"]
        );
        // An allowlist narrows the surface, and the denylist is applied last, so
        // a name both lists match is withheld.
        assert_eq!(
            published_under(&["serena_*", "read_file"], &[]),
            ["read_file", "serena_find", "serena_replace"]
        );
        assert_eq!(
            published_under(&["serena_*"], &["serena_find"]),
            ["serena_replace"]
        );
    }

    /// The same rules guard execution, so a name the model remembers from an
    /// earlier turn cannot be called once a pattern covers it.
    #[tokio::test]
    async fn a_pattern_that_hides_a_tool_also_refuses_to_execute_it() {
        let executor = SessionToolExecutor::new(
            filtering_registry(),
            &SessionIntent {
                disabled_tools: vec!["re:SERENA_.*".to_owned()],
                ..SessionIntent::default()
            },
        );
        let error = executor
            .execute("serena_find", "{}")
            .await
            .expect_err("a tool a pattern hides cannot execute");
        assert!(error.contains("disabled for this session"), "{error}");
        assert!(executor.execute("read_file", "{}").await.is_ok());
    }

    /// An entry that does not compile is dropped rather than applied, so one
    /// mistyped expression cannot empty the surface.
    #[test]
    fn an_uncompilable_entry_leaves_the_rest_of_the_list_in_force() {
        assert_eq!(
            published_under(&[], &["re:[", "serena_*"]),
            ["read_file", "web_fetch", "web_search"]
        );
    }

    #[tokio::test]
    async fn session_tool_filters_apply_again_at_execution_time() {
        let executions = Arc::new(AtomicUsize::new(0));
        let handler_executions = Arc::clone(&executions);
        let tools = ToolRegistry::default();
        tools
            .register(
                ToolSpec {
                    name: "mcp_fixture_echo".to_owned(),
                    description: "Echo through MCP".to_owned(),
                    input_schema: ObjectSchema::new()
                        .required("message", Property::string())
                        .build(),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Mcp,
                    source: ToolSource::Mcp,
                    selection_priority: 50,
                },
                Arc::new(
                    move |_invocation: &vibe_core::tools::ToolInvocation,
                          _output: vibe_core::tools::ToolOutputSink|
                          -> vibe_core::tools::OwnedToolHandlerFuture {
                        let executions = Arc::clone(&handler_executions);
                        Box::pin(async move {
                            executions.fetch_add(1, Ordering::AcqRel);
                            Ok(ToolExecutionOutput::text("unexpected execution"))
                        })
                    },
                ),
            )
            .expect("register test MCP tool");
        let executor = SessionToolExecutor::new(
            tools,
            &SessionIntent {
                disabled_tools: vec!["mcp_fixture_echo".to_owned()],
                ..SessionIntent::default()
            },
        );
        let error = executor
            .execute("mcp_fixture_echo", r#"{"message":"rust"}"#)
            .await
            .expect_err("disabled tool cannot execute");
        assert!(error.contains("disabled for this session"));
        assert_eq!(executions.load(Ordering::Acquire), 0);
    }

    /// The clearing a tool raises reaches the driver bound to the turn the
    /// server reserved, which is the identifier the tool cannot know.
    #[tokio::test]
    async fn a_raised_clearing_reaches_the_driver_with_the_reserved_turn() {
        /// One clearing as the driver received it.
        #[derive(Debug, PartialEq, Eq)]
        struct RecordedClearing {
            session_id: String,
            turn_id: String,
            continuation: String,
            plan_file_path: Option<String>,
        }

        #[derive(Default)]
        struct RecordingDriver {
            clearings: Mutex<Vec<RecordedClearing>>,
        }

        impl TurnDriver for RecordingDriver {
            fn run<'a>(&'a self, _reservation: &'a TurnReservation) -> DriverFuture<'a> {
                Box::pin(async { Err(DriverError::UnsupportedControl("turn/start")) })
            }

            fn clear_context(
                &self,
                session_id: &str,
                turn_id: &str,
                continuation: &str,
                plan_file_path: Option<&str>,
            ) -> Result<(), DriverError> {
                self.clearings
                    .lock()
                    .map_err(|_| DriverError::StatePoisoned)?
                    .push(RecordedClearing {
                        session_id: session_id.to_owned(),
                        turn_id: turn_id.to_owned(),
                        continuation: continuation.to_owned(),
                        plan_file_path: plan_file_path.map(str::to_owned),
                    });
                Ok(())
            }
        }

        let (sender, receiver) =
            tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(MAX_INTERACTIVE_CALLBACKS);
        let driver = Arc::new(RecordingDriver::default());
        let server = AppServer::default().using_surface_extension(
            Arc::new(InteractiveApprovalFactory {
                sender: sender.clone(),
            }),
            Arc::new(InteractiveSessionToolFactory {
                sender: sender.clone(),
                plan_directory: None,
            }),
        );
        let mut service = HeadlessService {
            client: InProcessClient::connect_with_server_and_client(
                server,
                ClientInfo {
                    name: "clearing-test".to_owned(),
                    version: "1".to_owned(),
                    title: None,
                    entrypoint: ClientEntrypoint::Cli,
                    terminal_emulator: TerminalEmulator::Unknown,
                },
                ClientCapabilities {
                    callback_kinds: vec![ClientCallbackKind::UserInput],
                    ..ClientCapabilities::default()
                },
            )
            .expect("client connects"),
            driver: Arc::clone(&driver),
            interactive_callbacks: Some(receiver),
            interactive_backlog: VecDeque::new(),
            pending_interactive_callbacks: HashMap::new(),
        };
        let session_id = service.start_session(&options()).expect("session starts");
        let reservation = service
            .reserve_prompt(&session_id, &TurnRequest::text("plan"))
            .await
            .expect("turn reserves");

        let (response, acknowledgment) = tokio::sync::oneshot::channel();
        sender
            .send(InteractiveCallbackRequest::ClearContext {
                session_id: session_id.clone(),
                continuation: "Plan approved.".to_owned(),
                plan_file_path: Some("/plans/session.md".to_owned()),
                response,
            })
            .await
            .expect("clearing queues");
        assert!(
            service
                .drain_callbacks()
                .expect("the clearing drains")
                .is_empty(),
            "a clearing is not a callback entry"
        );
        acknowledgment
            .await
            .expect("the tool is answered")
            .expect("the driver accepted the clearing");
        assert_eq!(
            driver.clearings.lock().expect("clearings").as_slice(),
            [RecordedClearing {
                session_id: session_id.clone(),
                turn_id: reservation.turn_id.clone(),
                continuation: "Plan approved.".to_owned(),
                plan_file_path: Some("/plans/session.md".to_owned()),
            }]
        );
        service
            .fail_reserved(
                &reservation,
                "fixture complete",
                TurnErrorCode::InternalError,
            )
            .expect("fixture turn closes");
    }

    #[tokio::test]
    async fn interactive_user_input_callbacks_serialize_and_resolve_through_the_server() {
        let mut service = HeadlessService::new_interactive_shared_with_server(
            Arc::new(EchoTurnDriver::new("unused")),
            AppServer::default(),
        )
        .expect("interactive service starts");
        let mut session_options = options();
        session_options.auto_approve = false;
        session_options.enabled_tools.clear();
        session_options.disabled_tools.clear();
        session_options.tool_filters.clear();
        let session_id = service
            .start_session(&session_options)
            .expect("session starts");
        let reservation = service
            .reserve_prompt(&session_id, &TurnRequest::text("ask"))
            .await
            .expect("turn reserves");
        let arguments = json!({
            "questions": [{
                "question": "Language?",
                "header": "Runtime",
                "options": [
                    {"label": "Rust", "description": "Native"},
                    {"label": "Python", "description": "Dynamic"}
                ],
                "multiSelect": false,
                "hideOther": false
            }]
        })
        .to_string();
        let first_tools = reservation.tools.clone();
        let first_arguments = arguments.clone();
        let first = tokio::spawn(async move {
            first_tools
                .execute("ask_user_question", &first_arguments)
                .await
        });
        let second_tools = reservation.tools.clone();
        let second =
            tokio::spawn(
                async move { second_tools.execute("ask_user_question", &arguments).await },
            );

        let first_entry = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let entries = service
                    .drain_callbacks()
                    .expect("callback queue remains valid");
                if let Some(entry) = entries.into_iter().next() {
                    break entry;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first callback arrives");
        assert!(
            service
                .drain_callbacks()
                .expect("second request remains queued")
                .is_empty()
        );
        assert!(matches!(first_entry, PublicHistoryEntry::Callback { .. }));
        let PublicHistoryEntry::Callback {
            callback_id: first_callback_id,
            ..
        } = first_entry
        else {
            return;
        };
        service
            .respond_callback(json!({
                "sessionId": session_id,
                "callbackId": first_callback_id,
                "output": {
                    "type": "user_input",
                    "result": {
                        "answers": [{
                            "question": "Language?",
                            "answer": "Rust",
                            "isOther": false
                        }],
                        "cancelled": false
                    }
                }
            }))
            .expect("first response is accepted");

        let second_entry = service
            .drain_callbacks()
            .expect("queued callback opens")
            .pop()
            .expect("second callback is delivered");
        assert!(matches!(second_entry, PublicHistoryEntry::Callback { .. }));
        let PublicHistoryEntry::Callback {
            callback_id: second_callback_id,
            ..
        } = second_entry
        else {
            return;
        };
        service
            .respond_callback(json!({
                "sessionId": session_id,
                "callbackId": second_callback_id,
                "output": {
                    "type": "user_input",
                    "result": {
                        "answers": [{
                            "question": "Language?",
                            "answer": "Python",
                            "isOther": false
                        }],
                        "cancelled": false
                    }
                }
            }))
            .expect("second response is accepted");

        for task in [first, second] {
            let output = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("tool callback unblocks")
                .expect("tool task joins")
                .expect("tool returns output");
            assert_eq!(output.typed_result["cancelled"], false);
        }
        service
            .fail_reserved(
                &reservation,
                "fixture complete",
                TurnErrorCode::InternalError,
            )
            .expect("fixture turn closes");
    }

    #[tokio::test]
    async fn interactive_approval_callback_returns_the_exact_policy_decision() {
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(MAX_INTERACTIVE_CALLBACKS);
        let driver = Arc::new(EchoTurnDriver::new("unused"));
        let server = AppServer::default().using_surface_extension(
            Arc::new(InteractiveApprovalFactory {
                sender: sender.clone(),
            }),
            Arc::new(InteractiveSessionToolFactory {
                sender: sender.clone(),
                plan_directory: driver.plan_directory(),
            }),
        );
        let mut service = HeadlessService {
            client: InProcessClient::connect_with_server_and_client(
                server,
                ClientInfo {
                    name: "approval-test".to_owned(),
                    version: "1".to_owned(),
                    title: None,
                    entrypoint: ClientEntrypoint::Cli,
                    terminal_emulator: TerminalEmulator::Unknown,
                },
                ClientCapabilities {
                    callback_kinds: vec![ClientCallbackKind::Approval],
                    ..ClientCapabilities::default()
                },
            )
            .expect("client connects"),
            driver,
            interactive_callbacks: Some(receiver),
            interactive_backlog: VecDeque::new(),
            pending_interactive_callbacks: HashMap::new(),
        };
        let mut session_options = options();
        session_options.auto_approve = false;
        let session_id = service
            .start_session(&session_options)
            .expect("session starts");
        let reservation = service
            .reserve_prompt(&session_id, &TurnRequest::text("approve"))
            .await
            .expect("turn reserves");
        let approval_agent = InteractiveApprovalAgent {
            session_id: session_id.clone(),
            sender,
        };
        let requested = tokio::spawn(async move {
            approval_agent
                .request(ApprovalRequest {
                    tool: "shell".to_owned(),
                    input: json!({"command": "cargo test"}),
                    requirements: vec![vibe_core::policy::PermissionRequirement::command(
                        "cargo test",
                    )],
                    rationale: "shell command requires approval".to_owned(),
                })
                .await
        });
        let entry = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(entry) = service
                    .drain_callbacks()
                    .expect("callback queue remains valid")
                    .pop()
                {
                    break entry;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approval callback arrives");
        assert!(matches!(entry, PublicHistoryEntry::Callback { .. }));
        let PublicHistoryEntry::Callback {
            callback_id,
            detail,
            ..
        } = entry
        else {
            return;
        };
        let detail = serde_json::to_value(&detail).expect("the callback detail serializes");
        assert_eq!(
            detail["requiredPermissions"],
            json!([{
                "scope": "command_pattern",
                "invocationPattern": "cargo test",
                "sessionPattern": "cargo test *",
                "label": "cargo test *",
            }])
        );
        assert_eq!(detail["effect"]["input"], json!({"command": "cargo test"}));
        assert_eq!(detail["effect"]["kind"], "shell");
        service
            .respond_callback(json!({
                "sessionId": session_id,
                "callbackId": callback_id,
                "output": {
                    "type": "approval",
                    "decision": {"type": "approve_for_session"}
                }
            }))
            .expect("approval response is accepted");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), requested)
                .await
                .expect("approval unblocks")
                .expect("approval task joins")
                .expect("approval succeeds"),
            ApprovalDecision::ApproveForSession
        );
        service
            .fail_reserved(
                &reservation,
                "fixture complete",
                TurnErrorCode::InternalError,
            )
            .expect("fixture turn closes");
    }

    #[tokio::test]
    async fn server_bound_observer_sequences_repeated_turns_without_replaying_history() {
        let mut service = HeadlessService::new_shared_with_server(
            Arc::new(EchoTurnDriver::new("reply")),
            AppServer::default(),
        )
        .expect("service starts");
        let session_id = service.start_session(&options()).expect("session starts");

        let first = service
            .reserve_prompt(&session_id, &TurnRequest::text("first"))
            .await
            .expect("first turn reserves");
        let (first_observer, mut first_updates) = service
            .interactive_update_channel_after(&session_id, &first.turn_id, 0)
            .expect("first observer binds");
        let first_outcome = service
            .driver()
            .run_observed(&first, first_observer)
            .await
            .expect("first turn runs");
        let mut first_event_ids = Vec::new();
        while let Ok(update) = first_updates.try_recv() {
            let ProgrammaticUpdate::HistoryEntry {
                event_id, entry, ..
            } = update
            else {
                continue;
            };
            assert_eq!(
                entry.metadata().turn_id.as_deref(),
                Some(first.turn_id.as_str())
            );
            first_event_ids.push(event_id);
        }
        assert!(!first_event_ids.is_empty());
        service
            .finish_reserved(&first, first_outcome)
            .expect("first turn finishes");
        let first_watermark = service
            .public_call("session/read", json!({"sessionId": session_id}))
            .expect("canonical state reads")["state"]["eventId"]
            .as_u64()
            .expect("canonical watermark");

        let second = service
            .reserve_prompt(&session_id, &TurnRequest::text("second"))
            .await
            .expect("second turn reserves");
        let (second_observer, mut second_updates) = service
            .interactive_update_channel_after(&session_id, &second.turn_id, first_watermark)
            .expect("second observer binds");
        let second_outcome = service
            .driver()
            .run_observed(&second, second_observer)
            .await
            .expect("second turn runs");
        let mut second_event_ids = Vec::new();
        while let Ok(update) = second_updates.try_recv() {
            let ProgrammaticUpdate::HistoryEntry {
                event_id, entry, ..
            } = update
            else {
                continue;
            };
            assert_eq!(
                entry.metadata().turn_id.as_deref(),
                Some(second.turn_id.as_str())
            );
            assert!(event_id > first_watermark);
            second_event_ids.push(event_id);
        }
        assert!(!second_event_ids.is_empty());
        assert!(
            second_event_ids
                .windows(2)
                .all(|window| window[1] == window[0].saturating_add(1))
        );
        service
            .finish_reserved(&second, second_outcome)
            .expect("second turn finishes");
    }
}
