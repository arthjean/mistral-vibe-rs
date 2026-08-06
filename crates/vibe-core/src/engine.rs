use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Notify, mpsc};

use crate::events::{
    EngineEvent, EventEnvelope, LifecycleState, ModelMessage, ModelToolCall, ProjectionError,
    ProjectionReducer, ProjectionSnapshot, SessionHandoffCause,
};
use crate::provider::{
    AssistantMessage, ProviderBackend, ProviderChunk, ProviderError, ProviderInput, ProviderStream,
    ProviderTransport, RetrySink, TransportError, Usage, aggregate_provider_chunks,
};
use crate::storage::{SessionMetadata, SessionStore};
use crate::text::bounded_utf8;
use crate::tools::{MAX_TOOL_ERROR_BYTES, ToolExecutionOutput};

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AssistantMessage, ProviderError>> + Send + 'a>>;
pub type ProviderStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderStream, ProviderError>> + Send + 'a>>;
pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, String>> + Send + 'a>>;
pub type ToolStreamSink = Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;
pub type CompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionResult, String>> + Send + 'a>>;
pub type PersistenceFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Buffered tool output chunks awaiting projection.
const TOOL_STREAM_CAPACITY: usize = 256;
/// Stands in for a tool result the turn was cancelled before receiving.
const INTERRUPTED_TOOL_RESULT: &str = "Tool execution interrupted";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub usage: Usage,
    pub context_tokens: u64,
    pub steps: u32,
}

pub trait CompletionProvider: Send + Sync {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a>;

    /// Streams one completion, reporting every retry to `retries`.
    ///
    /// A provider that does not retry never calls the sink, which is why the
    /// default forwards to [`CompletionProvider::stream`] instead of requiring
    /// every implementation to know about retries.
    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        _retries: &'a (dyn RetrySink + 'a),
    ) -> ProviderStreamFuture<'a> {
        self.stream(input)
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        Box::pin(async move {
            let message = self.complete(input).await?;
            let mut chunks = Vec::new();
            if let Some(reasoning) = message.reasoning {
                chunks.push(ProviderChunk::Reasoning {
                    text: reasoning,
                    signature: message.reasoning_signature.clone(),
                });
            }
            chunks.extend(
                message
                    .reasoning_state
                    .into_iter()
                    .filter(|state| Some(state) != message.reasoning_signature.as_ref())
                    .map(|signature| ProviderChunk::Reasoning {
                        text: String::new(),
                        signature: Some(signature),
                    }),
            );
            chunks.push(ProviderChunk::Text { text: message.text });
            chunks.extend(
                message
                    .tool_calls
                    .into_iter()
                    .map(|call| ProviderChunk::ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                    }),
            );
            chunks.push(ProviderChunk::Usage {
                input_tokens: message.usage.input_tokens,
                output_tokens: message.usage.output_tokens,
            });
            if let Some(message) = message.refusal {
                chunks.push(ProviderChunk::Refusal { message });
            }
            chunks.push(ProviderChunk::Stop {
                reason: message.stop_reason,
            });
            Ok(ProviderStream {
                correlation_id: message.correlation_id,
                chunks: Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok))),
            })
        })
    }
}

impl<T> CompletionProvider for ProviderBackend<T>
where
    T: ProviderTransport,
{
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(ProviderBackend::complete(self, input))
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        Box::pin(ProviderBackend::stream(self, input))
    }

    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        retries: &'a (dyn RetrySink + 'a),
    ) -> ProviderStreamFuture<'a> {
        Box::pin(ProviderBackend::stream_observed(self, input, retries))
    }
}

impl<P> CompletionProvider for Arc<P>
where
    P: CompletionProvider + ?Sized,
{
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        (**self).complete(input)
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        (**self).stream(input)
    }

    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        retries: &'a (dyn RetrySink + 'a),
    ) -> ProviderStreamFuture<'a> {
        (**self).stream_observed(input, retries)
    }
}

/// Forwards a retry to the turn that is waiting on the request.
struct ChannelRetrySink {
    reasons: mpsc::UnboundedSender<String>,
}

impl RetrySink for ChannelRetrySink {
    fn retrying(&self, reason: &str) {
        // A closed receiver means the turn is gone; the request still finishes.
        let _ = self.reasons.send(reason.to_owned());
    }
}

pub trait ToolExecutor: Send + Sync {
    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a>;

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        _output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        self.execute(name, arguments)
    }
}

pub trait Compactor: Send + Sync {
    fn compact<'a>(
        &'a self,
        current_session_id: &'a str,
        messages: &'a [ModelMessage],
    ) -> CompactionFuture<'a>;

    /// Mints the identifier a cleared transcript continues under.
    ///
    /// Clearing borrows the compactor's naming authority without its summary:
    /// the transcript is dropped rather than condensed, but the session still
    /// rotates onto an identifier no other handoff has claimed.
    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String>;
}

pub trait TranscriptSink: Send + Sync {
    fn persist<'a>(
        &'a self,
        messages: &'a [ModelMessage],
        snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a>;

    fn persist_stats<'a>(&'a self, _stats: &'a SessionStats) -> PersistenceFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

pub trait EventObserver: Send + Sync {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String>;
}

pub struct CompositeEventObserver {
    primary: Arc<dyn EventObserver>,
    secondary: Arc<dyn EventObserver>,
}

impl CompositeEventObserver {
    #[must_use]
    pub fn new(primary: Arc<dyn EventObserver>, secondary: Arc<dyn EventObserver>) -> Self {
        Self { primary, secondary }
    }
}

impl EventObserver for CompositeEventObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        self.primary.observe(event)?;
        self.secondary.observe(event)
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopEventObserver;

impl EventObserver for NoopEventObserver {
    fn observe(&self, _event: &EventEnvelope) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoTools;

impl ToolExecutor for NoTools {
    fn execute<'a>(&'a self, name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
        Box::pin(async move { Err(format!("tool `{name}` is unavailable")) })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RejectCompaction;

impl Compactor for RejectCompaction {
    fn compact<'a>(
        &'a self,
        _current_session_id: &'a str,
        _messages: &'a [ModelMessage],
    ) -> CompactionFuture<'a> {
        Box::pin(async { Err("compaction is unavailable".to_owned()) })
    }

    fn cleared_session_id(&self, _current_session_id: &str) -> Result<String, String> {
        Err("context clearing is unavailable".to_owned())
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopTranscriptSink;

impl TranscriptSink for NoopTranscriptSink {
    fn persist<'a>(
        &'a self,
        _messages: &'a [ModelMessage],
        _snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

pub struct SessionTranscriptSink {
    store: SessionStore,
    metadata: Mutex<SessionMetadata>,
}

impl SessionTranscriptSink {
    #[must_use]
    pub fn new(store: SessionStore, metadata: SessionMetadata) -> Self {
        Self {
            store,
            metadata: Mutex::new(metadata),
        }
    }
}

impl TranscriptSink for SessionTranscriptSink {
    fn persist<'a>(
        &'a self,
        messages: &'a [ModelMessage],
        snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a> {
        Box::pin(async move {
            let persisted_at = current_time_millis();
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| "session metadata lock poisoned".to_owned())?;
            if snapshot.session_id != metadata.id {
                let handoff = self
                    .store
                    .handoff_messages(&metadata, &snapshot.session_id, messages, persisted_at)
                    .map_err(|error| error.to_string())?;
                *metadata = handoff;
                return Ok(());
            }
            // The metadata already records how much of the log is persisted, so
            // a checkpoint never has to re-read the transcript it just wrote.
            let non_system: Vec<&ModelMessage> = messages
                .iter()
                .filter(|message| !matches!(message, ModelMessage::System { .. }))
                .collect();
            if !SessionStore::extends_persisted_log(&metadata, &non_system)
                .map_err(|error| error.to_string())?
            {
                return self
                    .store
                    .replace_messages(&mut metadata, messages, persisted_at)
                    .map_err(|error| error.to_string());
            }
            let persisted = usize::try_from(metadata.message_count).unwrap_or(usize::MAX);
            let pending: Vec<ModelMessage> = messages
                .iter()
                .find(|message| matches!(message, ModelMessage::System { .. }))
                .into_iter()
                .chain(non_system.into_iter().skip(persisted))
                .cloned()
                .collect();
            self.store
                .append_messages(&mut metadata, &pending, persisted_at)
                .map_err(|error| error.to_string())
        })
    }

    fn persist_stats<'a>(&'a self, stats: &'a SessionStats) -> PersistenceFuture<'a> {
        Box::pin(async move {
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| "session metadata lock poisoned".to_owned())?;
            metadata.statistics.insert(
                "session_prompt_tokens".to_owned(),
                serde_json::Value::from(stats.usage.input_tokens),
            );
            metadata.statistics.insert(
                "session_completion_tokens".to_owned(),
                serde_json::Value::from(stats.usage.output_tokens),
            );
            metadata.statistics.insert(
                "context_tokens".to_owned(),
                serde_json::Value::from(stats.context_tokens),
            );
            metadata
                .statistics
                .insert("steps".to_owned(), serde_json::Value::from(stats.steps));
            self.store
                .update_metadata(&metadata)
                .map_err(|error| error.to_string())
        })
    }
}

#[derive(Debug, Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self {
            inner: Arc::new(CancellationState::default()),
        }
    }
}

impl CancellationToken {
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.inner.notify.notified().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnControl {
    Steer {
        content: String,
    },
    InjectContext {
        content: String,
        as_message: bool,
    },
    ResolveCallback {
        callback_id: String,
        accepted: bool,
        value: Option<String>,
    },
    /// Drops the transcript and rotates the session at the next cycle boundary,
    /// leaving the turn to continue from `continuation` alone.
    ClearContext {
        continuation: String,
        plan_file_path: Option<String>,
    },
}

/// What draining the control queue leaves the turn to do.
enum ControlOutcome {
    /// The turn continues. `true` when the transcript was cleared and the
    /// session rotated, which the caller checkpoints before the next request.
    Continue(bool),
    Stop(TurnStopReason),
}

#[derive(Debug, Clone, Default)]
pub struct TurnControlHandle {
    queue: Arc<Mutex<VecDeque<TurnControl>>>,
}

impl TurnControlHandle {
    pub fn send(&self, control: TurnControl) -> Result<(), EngineError> {
        self.queue
            .lock()
            .map_err(|_| EngineError::ControlStatePoisoned)?
            .push_back(control);
        Ok(())
    }

    fn drain(&self) -> Result<Vec<TurnControl>, EngineError> {
        Ok(self
            .queue
            .lock()
            .map_err(|_| EngineError::ControlStatePoisoned)?
            .drain(..)
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineLimits {
    pub max_steps: u32,
    pub max_total_tokens: u64,
    pub max_price_micros: u64,
    pub input_price_per_million_micros: u64,
    pub output_price_per_million_micros: u64,
    pub max_response_bytes: usize,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_total_tokens: 200_000,
            max_price_micros: u64::MAX,
            input_price_per_million_micros: 0,
            output_price_per_million_micros: 0,
            max_response_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub new_session_id: String,
    pub summary: String,
    pub messages: Vec<ModelMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStopReason {
    Complete,
    MaxSteps,
    TokenLimit,
    PriceLimit,
    Refusal,
    ResponseLength,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub session_id: String,
    pub events: Vec<EventEnvelope>,
    pub snapshot: ProjectionSnapshot,
    pub messages: Vec<ModelMessage>,
    pub usage: Usage,
    pub context_tokens: u64,
    pub price_micros: u64,
    pub steps: u32,
    pub checkpoints: u32,
    pub stop_reason: TurnStopReason,
}

pub struct ConversationEngine<P, T = NoTools, C = RejectCompaction, S = NoopTranscriptSink> {
    provider: P,
    tools: T,
    compactor: C,
    sink: S,
    limits: EngineLimits,
    baseline: SessionStats,
    observer: Arc<dyn EventObserver>,
}

impl<P> ConversationEngine<P> {
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            tools: NoTools,
            compactor: RejectCompaction,
            sink: NoopTranscriptSink,
            limits: EngineLimits::default(),
            baseline: SessionStats::default(),
            observer: Arc::new(NoopEventObserver),
        }
    }
}

impl<P, T, C, S> ConversationEngine<P, T, C, S> {
    #[must_use]
    pub fn with_tools<T2>(self, tools: T2) -> ConversationEngine<P, T2, C, S> {
        ConversationEngine {
            provider: self.provider,
            tools,
            compactor: self.compactor,
            sink: self.sink,
            limits: self.limits,
            baseline: self.baseline,
            observer: self.observer,
        }
    }

    #[must_use]
    pub fn with_compactor<C2>(self, compactor: C2) -> ConversationEngine<P, T, C2, S> {
        ConversationEngine {
            provider: self.provider,
            tools: self.tools,
            compactor,
            sink: self.sink,
            limits: self.limits,
            baseline: self.baseline,
            observer: self.observer,
        }
    }

    #[must_use]
    pub fn with_sink<S2>(self, sink: S2) -> ConversationEngine<P, T, C, S2> {
        ConversationEngine {
            provider: self.provider,
            tools: self.tools,
            compactor: self.compactor,
            sink,
            limits: self.limits,
            baseline: self.baseline,
            observer: self.observer,
        }
    }

    #[must_use]
    pub fn with_limits(mut self, limits: EngineLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_baseline(mut self, baseline: SessionStats) -> Self {
        self.baseline = baseline;
        self
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observer = observer;
        self
    }
}

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    pub async fn run_turn(
        &self,
        session_id: impl Into<String>,
        input: ProviderInput,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, EngineError> {
        self.run_turn_controlled(
            session_id,
            input,
            prompt,
            cancellation,
            TurnControlHandle::default(),
        )
        .await
    }

    pub async fn run_turn_controlled(
        &self,
        session_id: impl Into<String>,
        mut input: ProviderInput,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
    ) -> Result<TurnOutcome, EngineError> {
        let prompt = prompt.into();
        let mut recorder =
            TurnRecorder::new(self.observer.as_ref(), session_id, input.turn_id.as_deref());
        let mut messages = input.messages.clone();
        let mut ledger = TurnLedger::new(&self.baseline, &self.limits);
        let mut checkpoints = 0_u32;

        recorder.emit(EngineEvent::UserMessage {
            content: prompt.clone(),
        })?;
        messages.push(ModelMessage::User { content: prompt });
        recorder.emit(EngineEvent::Title {
            title: title_from_messages(&messages),
        })?;
        persist(&self.sink, &messages, recorder.state()).await?;
        checkpoints = checkpoints.saturating_add(1);

        let stop_reason = loop {
            if let Some(reason) = self.exhausted_budget(&ledger, &cancellation) {
                break reason;
            }
            match self.apply_controls(&mut recorder, &mut messages, &controls)? {
                ControlOutcome::Stop(reason) => break reason,
                // A rotated session is only durable once the transcript lands
                // under its new identifier, so it checkpoints before the next
                // request rather than at the end of the cycle.
                ControlOutcome::Continue(true) => {
                    persist(&self.sink, &messages, recorder.state()).await?;
                    checkpoints = checkpoints.saturating_add(1);
                }
                ControlOutcome::Continue(false) => {}
            }
            input.messages.clone_from(&messages);
            let completion = match self
                .stream_completion(&mut recorder, &input, &cancellation)
                .await?
            {
                StreamOutcome::Cancelled => break TurnStopReason::Cancelled,
                StreamOutcome::Completed(Ok(completion)) => *completion,
                StreamOutcome::Completed(Err(ProviderError::ContextOverflow)) => {
                    match self
                        .compact(&mut recorder, &mut messages, &cancellation)
                        .await?
                    {
                        Some(()) => {
                            persist(&self.sink, &messages, recorder.state()).await?;
                            checkpoints = checkpoints.saturating_add(1);
                            continue;
                        }
                        None => break TurnStopReason::Cancelled,
                    }
                }
                StreamOutcome::Completed(Err(ProviderError::Refusal(_))) => {
                    break TurnStopReason::Refusal;
                }
                StreamOutcome::Completed(Err(ProviderError::Transport(
                    TransportError::ResponseTooLarge { .. },
                ))) => break TurnStopReason::ResponseLength,
                StreamOutcome::Completed(Err(error)) => {
                    recorder.emit(EngineEvent::Lifecycle {
                        state: LifecycleState::Failed,
                        message: Some(error.to_string()),
                    })?;
                    persist(&self.sink, &messages, recorder.state()).await?;
                    return Err(EngineError::Provider(error));
                }
            };

            ledger.record_completion(&completion.usage, &self.limits);
            recorder.emit(EngineEvent::Stats {
                context_tokens: ledger.context_tokens,
                input_tokens: ledger.usage.input_tokens,
                output_tokens: ledger.usage.output_tokens,
            })?;
            let assistant_message = ModelMessage::Assistant {
                content: completion.text.clone(),
                reasoning: completion.reasoning.clone(),
                reasoning_signature: completion.reasoning_signature.clone(),
                reasoning_state: completion.reasoning_state.clone(),
                tool_calls: completion.tool_calls.clone(),
            };
            // A limit reached mid-cycle keeps a tool-free reply, but never a
            // reply whose tool calls would be left unanswered.
            if let Some(reason) = self.exhausted_budget(&ledger, &cancellation) {
                if completion.tool_calls.is_empty() {
                    messages.push(assistant_message);
                }
                break reason;
            }
            if completion.text.len() > self.limits.max_response_bytes {
                break TurnStopReason::ResponseLength;
            }
            messages.push(assistant_message);
            if completion.tool_calls.is_empty() {
                break TurnStopReason::Complete;
            }

            let results = self
                .execute_tool_calls(&mut recorder, &completion.tool_calls, &cancellation)
                .await?;
            for (call, (content, is_error)) in completion.tool_calls.into_iter().zip(results) {
                messages.push(ModelMessage::Tool {
                    call_id: call.id,
                    content,
                    is_error,
                });
            }
            persist(&self.sink, &messages, recorder.state()).await?;
            checkpoints = checkpoints.saturating_add(1);
            if cancellation.is_cancelled() {
                break TurnStopReason::Cancelled;
            }
        };

        recorder.emit(EngineEvent::Lifecycle {
            state: lifecycle_for(&stop_reason),
            message: Some(stop_message(&stop_reason).to_owned()),
        })?;
        persist(&self.sink, &messages, recorder.state()).await?;
        persist_stats(
            &self.sink,
            &SessionStats {
                usage: ledger.usage.clone(),
                context_tokens: ledger.context_tokens,
                steps: ledger.steps,
            },
        )
        .await?;
        let (events, snapshot) = recorder.finish();
        Ok(TurnOutcome {
            session_id: snapshot.session_id.clone(),
            events,
            snapshot,
            messages,
            usage: ledger.usage,
            context_tokens: ledger.context_tokens,
            price_micros: ledger.price_micros,
            steps: ledger.steps,
            checkpoints,
            stop_reason,
        })
    }

    /// Reports the limit that ends the turn, if any is already reached.
    fn exhausted_budget(
        &self,
        ledger: &TurnLedger,
        cancellation: &CancellationToken,
    ) -> Option<TurnStopReason> {
        if cancellation.is_cancelled() {
            return Some(TurnStopReason::Cancelled);
        }
        if ledger.steps >= self.limits.max_steps {
            return Some(TurnStopReason::MaxSteps);
        }
        if ledger.total_tokens() > self.limits.max_total_tokens {
            return Some(TurnStopReason::TokenLimit);
        }
        if ledger.price_micros > self.limits.max_price_micros {
            return Some(TurnStopReason::PriceLimit);
        }
        None
    }

    /// Drains queued steering, context injection, callback resolutions and
    /// context clearings.
    fn apply_controls(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        controls: &TurnControlHandle,
    ) -> Result<ControlOutcome, EngineError> {
        let mut cleared = false;
        for control in controls.drain()? {
            match control {
                TurnControl::Steer { content } => {
                    recorder.emit(EngineEvent::UserSteer {
                        content: content.clone(),
                    })?;
                    messages.push(ModelMessage::User { content });
                }
                TurnControl::InjectContext {
                    content,
                    as_message,
                } => {
                    recorder.emit(EngineEvent::ContextInjected {
                        content: content.clone(),
                        as_message,
                    })?;
                    messages.push(ModelMessage::User { content });
                }
                TurnControl::ResolveCallback {
                    callback_id,
                    accepted,
                    value,
                } => {
                    // Only a callback the projection knows about can be resolved
                    // against it; others are late replies to a dropped turn.
                    if recorder.has_callback(&callback_id) {
                        recorder.emit(EngineEvent::CallbackResolved {
                            callback_id,
                            accepted,
                            value,
                        })?;
                    }
                    if !accepted {
                        return Ok(ControlOutcome::Stop(TurnStopReason::Cancelled));
                    }
                }
                TurnControl::ClearContext {
                    continuation,
                    plan_file_path,
                } => {
                    let from_session_id = recorder.state().session_id.clone();
                    let to_session_id = self
                        .compactor
                        .cleared_session_id(&from_session_id)
                        .map_err(EngineError::Compaction)?;
                    // The system prompt is the harness, not the conversation:
                    // clearing drops what was said, and the continuation is the
                    // only instruction the next request carries.
                    messages.retain(|message| matches!(message, ModelMessage::System { .. }));
                    messages.push(ModelMessage::User {
                        content: continuation,
                    });
                    recorder.emit(EngineEvent::SessionHandoff {
                        from_session_id,
                        to_session_id,
                        cause: SessionHandoffCause::ContextCleared { plan_file_path },
                    })?;
                    cleared = true;
                }
            }
        }
        Ok(ControlOutcome::Continue(cleared))
    }

    /// Streams one provider completion, projecting text and reasoning as it arrives.
    async fn stream_completion(
        &self,
        recorder: &mut TurnRecorder<'_>,
        input: &ProviderInput,
        cancellation: &CancellationToken,
    ) -> Result<StreamOutcome, EngineError> {
        // Retries are reported while the request is still waiting: a client
        // renders the wait, so learning about it once the backend gave up would
        // be too late to be worth anything.
        let (retry_sender, mut retry_reasons) = mpsc::unbounded_channel();
        let retries = ChannelRetrySink {
            reasons: retry_sender,
        };
        let mut opening = self.provider.stream_observed(input, &retries);
        let mut stream = loop {
            tokio::select! {
                result = &mut opening => break match result {
                    Ok(stream) => stream,
                    Err(error) => return Ok(StreamOutcome::Completed(Err(error))),
                },
                Some(reason) = retry_reasons.recv() => {
                    recorder.emit(EngineEvent::Retrying { reason })?;
                }
                () = cancellation.cancelled() => return Ok(StreamOutcome::Cancelled),
            }
        };
        drop(opening);
        while let Ok(reason) = retry_reasons.try_recv() {
            recorder.emit(EngineEvent::Retrying { reason })?;
        }
        let mut chunks = Vec::new();
        loop {
            let next = tokio::select! {
                next = stream.chunks.next() => next,
                () = cancellation.cancelled() => return Ok(StreamOutcome::Cancelled),
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => return Ok(StreamOutcome::Completed(Err(error))),
            };
            match &chunk {
                ProviderChunk::Text { text } if !text.is_empty() => {
                    recorder.emit(EngineEvent::ModelText { text: text.clone() })?;
                }
                ProviderChunk::Reasoning { text, .. } if !text.is_empty() => {
                    recorder.emit(EngineEvent::ModelReasoning { text: text.clone() })?;
                }
                ProviderChunk::Text { .. }
                | ProviderChunk::Reasoning { .. }
                | ProviderChunk::ToolCall { .. }
                | ProviderChunk::Usage { .. }
                | ProviderChunk::Refusal { .. }
                | ProviderChunk::Stop { .. } => {}
            }
            chunks.push(chunk);
        }
        if chunks.is_empty() {
            return Ok(StreamOutcome::Completed(Err(
                ProviderError::MalformedStream(
                    "provider stream produced no typed chunks".to_owned(),
                ),
            )));
        }
        Ok(StreamOutcome::Completed(
            aggregate_provider_chunks(chunks, stream.correlation_id).map(Box::new),
        ))
    }

    /// Compacts the transcript after a context overflow.
    ///
    /// `None` means the turn was cancelled while compaction was running.
    async fn compact(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        cancellation: &CancellationToken,
    ) -> Result<Option<()>, EngineError> {
        let compaction = tokio::select! {
            result = self.compactor.compact(&recorder.state().session_id, messages) => result,
            () = cancellation.cancelled() => return Ok(None),
        }
        .map_err(EngineError::Compaction)?;
        recorder.emit(EngineEvent::Compaction {
            summary: compaction.summary,
        })?;
        let from_session_id = recorder.state().session_id.clone();
        recorder.emit(EngineEvent::SessionHandoff {
            from_session_id,
            to_session_id: compaction.new_session_id,
            cause: SessionHandoffCause::Compaction,
        })?;
        *messages = compaction.messages;
        Ok(Some(()))
    }

    /// Runs every declared tool call concurrently, in declaration order.
    ///
    /// Events follow completion order so observers see progress as it happens,
    /// while the returned results follow declaration order so the transcript
    /// always answers the model's calls in the order it made them. Cancellation
    /// still yields one result per call: an unanswered call would leave the
    /// transcript malformed for the next request.
    async fn execute_tool_calls(
        &self,
        recorder: &mut TurnRecorder<'_>,
        tool_calls: &[ModelToolCall],
        cancellation: &CancellationToken,
    ) -> Result<Vec<(String, bool)>, EngineError> {
        for call in tool_calls {
            recorder.emit(EngineEvent::ToolCall {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })?;
        }
        let mut pending = FuturesUnordered::new();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(TOOL_STREAM_CAPACITY);
        for (index, call) in tool_calls.iter().enumerate() {
            let started = Instant::now();
            let sender = stream_tx.clone();
            let output: ToolStreamSink = Arc::new(move |chunk| {
                sender
                    .try_send((index, chunk))
                    .map_err(|error| format!("tool output backpressure: {error}"))
            });
            pending.push(
                async move {
                    (
                        index,
                        started,
                        self.tools
                            .execute_stream(&call.name, &call.arguments, output)
                            .await,
                    )
                }
                .boxed(),
            );
        }
        drop(stream_tx);

        let mut results = vec![None; tool_calls.len()];
        while !pending.is_empty() {
            tokio::select! {
                streamed = stream_rx.recv() => {
                    if let Some((index, chunk)) = streamed {
                        recorder.emit(EngineEvent::ToolStream {
                            call_id: tool_calls[index].id.clone(),
                            chunk,
                        })?;
                    }
                }
                next = pending.next() => {
                    let Some((index, started, result)) = next else {
                        break;
                    };
                    drain_tool_stream(recorder, tool_calls, &mut stream_rx)?;
                    let duration_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let (output, is_error) = match result {
                        Ok(output) => (output, false),
                        Err(message) => (
                            ToolExecutionOutput::text(bounded_utf8(
                                &message,
                                MAX_TOOL_ERROR_BYTES,
                                "…",
                            )),
                            true,
                        ),
                    };
                    recorder.emit(EngineEvent::ToolResult {
                        call_id: tool_calls[index].id.clone(),
                        content: output.model_text.clone(),
                        typed_result: output.typed_result,
                        display: output.display,
                        duration_ms,
                        is_error,
                        cancelled: false,
                    })?;
                    results[index] = Some((output.model_text, is_error));
                }
                () = cancellation.cancelled() => break,
            }
        }
        drop(pending);
        drain_tool_stream(recorder, tool_calls, &mut stream_rx)?;

        for (index, result) in results.iter_mut().enumerate() {
            if result.is_some() {
                continue;
            }
            recorder.emit(EngineEvent::ToolResult {
                call_id: tool_calls[index].id.clone(),
                content: INTERRUPTED_TOOL_RESULT.to_owned(),
                typed_result: Value::Null,
                display: Value::Null,
                duration_ms: 0,
                is_error: true,
                cancelled: true,
            })?;
            *result = Some((INTERRUPTED_TOOL_RESULT.to_owned(), true));
        }
        Ok(results
            .into_iter()
            .map(|result| result.unwrap_or_else(|| (INTERRUPTED_TOOL_RESULT.to_owned(), true)))
            .collect())
    }
}

/// Projects every chunk a tool emitted before its result arrived.
fn drain_tool_stream(
    recorder: &mut TurnRecorder<'_>,
    tool_calls: &[ModelToolCall],
    stream_rx: &mut tokio::sync::mpsc::Receiver<(usize, String)>,
) -> Result<(), EngineError> {
    while let Ok((index, chunk)) = stream_rx.try_recv() {
        recorder.emit(EngineEvent::ToolStream {
            call_id: tool_calls[index].id.clone(),
            chunk,
        })?;
    }
    Ok(())
}

/// One provider exchange: either it produced a response, or the turn was cancelled.
enum StreamOutcome {
    /// The assistant message is boxed: it dwarfs the cancelled variant.
    Completed(Result<Box<AssistantMessage>, ProviderError>),
    Cancelled,
}

/// Accumulates everything a turn spends against [`EngineLimits`].
struct TurnLedger {
    usage: Usage,
    context_tokens: u64,
    price_micros: u64,
    steps: u32,
}

impl TurnLedger {
    fn new(baseline: &SessionStats, limits: &EngineLimits) -> Self {
        Self {
            price_micros: total_price_micros(&baseline.usage, limits),
            usage: baseline.usage.clone(),
            context_tokens: baseline.context_tokens,
            steps: baseline.steps,
        }
    }

    fn total_tokens(&self) -> u64 {
        self.usage
            .input_tokens
            .saturating_add(self.usage.output_tokens)
    }

    fn record_completion(&mut self, usage: &Usage, limits: &EngineLimits) {
        self.steps = self.steps.saturating_add(1);
        self.context_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.price_micros = total_price_micros(&self.usage, limits);
    }
}

/// Collects a turn's events while keeping the public projection in step.
///
/// Every engine event flows through [`TurnRecorder::emit`], which is the single
/// place where an event is stamped, reduced, observed, and retained.
struct TurnRecorder<'a> {
    observer: &'a dyn EventObserver,
    reducer: ProjectionReducer,
    events: Vec<EventEnvelope>,
    next_event_id: u64,
}

impl<'a> TurnRecorder<'a> {
    fn new(
        observer: &'a dyn EventObserver,
        session_id: impl Into<String>,
        turn_id: Option<&str>,
    ) -> Self {
        let session_id = session_id.into();
        Self {
            observer,
            reducer: turn_id.map_or_else(
                || ProjectionReducer::new(&session_id),
                |turn_id| ProjectionReducer::for_turn(&session_id, turn_id),
            ),
            events: Vec::new(),
            next_event_id: 1,
        }
    }

    fn state(&self) -> &ProjectionSnapshot {
        self.reducer.state()
    }

    fn has_callback(&self, callback_id: &str) -> bool {
        self.state().history.iter().any(|entry| {
            matches!(
                entry,
                crate::events::PublicHistoryEntry::Callback {
                    callback_id: projected,
                    ..
                } if projected == callback_id
            )
        })
    }

    fn emit(&mut self, event: EngineEvent) -> Result<(), EngineError> {
        let envelope = EventEnvelope {
            session_id: self.state().session_id.clone(),
            turn_id: self.state().turn_id.clone(),
            emitted_at: current_time_millis(),
            event_id: self.next_event_id,
            event,
        };
        self.reducer.apply(&envelope)?;
        self.observer
            .observe(&envelope)
            .map_err(EngineError::Observation)?;
        self.events.push(envelope);
        self.next_event_id = self.next_event_id.saturating_add(1);
        Ok(())
    }

    fn finish(self) -> (Vec<EventEnvelope>, ProjectionSnapshot) {
        (self.events, self.reducer.into_state())
    }
}

async fn persist(
    sink: &impl TranscriptSink,
    messages: &[ModelMessage],
    snapshot: &ProjectionSnapshot,
) -> Result<(), EngineError> {
    sink.persist(messages, snapshot)
        .await
        .map_err(EngineError::Persistence)
}

async fn persist_stats(
    sink: &impl TranscriptSink,
    stats: &SessionStats,
) -> Result<(), EngineError> {
    sink.persist_stats(stats)
        .await
        .map_err(EngineError::Persistence)
}

/// Total price of `usage` under `limits`, in micros.
fn total_price_micros(usage: &Usage, limits: &EngineLimits) -> u64 {
    priced_tokens(usage.input_tokens, limits.input_price_per_million_micros).saturating_add(
        priced_tokens(usage.output_tokens, limits.output_price_per_million_micros),
    )
}

/// The lifecycle state a stop reason lands in.
const fn lifecycle_for(reason: &TurnStopReason) -> LifecycleState {
    match reason {
        TurnStopReason::Complete
        | TurnStopReason::MaxSteps
        | TurnStopReason::TokenLimit
        | TurnStopReason::PriceLimit => LifecycleState::Completed,
        TurnStopReason::Cancelled => LifecycleState::Cancelled,
        TurnStopReason::Failed | TurnStopReason::Refusal | TurnStopReason::ResponseLength => {
            LifecycleState::Failed
        }
    }
}

/// Price of `tokens` at `price_per_million_micros`, rounded down.
///
/// The multiplication happens in `u128` so a large token count at a large unit
/// price cannot wrap before the division brings it back into range.
fn priced_tokens(tokens: u64, price_per_million_micros: u64) -> u64 {
    const MILLION: u128 = 1_000_000;
    u64::try_from(u128::from(tokens).saturating_mul(u128::from(price_per_million_micros)) / MILLION)
        .unwrap_or(u64::MAX)
}

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn title_from_messages(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            ModelMessage::User { content } => Some(content),
            _ => None,
        })
        .map(|content| {
            let mut title = content.chars().take(60).collect::<String>();
            if content.chars().count() > 60 {
                title.push('…');
            }
            title
        })
        .unwrap_or_else(|| "New session".to_owned())
}

fn stop_message(reason: &TurnStopReason) -> &'static str {
    match reason {
        TurnStopReason::Complete => "Turn completed",
        TurnStopReason::MaxSteps => "Maximum turn steps reached",
        TurnStopReason::TokenLimit => "Token limit reached",
        TurnStopReason::PriceLimit => "Price limit reached",
        TurnStopReason::Refusal => "Provider refused the request",
        TurnStopReason::ResponseLength => {
            "The model's response exceeded the maximum output token limit."
        }
        TurnStopReason::Cancelled => "Turn cancelled",
        TurnStopReason::Failed => "Turn failed",
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Provider(ProviderError),
    #[error("compaction failed: {0}")]
    Compaction(String),
    #[error("persistence failed: {0}")]
    Persistence(String),
    #[error("event observation failed: {0}")]
    Observation(String),
    #[error("turn control state lock is poisoned")]
    ControlStatePoisoned,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;
    use crate::events::ModelToolCall;
    use crate::provider::{ProviderChunk, RequestLimits};
    use serde_json::json;

    #[derive(Default)]
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Result<AssistantMessage, ProviderError>>>,
    }

    impl ScriptedProvider {
        fn new(
            responses: impl IntoIterator<Item = Result<AssistantMessage, ProviderError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }
    }

    impl CompletionProvider for ScriptedProvider {
        fn complete<'a>(&'a self, _input: &'a ProviderInput) -> ProviderFuture<'a> {
            Box::pin(async move {
                self.responses
                    .lock()
                    .map_err(|_| ProviderError::MalformedStream("fake lock poisoned".to_owned()))?
                    .pop_front()
                    .ok_or_else(|| {
                        ProviderError::MalformedStream("missing fake response".to_owned())
                    })?
            })
        }
    }

    struct FakeTools;

    impl ToolExecutor for FakeTools {
        fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolExecutionOutput::text(format!("{name}:{arguments}"))) })
        }
    }

    #[derive(Clone, Default)]
    struct BlockingTools {
        started: Arc<Notify>,
    }

    impl ToolExecutor for BlockingTools {
        fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
            Box::pin(async move {
                self.started.notify_one();
                std::future::pending::<Result<ToolExecutionOutput, String>>().await
            })
        }
    }

    #[derive(Clone, Default)]
    struct ControlledTools {
        started_count: Arc<AtomicUsize>,
        all_started: Arc<Notify>,
        release_first: Arc<Notify>,
        release_second: Arc<Notify>,
    }

    impl ToolExecutor for ControlledTools {
        fn execute<'a>(&'a self, name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
            Box::pin(async move {
                if self.started_count.fetch_add(1, AtomicOrdering::SeqCst) == 1 {
                    self.all_started.notify_one();
                }
                match name {
                    "first" => self.release_first.notified().await,
                    "second" => self.release_second.notified().await,
                    _ => return Err("unexpected tool".to_owned()),
                }
                Ok(ToolExecutionOutput {
                    typed_result: json!({"tool": name}),
                    model_text: format!("{name}-result"),
                    display: json!({"summary": name}),
                    chunks: Vec::new(),
                })
            })
        }

        fn execute_stream<'a>(
            &'a self,
            name: &'a str,
            arguments: &'a str,
            output: ToolStreamSink,
        ) -> ToolFuture<'a> {
            Box::pin(async move {
                output(format!("{name}-chunk"))?;
                self.execute(name, arguments).await
            })
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<EngineEvent>>,
        result_seen: Notify,
        stream_seen: Notify,
    }

    impl EventObserver for RecordingObserver {
        fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
            if matches!(event.event, EngineEvent::ToolResult { .. }) {
                self.result_seen.notify_one();
            }
            if matches!(event.event, EngineEvent::ToolStream { .. }) {
                self.stream_seen.notify_one();
            }
            self.events
                .lock()
                .map_err(|_| "recording observer lock poisoned".to_owned())?
                .push(event.event.clone());
            Ok(())
        }
    }

    struct FakeCompactor;

    impl Compactor for FakeCompactor {
        fn compact<'a>(
            &'a self,
            _current_session_id: &'a str,
            _messages: &'a [ModelMessage],
        ) -> CompactionFuture<'a> {
            Box::pin(async {
                Ok(CompactionResult {
                    new_session_id: "session-2".to_owned(),
                    summary: "compact summary".to_owned(),
                    messages: vec![ModelMessage::System {
                        content: "summary".to_owned(),
                    }],
                })
            })
        }

        fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
            Ok(format!("{current_session_id}-cleared"))
        }
    }

    #[derive(Clone, Default)]
    struct BlockingProvider {
        started: Arc<Notify>,
    }

    impl CompletionProvider for BlockingProvider {
        fn complete<'a>(&'a self, _input: &'a ProviderInput) -> ProviderFuture<'a> {
            Box::pin(std::future::pending())
        }

        fn stream<'a>(&'a self, _input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                Ok(ProviderStream {
                    correlation_id: None,
                    chunks: Box::pin(futures_util::stream::once(async move {
                        started.notify_one();
                        std::future::pending::<Result<ProviderChunk, ProviderError>>().await
                    })),
                })
            })
        }
    }

    #[derive(Clone, Default)]
    struct BlockingCompactor {
        started: Arc<Notify>,
    }

    impl Compactor for BlockingCompactor {
        fn compact<'a>(
            &'a self,
            _current_session_id: &'a str,
            _messages: &'a [ModelMessage],
        ) -> CompactionFuture<'a> {
            Box::pin(async move {
                self.started.notify_one();
                std::future::pending::<Result<CompactionResult, String>>().await
            })
        }

        fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
            Ok(format!("{current_session_id}-cleared"))
        }
    }

    #[derive(Clone, Default)]
    struct BlockingSink {
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl TranscriptSink for BlockingSink {
        fn persist<'a>(
            &'a self,
            _messages: &'a [ModelMessage],
            _snapshot: &'a ProjectionSnapshot,
        ) -> PersistenceFuture<'a> {
            Box::pin(async move {
                if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Ok(())
            })
        }
    }

    /// A provider that reports one retry before answering, the way a backend
    /// waiting on a retryable status does.
    struct RetryingProvider {
        inner: ScriptedProvider,
    }

    impl CompletionProvider for RetryingProvider {
        fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
            self.inner.complete(input)
        }

        fn stream_observed<'a>(
            &'a self,
            input: &'a ProviderInput,
            retries: &'a (dyn RetrySink + 'a),
        ) -> ProviderStreamFuture<'a> {
            retries.retrying("provider answered HTTP 503");
            self.stream(input)
        }
    }

    /// A retry is a fact about the wait, so the turn reports it while it is
    /// still waiting rather than folding it into the outcome.
    #[tokio::test]
    async fn a_retried_request_is_reported_as_a_turn_event() {
        let outcome = ConversationEngine::new(RetryingProvider {
            inner: ScriptedProvider::new([Ok(completion("answer", Vec::new()))]),
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("turn completes");
        assert_eq!(
            outcome
                .events
                .iter()
                .filter_map(|event| match &event.event {
                    EngineEvent::Retrying { reason } => Some(reason.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["provider answered HTTP 503"]
        );
    }

    fn provider_input() -> ProviderInput {
        ProviderInput {
            turn_id: None,
            model_override: None,
            messages: vec![ModelMessage::System {
                content: "system".to_owned(),
            }],
            stream: true,
            images: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            thinking: false,
            reasoning_effort: None,
            headers: BTreeMap::new(),
            limits: RequestLimits::default(),
            metadata: BTreeMap::new(),
        }
    }

    fn completion(text: &str, calls: Vec<ModelToolCall>) -> AssistantMessage {
        AssistantMessage {
            text: text.to_owned(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: calls,
            usage: Usage {
                input_tokens: 2,
                output_tokens: 3,
            },
            refusal: None,
            stop_reason: "stop".to_owned(),
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn tool_cycles_checkpoint_and_finalize_once() {
        let provider = ScriptedProvider::new([
            Ok(completion(
                "",
                vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            )),
            Ok(completion("done", Vec::new())),
        ]);
        let outcome = ConversationEngine::new(provider)
            .with_tools(FakeTools)
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("turn completes");
        assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.usage.output_tokens, 6);
        assert!(matches!(
            outcome.messages.last(),
            Some(ModelMessage::Assistant { content, .. }) if content == "done"
        ));
        assert_eq!(
            outcome
                .events
                .iter()
                .filter(|event| matches!(
                    event.event,
                    EngineEvent::Lifecycle {
                        state: LifecycleState::Completed,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn parallel_tool_events_follow_arrival_but_transcript_follows_declaration() {
        let provider = ScriptedProvider::new([
            Ok(completion(
                "",
                vec![
                    ModelToolCall {
                        id: "call-1".to_owned(),
                        name: "first".to_owned(),
                        arguments: "{}".to_owned(),
                    },
                    ModelToolCall {
                        id: "call-2".to_owned(),
                        name: "second".to_owned(),
                        arguments: "{}".to_owned(),
                    },
                ],
            )),
            Ok(completion("done", Vec::new())),
        ]);
        let tools = ControlledTools::default();
        let controls = tools.clone();
        let observer = Arc::new(RecordingObserver::default());
        let recorded = observer.clone();
        let task = tokio::spawn(async move {
            ConversationEngine::new(provider)
                .with_tools(tools)
                .with_observer(observer)
                .run_turn(
                    "session-1",
                    provider_input(),
                    "hello",
                    CancellationToken::default(),
                )
                .await
        });

        controls.all_started.notified().await;
        recorded.stream_seen.notified().await;
        assert!(
            recorded
                .events
                .lock()
                .expect("observer lock")
                .iter()
                .all(|event| !matches!(event, EngineEvent::ToolResult { .. }))
        );
        controls.release_second.notify_one();
        recorded.result_seen.notified().await;
        let first_completed_call = recorded
            .events
            .lock()
            .expect("observer lock")
            .iter()
            .find_map(|event| match event {
                EngineEvent::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            });
        assert_eq!(first_completed_call.as_deref(), Some("call-2"));
        controls.release_first.notify_one();

        let outcome = task.await.expect("engine joins").expect("turn completes");
        let tool_messages = outcome
            .messages
            .iter()
            .filter_map(|message| match message {
                ModelMessage::Tool {
                    call_id, content, ..
                } => Some((call_id.as_str(), content.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tool_messages,
            vec![("call-1", "first-result"), ("call-2", "second-result")]
        );
        let events = recorded.events.lock().expect("observer lock");
        let second_stream = events.iter().position(|event| {
            matches!(
                event,
                EngineEvent::ToolStream { call_id, chunk }
                    if call_id == "call-2" && chunk == "second-chunk"
            )
        });
        let second_result = events.iter().position(|event| {
            matches!(
                event,
                EngineEvent::ToolResult { call_id, .. } if call_id == "call-2"
            )
        });
        assert!(second_stream < second_result);
    }

    #[tokio::test]
    async fn reactive_compaction_retries_without_spending_step_budget() {
        let provider = ScriptedProvider::new([
            Err(ProviderError::ContextOverflow),
            Ok(completion("after compact", Vec::new())),
        ]);
        let outcome = ConversationEngine::new(provider)
            .with_compactor(FakeCompactor)
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("compacted turn completes");
        assert_eq!(outcome.session_id, "session-2");
        assert_eq!(outcome.steps, 1);
        assert!(
            outcome
                .events
                .iter()
                .any(|event| matches!(event.event, EngineEvent::SessionHandoff { .. }))
        );
    }

    /// A tool that accepts a plan clears the context from inside the turn, the
    /// way the reference does: the transcript is dropped, the session rotates,
    /// and the continuation is the only thing the next request carries.
    #[tokio::test]
    async fn a_cleared_context_rotates_the_session_and_keeps_only_the_continuation() {
        struct ClearingTools {
            controls: TurnControlHandle,
        }

        impl ToolExecutor for ClearingTools {
            fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
                Box::pin(async move {
                    self.controls
                        .send(TurnControl::ClearContext {
                            continuation: "Plan approved. Switch to code mode.".to_owned(),
                            plan_file_path: Some("/plans/session-1.md".to_owned()),
                        })
                        .map_err(|error| error.to_string())?;
                    Ok(ToolExecutionOutput::text("plan accepted"))
                })
            }
        }

        let provider = ScriptedProvider::new([
            Ok(completion(
                "",
                vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "exit_plan_mode".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            )),
            Ok(completion("implementing", Vec::new())),
        ]);
        let controls = TurnControlHandle::default();
        let outcome = ConversationEngine::new(provider)
            .with_tools(ClearingTools {
                controls: controls.clone(),
            })
            .with_compactor(FakeCompactor)
            .run_turn_controlled(
                "session-1",
                provider_input(),
                "write a plan",
                CancellationToken::default(),
                controls,
            )
            .await
            .expect("cleared turn completes");

        assert_eq!(outcome.session_id, "session-1-cleared");
        assert_eq!(
            outcome.messages,
            vec![
                ModelMessage::System {
                    content: "system".to_owned(),
                },
                ModelMessage::User {
                    content: "Plan approved. Switch to code mode.".to_owned(),
                },
                ModelMessage::Assistant {
                    content: "implementing".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                },
            ],
            "clearing keeps the harness and the continuation, and nothing that was said"
        );
        let handoff = outcome
            .events
            .iter()
            .find_map(|event| match &event.event {
                EngineEvent::SessionHandoff {
                    from_session_id,
                    to_session_id,
                    cause,
                } => Some((
                    from_session_id.clone(),
                    to_session_id.clone(),
                    cause.clone(),
                )),
                _ => None,
            })
            .expect("the clearing publishes a handoff");
        assert_eq!(
            handoff,
            (
                "session-1".to_owned(),
                "session-1-cleared".to_owned(),
                SessionHandoffCause::ContextCleared {
                    plan_file_path: Some("/plans/session-1.md".to_owned()),
                },
            )
        );
    }

    /// A compactor that cannot mint a cleared identifier fails the turn instead
    /// of rotating onto its own name, which a reference projection rejects.
    #[tokio::test]
    async fn clearing_without_a_compactor_fails_the_turn() {
        struct ClearingTools {
            controls: TurnControlHandle,
        }

        impl ToolExecutor for ClearingTools {
            fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
                Box::pin(async move {
                    self.controls
                        .send(TurnControl::ClearContext {
                            continuation: "continue".to_owned(),
                            plan_file_path: None,
                        })
                        .map_err(|error| error.to_string())?;
                    Ok(ToolExecutionOutput::text("plan accepted"))
                })
            }
        }

        let provider = ScriptedProvider::new([
            Ok(completion(
                "",
                vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "exit_plan_mode".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            )),
            Ok(completion("unreachable", Vec::new())),
        ]);
        let controls = TurnControlHandle::default();
        let error = ConversationEngine::new(provider)
            .with_tools(ClearingTools {
                controls: controls.clone(),
            })
            .run_turn_controlled(
                "session-1",
                provider_input(),
                "write a plan",
                CancellationToken::default(),
                controls,
            )
            .await
            .expect_err("a clearing with no compactor cannot rotate the session");
        assert!(
            matches!(&error, EngineError::Compaction(message) if message.contains("clearing")),
            "the failure names what could not be done: {error}"
        );
    }

    #[tokio::test]
    async fn limits_finish_with_typed_public_status() {
        let provider = ScriptedProvider::new([Ok(completion("too costly", Vec::new()))]);
        let outcome = ConversationEngine::new(provider)
            .with_limits(EngineLimits {
                max_total_tokens: 4,
                ..EngineLimits::default()
            })
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("limit is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::TokenLimit);
        assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Completed);
    }

    #[tokio::test]
    async fn every_terminal_limit_has_a_typed_outcome() {
        let max_steps = ConversationEngine::new(ScriptedProvider::default())
            .with_limits(EngineLimits {
                max_steps: 0,
                ..EngineLimits::default()
            })
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("max steps is an outcome");
        assert_eq!(max_steps.stop_reason, TurnStopReason::MaxSteps);

        let price =
            ConversationEngine::new(ScriptedProvider::new([Ok(completion("cost", Vec::new()))]))
                .with_limits(EngineLimits {
                    max_price_micros: 4,
                    input_price_per_million_micros: 1_000_000,
                    output_price_per_million_micros: 1_000_000,
                    ..EngineLimits::default()
                })
                .run_turn(
                    "session-1",
                    provider_input(),
                    "hello",
                    CancellationToken::default(),
                )
                .await
                .expect("price limit is an outcome");
        assert_eq!(price.stop_reason, TurnStopReason::PriceLimit);
        assert_eq!(price.price_micros, 5);
        assert_eq!(price.snapshot.lifecycle, LifecycleState::Completed);
        assert!(matches!(
            price.messages.last(),
            Some(ModelMessage::Assistant { content, .. }) if content == "cost"
        ));

        let refusal = ConversationEngine::new(ScriptedProvider::new([Err(
            ProviderError::Refusal("policy".to_owned()),
        )]))
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("refusal is an outcome");
        assert_eq!(refusal.stop_reason, TurnStopReason::Refusal);

        let response_length =
            ConversationEngine::new(ScriptedProvider::new([Ok(completion("long", Vec::new()))]))
                .with_limits(EngineLimits {
                    max_response_bytes: 3,
                    ..EngineLimits::default()
                })
                .run_turn(
                    "session-1",
                    provider_input(),
                    "hello",
                    CancellationToken::default(),
                )
                .await
                .expect("response length is an outcome");
        assert_eq!(response_length.stop_reason, TurnStopReason::ResponseLength);
    }

    #[tokio::test]
    async fn resumed_session_limits_include_the_persisted_usage_baseline() {
        let outcome = ConversationEngine::new(ScriptedProvider::default())
            .with_limits(EngineLimits {
                max_total_tokens: 4,
                input_price_per_million_micros: 1_000_000,
                output_price_per_million_micros: 1_000_000,
                ..EngineLimits::default()
            })
            .with_baseline(SessionStats {
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                },
                context_tokens: 5,
                steps: 1,
            })
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("baseline limit is a typed outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::TokenLimit);
        assert_eq!(outcome.usage.input_tokens, 3);
        assert_eq!(outcome.usage.output_tokens, 2);
        assert_eq!(outcome.context_tokens, 5);
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.price_micros, 5);
        assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Completed);
    }

    #[tokio::test]
    async fn pre_cancelled_turn_finalizes_once_without_provider_work() {
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let outcome = ConversationEngine::new(ScriptedProvider::default())
            .run_turn("session-1", provider_input(), "hello", cancellation)
            .await
            .expect("cancellation is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        assert_eq!(outcome.steps, 0);
        assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_during_persistence_drains_then_finalizes_once() {
        let sink = BlockingSink::default();
        let observer = sink.clone();
        let cancellation = CancellationToken::default();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            ConversationEngine::new(ScriptedProvider::default())
                .with_sink(sink)
                .run_turn("session-1", provider_input(), "hello", cancellation)
                .await
        });
        observer.started.notified().await;
        cancel.cancel();
        observer.release.notify_one();
        let outcome = task
            .await
            .expect("engine task joins")
            .expect("cancellation is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
        assert_eq!(
            outcome
                .events
                .iter()
                .filter(|event| matches!(
                    event.event,
                    EngineEvent::Lifecycle {
                        state: LifecycleState::Cancelled,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(observer.calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_repairs_every_declared_tool_result() {
        let provider = ScriptedProvider::new([Ok(completion(
            "",
            vec![
                ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "first".to_owned(),
                    arguments: "{}".to_owned(),
                },
                ModelToolCall {
                    id: "call-2".to_owned(),
                    name: "second".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ],
        ))]);
        let tools = BlockingTools::default();
        let observer = tools.clone();
        let cancellation = CancellationToken::default();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            ConversationEngine::new(provider)
                .with_tools(tools)
                .run_turn("session-1", provider_input(), "hello", cancellation)
                .await
        });
        observer.started.notified().await;
        cancel.cancel();
        let outcome = task
            .await
            .expect("engine task joins")
            .expect("cancellation is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        assert_eq!(
            outcome
                .messages
                .iter()
                .filter(|message| matches!(message, ModelMessage::Tool { is_error: true, .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_during_provider_stream_finalizes_once() {
        let provider = BlockingProvider::default();
        let observer = provider.clone();
        let cancellation = CancellationToken::default();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            ConversationEngine::new(provider)
                .run_turn("session-1", provider_input(), "hello", cancellation)
                .await
        });
        observer.started.notified().await;
        cancel.cancel();
        let outcome = task
            .await
            .expect("engine task joins")
            .expect("cancellation is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_during_compaction_finalizes_once() {
        let compactor = BlockingCompactor::default();
        let observer = compactor.clone();
        let cancellation = CancellationToken::default();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            ConversationEngine::new(ScriptedProvider::new([Err(ProviderError::ContextOverflow)]))
                .with_compactor(compactor)
                .run_turn("session-1", provider_input(), "hello", cancellation)
                .await
        });
        observer.started.notified().await;
        cancel.cancel();
        let outcome = task
            .await
            .expect("engine task joins")
            .expect("cancellation is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
    }

    #[tokio::test]
    async fn transcript_checkpoints_append_and_rewrite_a_diverged_history() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("sink");
        let metadata = store
            .create("session-sink", "/workspace", None, 10)
            .expect("session creates");
        let sink = SessionTranscriptSink::new(store.clone(), metadata);
        let snapshot = ProjectionReducer::new("session-sink").state().clone();
        let system = ModelMessage::System {
            content: "system".to_owned(),
        };
        let first = ModelMessage::User {
            content: "first".to_owned(),
        };
        let second = ModelMessage::User {
            content: "second".to_owned(),
        };

        sink.persist(&[system.clone(), first.clone()], &snapshot)
            .await
            .expect("first checkpoint");
        sink.persist(&[system.clone(), first.clone(), second.clone()], &snapshot)
            .await
            .expect("appending checkpoint");
        assert_eq!(
            store
                .load("session-sink")
                .expect("loads")
                .messages
                .iter()
                .filter_map(|message| match message {
                    ModelMessage::User { content } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );

        // A rewind shortens the history: the log must be rewritten, not extended.
        sink.persist(&[system, first], &snapshot)
            .await
            .expect("rewound checkpoint");
        let stored = store.load("session-sink").expect("loads");
        assert_eq!(stored.messages.len(), 1);
        assert!(matches!(
            stored.messages.first(),
            Some(ModelMessage::User { content }) if content == "first"
        ));
    }

    #[test]
    fn provider_chunk_schema_remains_typed() {
        let chunk = ProviderChunk::Text {
            text: "hello".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(chunk).expect("chunk serializes")["type"],
            "text"
        );
    }
}
