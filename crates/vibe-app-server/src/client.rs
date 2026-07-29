use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::engine::{
    CancellationToken, CompletionProvider, ConversationEngine, EngineLimits, EventObserver,
    NoopEventObserver, SessionStats, SessionTranscriptSink, TurnControl, TurnControlHandle,
    TurnOutcome,
};
use vibe_core::events::{
    ApplyOutcome, EventEnvelope, LifecycleState, ModelMessage, ProjectionReducer,
    PublicContentBlock, PublicError, PublicHistoryEntry,
};
use vibe_core::provider::{
    HttpTransport, ImageInput, ProviderBackend, ProviderInput, ProviderStyle, RequestLimits, Usage,
};
use vibe_core::storage::SessionStore;
use vibe_protocol::{
    Envelope, ErrorResponse, ProtocolError, RequestId, SuccessResponse, TransportKind, decode_frame,
};

use crate::server::{
    AppServer, DeferredWork, ServerConnection, ServerError, SessionIntent, SessionView,
};

pub use vibe_core::engine::TurnStopReason as PublicTurnStopReason;

pub type DriverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TurnOutcome, DriverError>> + Send + 'a>>;

pub trait TurnDriver: Send + Sync {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a>;

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

    fn steer(&self, _session_id: &str, _turn_id: &str, _content: &str) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedControl("turn/steer"))
    }

    fn inject_context(
        &self,
        _session_id: &str,
        _content: &str,
        _as_message: bool,
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
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_price_micros: Option<u64>,
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
    pub client_user_message_id: Option<String>,
    pub auto_title: Option<String>,
    pub user_display_content: Option<Value>,
    pub mention_stats: Option<Value>,
    pub working_directory: String,
    pub intent: SessionIntent,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgrammaticUpdate {
    HistoryEntry {
        event_id: u64,
        emitted_at: u64,
        entry: PublicHistoryEntry,
    },
}

pub fn programmatic_update_channel(
    session_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::UnboundedReceiver<ProgrammaticUpdate>,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(ProgrammaticEventObserver {
            reducer: Mutex::new(ProjectionReducer::new(session_id)),
            emitted: Mutex::new(BTreeSet::new()),
            sender,
        }),
        receiver,
    )
}

pub fn programmatic_update_channel_for_turn(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::UnboundedReceiver<ProgrammaticUpdate>,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(ProgrammaticEventObserver {
            reducer: Mutex::new(ProjectionReducer::for_turn(session_id, turn_id)),
            emitted: Mutex::new(BTreeSet::new()),
            sender,
        }),
        receiver,
    )
}

struct ProgrammaticEventObserver {
    reducer: Mutex<ProjectionReducer>,
    emitted: Mutex<BTreeSet<String>>,
    sender: tokio::sync::mpsc::UnboundedSender<ProgrammaticUpdate>,
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
        let mut emitted = self
            .emitted
            .lock()
            .map_err(|_| "programmatic emission lock is poisoned".to_owned())?;
        for entry in &reducer.state().history {
            if entry.is_completed() && emitted.insert(entry.metadata().id.clone()) {
                self.sender
                    .send(ProgrammaticUpdate::HistoryEntry {
                        event_id: event.event_id,
                        emitted_at: event.emitted_at,
                        entry: entry.clone(),
                    })
                    .map_err(|_| "programmatic update receiver is closed".to_owned())?;
            }
        }
        Ok(())
    }
}

pub struct InProcessClient {
    server: AppServer,
    connection: ServerConnection,
    next_request: i64,
}

impl InProcessClient {
    pub fn connect() -> Result<Self, ClientError> {
        let server = AppServer::default();
        let mut client = Self {
            connection: server.connect(TransportKind::InProcess),
            server,
            next_request: 1,
        };
        client.initialize()?;
        Ok(client)
    }

    pub fn start_session(&mut self, options: &SessionOptions) -> Result<String, ClientError> {
        let result = self.call(
            "session/start",
            serde_json::to_value(options).map_err(ClientError::Json)?,
        )?;
        result
            .get("state")
            .and_then(|state| state.pointer("/session/id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ClientError::InvalidResponse("missing sessionId".to_owned()))
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
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "turn/start",
            json!({
                "sessionId": session_id,
                "input": [{"type": "text", "text": prompt}],
            }),
        )?;
        let batch = self.connection.dispatch(&request);
        let result = response_result(single_outbound(batch.outbound)?, &request_id)?;
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
                && deferred_prompt == prompt => {}
            _ => {
                return Err(ClientError::InvalidResponse(
                    "turn reservation omitted deferred work".to_owned(),
                ));
            }
        }
        let session = self.server.session(session_id)?;
        Ok(TurnReservation {
            session_id: session_id.to_owned(),
            turn_id,
            prompt: prompt.to_owned(),
            input: vec![PublicContentBlock::Text {
                text: prompt.to_owned(),
            }],
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: session.working_directory,
            intent: session.intent,
        })
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
    ) -> Result<(), ClientError> {
        self.server
            .fail_turn(&reservation.session_id, &reservation.turn_id, message)?;
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
        response_result(single_outbound(batch.outbound)?, &request_id)?;
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
        response_result(single_outbound(batch.outbound)?, &request_id)?;
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

    fn initialize(&mut self) -> Result<(), ClientError> {
        self.call(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "vibe-programmatic",
                    "version": env!("CARGO_PKG_VERSION"),
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                },
                "capabilities": {
                    "callbackKinds": ["approval", "user_input"],
                    "clientTools": [],
                    "disabledNotifications": []
                }
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
        response_result(single_outbound(batch.outbound)?, &request_id)
    }

    fn take_request_id(&mut self) -> RequestId {
        let id = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        RequestId::Integer(id)
    }
}

pub struct HeadlessService<D> {
    client: InProcessClient,
    driver: Arc<D>,
}

impl<D> HeadlessService<D>
where
    D: TurnDriver,
{
    pub fn new(driver: D) -> Result<Self, ClientError> {
        Ok(Self {
            client: InProcessClient::connect()?,
            driver: Arc::new(driver),
        })
    }

    pub fn start_session(&mut self, options: &SessionOptions) -> Result<String, ClientError> {
        self.client.start_session(options)
    }

    pub async fn prompt(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<ProgrammaticTurn, ClientError> {
        let reservation = self.client.reserve_turn(session_id, prompt)?;
        match self.driver.run(&reservation).await {
            Ok(outcome) => self.client.finish_turn(&reservation, outcome),
            Err(error) => {
                self.client.fail_turn(&reservation, &error.to_string())?;
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
        let reservation = self.client.reserve_turn(session_id, prompt)?;
        match self.driver.run_observed(&reservation, observer).await {
            Ok(outcome) => self.client.finish_turn(&reservation, outcome),
            Err(error) => {
                self.client.fail_turn(&reservation, &error.to_string())?;
                Err(ClientError::Driver(error))
            }
        }
    }

    pub fn interrupt(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError> {
        self.client.interrupt(session_id, turn_id)?;
        self.driver.interrupt(session_id, turn_id)?;
        Ok(())
    }

    pub async fn close_session(&mut self, session_id: &str) -> Result<(), ClientError> {
        if let Some((session_id, turn_id)) = self.client.close_session(session_id).await? {
            self.driver.interrupt(&session_id, &turn_id)?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.client.shutdown()
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
}

pub struct LiveTurnDriver {
    provider: Arc<dyn CompletionProvider>,
    system_prompt: String,
    session_root: Option<PathBuf>,
    input_price_per_million_micros: u64,
    output_price_per_million_micros: u64,
    controls: Mutex<HashMap<(String, String), LiveTurnControl>>,
    pending_context: Mutex<HashMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Default)]
struct LiveTurnControl {
    cancellation: CancellationToken,
    engine: TurnControlHandle,
}

impl LiveTurnDriver {
    pub fn from_environment(config: LiveDriverConfig) -> Result<Self, DriverError> {
        let style = ProviderStyle::parse(&config.style).map_err(DriverError::Provider)?;
        let credential = std::env::var(&config.credential_environment).map_err(|_| {
            DriverError::MissingCredentialEnvironment(config.credential_environment.clone())
        })?;
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
        Ok(Self {
            provider: Arc::new(provider),
            system_prompt: config.system_prompt,
            session_root: config.session_root,
            input_price_per_million_micros: config.input_price_per_million_micros,
            output_price_per_million_micros: config.output_price_per_million_micros,
            controls: Mutex::new(HashMap::new()),
            pending_context: Mutex::new(HashMap::new()),
        })
    }

    async fn run_engine(
        &self,
        reservation: &TurnReservation,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
        observer: Arc<dyn EventObserver>,
    ) -> Result<TurnOutcome, DriverError> {
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
            messages: vec![ModelMessage::System {
                content: self.system_prompt.clone(),
            }],
            stream: true,
            images: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            thinking: false,
            reasoning_effort: None,
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
        for block in &reservation.input {
            if let PublicContentBlock::Image { attachment } = block
                && let (Some(media_type), Some(data)) = (
                    attachment
                        .get("mediaType")
                        .or_else(|| attachment.get("mimeType"))
                        .and_then(Value::as_str),
                    attachment.get("data").and_then(Value::as_str),
                )
            {
                input.images.push(ImageInput {
                    media_type: media_type.to_owned(),
                    data: data.to_owned(),
                });
            }
        }
        let mut pending_context = self
            .pending_context
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?
            .remove(&reservation.session_id);
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
            let baseline = session_stats(&metadata);
            if let Some(context) = pending_context.take() {
                input.messages.extend(
                    context
                        .into_iter()
                        .map(|content| ModelMessage::User { content }),
                );
            }
            ConversationEngine::new(Arc::clone(&self.provider))
                .with_sink(SessionTranscriptSink::new(store, metadata))
                .with_limits(limits)
                .with_baseline(baseline)
                .with_observer(observer)
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
            if let Some(context) = pending_context.take() {
                input.messages.extend(
                    context
                        .into_iter()
                        .map(|content| ModelMessage::User { content }),
                );
            }
            ConversationEngine::new(Arc::clone(&self.provider))
                .with_limits(limits)
                .with_observer(observer)
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
}

impl TurnDriver for LiveTurnDriver {
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

    fn steer(&self, session_id: &str, turn_id: &str, content: &str) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::Steer {
                content: content.to_owned(),
            },
        )
    }

    fn inject_context(
        &self,
        session_id: &str,
        content: &str,
        _as_message: bool,
    ) -> Result<(), DriverError> {
        self.pending_context
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?
            .entry(session_id.to_owned())
            .or_default()
            .push(content.to_owned());
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
}

impl LiveTurnDriver {
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
}

impl EchoTurnDriver {
    #[must_use]
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
        }
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
            Ok(TurnOutcome {
                session_id: reservation.session_id.clone(),
                events,
                snapshot: reducer.state().clone(),
                messages: vec![
                    ModelMessage::User {
                        content: reservation.prompt.clone(),
                    },
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
            })
        })
    }

    fn steer(&self, _session_id: &str, _turn_id: &str, _content: &str) -> Result<(), DriverError> {
        Ok(())
    }

    fn inject_context(
        &self,
        _session_id: &str,
        _content: &str,
        _as_message: bool,
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
    #[error("event observer failed: {0}")]
    Observation(String),
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
        PublicTurnStopReason::Refusal => Some(PublicError {
            message: "Provider refused the request".to_owned(),
            code: Some("provider_refusal".to_owned()),
            details: Value::Null,
        }),
        PublicTurnStopReason::ResponseLength => Some(PublicError {
            message: "The model's response exceeded the maximum output token limit.".to_owned(),
            code: Some("response_too_long".to_owned()),
            details: Value::Null,
        }),
        PublicTurnStopReason::Failed => Some(PublicError {
            message: "Turn failed".to_owned(),
            code: Some("turn_failed".to_owned()),
            details: Value::Null,
        }),
        PublicTurnStopReason::Complete
        | PublicTurnStopReason::MaxSteps
        | PublicTurnStopReason::TokenLimit
        | PublicTurnStopReason::PriceLimit
        | PublicTurnStopReason::Cancelled => None,
    }
}

fn single_outbound(mut outbound: Vec<Vec<u8>>) -> Result<Vec<u8>, ClientError> {
    if outbound.len() != 1 {
        return Err(ClientError::InvalidResponse(format!(
            "expected one response, received {}",
            outbound.len()
        )));
    }
    outbound
        .pop()
        .ok_or_else(|| ClientError::InvalidResponse("missing response".to_owned()))
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

fn now_millis() -> Result<u64, DriverError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DriverError::InvalidSystemTime)?
        .as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use vibe_core::provider::{AssistantMessage, Usage};

    use super::*;

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
            max_turns: Some(4),
            max_tokens: Some(1000),
            max_price_micros: Some(500),
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
        while let Ok(ProgrammaticUpdate::HistoryEntry { entry, .. }) = updates.try_recv() {
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
                &ModelMessage::User {
                    content: "prior question".to_owned(),
                },
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
        let driver = LiveTurnDriver {
            provider: Arc::new(RecordingProvider {
                seen: Arc::clone(&seen),
            }),
            system_prompt: "current system".to_owned(),
            session_root: Some(temporary.path().to_path_buf()),
            input_price_per_million_micros: 0,
            output_price_per_million_micros: 0,
            controls: Mutex::new(HashMap::new()),
            pending_context: Mutex::new(HashMap::new()),
        };
        let outcome = driver
            .run(&TurnReservation {
                session_id: "session-resume".to_owned(),
                turn_id: "turn-1".to_owned(),
                prompt: "new question".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "new question".to_owned(),
                }],
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
                working_directory: "/workspace".to_owned(),
                intent: SessionIntent {
                    resume: Some("session-resume".to_owned()),
                    ..SessionIntent::default()
                },
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
            ModelMessage::User { content } if content == "prior question"
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
}
