use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::engine::{EngineError, SessionStats, TurnOutcome};
use vibe_core::events::{
    ApplyOutcome, CallbackKind as EngineCallbackKind, EventEnvelope, LifecycleState, ModelMessage,
    ProjectionReducer,
};
use vibe_core::mcp::McpServerConfig;
use vibe_core::middleware::CompactionSettings;
use vibe_core::provider::{ProviderError, TransportError, Usage};
use vibe_core::storage::{HydratedSession, SessionStore};
use vibe_core::tools::ToolRegistry;
use vibe_protocol::{
    CallbackKind as ClientCallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, Envelope,
    ErrorResponse, ProtocolError, RequestId, SuccessResponse, TerminalEmulator, TransportKind,
    decode_frame,
};

pub use crate::images::PreparedImages;
use crate::images::{provider_images, validate_prepared_images};
use crate::server::{
    AppServer, DeferredWork, ServerConnection, ServerError, SessionIntent, SessionView,
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
                    emitted_at: crate::host::now_millis(),
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
                let timestamp = crate::host::now_millis();
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

pub(crate) mod headless;
pub(crate) mod in_process;
pub(crate) mod interactive;
pub(crate) mod live;

pub use headless::HeadlessService;
pub use in_process::{InProcessClient, PendingPublicCall};

use in_process::ProgrammaticEventObserver;

pub use live::{LiveDriverConfig, LiveTurnDriver};

#[cfg(test)]
mod client_tests;
