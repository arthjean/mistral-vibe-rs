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
use tokio::sync::Notify;

use crate::events::{
    EngineEvent, EventEnvelope, LifecycleState, ModelMessage, ProjectionError, ProjectionReducer,
    ProjectionSnapshot,
};
use crate::provider::{
    AssistantMessage, ProviderBackend, ProviderChunk, ProviderError, ProviderInput, ProviderStream,
    ProviderTransport, TransportError, Usage, aggregate_provider_chunks,
};
use crate::storage::{SessionMetadata, SessionStore, StorageError};
use crate::tools::ToolExecutionOutput;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub usage: Usage,
    pub context_tokens: u64,
    pub steps: u32,
}

pub trait CompletionProvider: Send + Sync {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a>;

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
            let stored = self
                .store
                .load(&metadata.id)
                .map_err(|error| error.to_string())?;
            if let Some(system) = messages
                .iter()
                .find(|message| matches!(message, ModelMessage::System { .. }))
            {
                self.store
                    .append_message(&mut metadata, system, persisted_at)
                    .map_err(|error| error.to_string())?;
            }
            let non_system_messages: Vec<_> = messages
                .iter()
                .filter(|message| !matches!(message, ModelMessage::System { .. }))
                .collect();
            let boundary_unchanged = stored.messages.len() <= non_system_messages.len()
                && stored
                    .messages
                    .iter()
                    .zip(&non_system_messages)
                    .all(|(stored, current)| stored == *current);
            if !boundary_unchanged {
                self.store
                    .replace_messages(&mut metadata, messages, persisted_at)
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            for message in &non_system_messages[stored.messages.len()..] {
                self.store
                    .append_message(&mut metadata, message, persisted_at)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
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
        let session_id = session_id.into();
        let prompt = prompt.into();
        let mut reducer = input.turn_id.as_ref().map_or_else(
            || ProjectionReducer::new(&session_id),
            |turn_id| ProjectionReducer::for_turn(&session_id, turn_id),
        );
        let mut events = Vec::new();
        let mut messages = input.messages.clone();
        let mut usage = self.baseline.usage.clone();
        let mut price_micros = priced_tokens(
            usage.input_tokens,
            self.limits.input_price_per_million_micros,
        )
        .saturating_add(priced_tokens(
            usage.output_tokens,
            self.limits.output_price_per_million_micros,
        ));
        let mut context_tokens = self.baseline.context_tokens;
        let mut steps = self.baseline.steps;
        let mut checkpoints = 0_u32;
        let mut next_event_id = 1_u64;
        let mut finalized = false;

        emit(
            self.observer.as_ref(),
            &mut reducer,
            &mut events,
            &mut next_event_id,
            EngineEvent::UserMessage {
                content: prompt.clone(),
            },
        )?;
        messages.push(ModelMessage::User { content: prompt });
        emit(
            self.observer.as_ref(),
            &mut reducer,
            &mut events,
            &mut next_event_id,
            EngineEvent::Title {
                title: title_from_messages(&messages),
            },
        )?;
        persist_owned(&self.sink, &messages, reducer.state()).await?;
        checkpoints = checkpoints.saturating_add(1);

        let stop_reason = 'turn: loop {
            if cancellation.is_cancelled() {
                break TurnStopReason::Cancelled;
            }
            if steps >= self.limits.max_steps {
                break TurnStopReason::MaxSteps;
            }
            if usage.input_tokens.saturating_add(usage.output_tokens) > self.limits.max_total_tokens
            {
                break TurnStopReason::TokenLimit;
            }
            if price_micros > self.limits.max_price_micros {
                break TurnStopReason::PriceLimit;
            }
            for control in controls.drain()? {
                match control {
                    TurnControl::Steer { content } => {
                        emit(
                            self.observer.as_ref(),
                            &mut reducer,
                            &mut events,
                            &mut next_event_id,
                            EngineEvent::UserSteer {
                                content: content.clone(),
                            },
                        )?;
                        messages.push(ModelMessage::User { content });
                    }
                    TurnControl::InjectContext {
                        content,
                        as_message,
                    } => {
                        emit(
                            self.observer.as_ref(),
                            &mut reducer,
                            &mut events,
                            &mut next_event_id,
                            EngineEvent::ContextInjected {
                                content: content.clone(),
                                as_message,
                            },
                        )?;
                        messages.push(ModelMessage::User { content });
                    }
                    TurnControl::ResolveCallback {
                        callback_id,
                        accepted,
                        value,
                    } => {
                        let callback_is_projected = reducer.state().history.iter().any(|entry| {
                            matches!(
                                entry,
                                crate::events::PublicHistoryEntry::Callback {
                                    callback_id: projected_id,
                                    ..
                                } if projected_id == &callback_id
                            )
                        });
                        if callback_is_projected {
                            emit(
                                self.observer.as_ref(),
                                &mut reducer,
                                &mut events,
                                &mut next_event_id,
                                EngineEvent::CallbackResolved {
                                    callback_id,
                                    accepted,
                                    value,
                                },
                            )?;
                        }
                        if !accepted {
                            break 'turn TurnStopReason::Cancelled;
                        }
                    }
                }
            }
            input.messages.clone_from(&messages);
            let provider_stream = tokio::select! {
                result = self.provider.stream(&input) => result,
                () = cancellation.cancelled() => break TurnStopReason::Cancelled,
            };
            let completion = match provider_stream {
                Ok(mut provider_stream) => {
                    let mut chunks = Vec::new();
                    let stream_result = loop {
                        let next = tokio::select! {
                            next = provider_stream.chunks.next() => next,
                            () = cancellation.cancelled() => {
                                break 'turn TurnStopReason::Cancelled;
                            }
                        };
                        let Some(chunk) = next else {
                            break Ok(());
                        };
                        let chunk = match chunk {
                            Ok(chunk) => chunk,
                            Err(error) => break Err(error),
                        };
                        match &chunk {
                            ProviderChunk::Text { text } if !text.is_empty() => {
                                emit(
                                    self.observer.as_ref(),
                                    &mut reducer,
                                    &mut events,
                                    &mut next_event_id,
                                    EngineEvent::ModelText { text: text.clone() },
                                )?;
                            }
                            ProviderChunk::Reasoning { text, .. } if !text.is_empty() => {
                                emit(
                                    self.observer.as_ref(),
                                    &mut reducer,
                                    &mut events,
                                    &mut next_event_id,
                                    EngineEvent::ModelReasoning { text: text.clone() },
                                )?;
                            }
                            ProviderChunk::Text { .. }
                            | ProviderChunk::Reasoning { .. }
                            | ProviderChunk::ToolCall { .. }
                            | ProviderChunk::Usage { .. }
                            | ProviderChunk::Refusal { .. }
                            | ProviderChunk::Stop { .. } => {}
                        }
                        chunks.push(chunk);
                    };
                    if let Err(error) = stream_result {
                        Err(error)
                    } else if chunks.is_empty() {
                        Err(ProviderError::MalformedStream(
                            "provider stream produced no typed chunks".to_owned(),
                        ))
                    } else {
                        aggregate_provider_chunks(chunks, provider_stream.correlation_id)
                    }
                }
                Err(error) => Err(error),
            };
            let completion = match completion {
                Ok(completion) => completion,
                Err(ProviderError::ContextOverflow) => {
                    let compaction = tokio::select! {
                        result = self.compactor.compact(&reducer.state().session_id, &messages) => result,
                        () = cancellation.cancelled() => break TurnStopReason::Cancelled,
                    }
                    .map_err(EngineError::Compaction)?;
                    emit(
                        self.observer.as_ref(),
                        &mut reducer,
                        &mut events,
                        &mut next_event_id,
                        EngineEvent::Compaction {
                            summary: compaction.summary,
                        },
                    )?;
                    let old_session_id = reducer.state().session_id.clone();
                    emit(
                        self.observer.as_ref(),
                        &mut reducer,
                        &mut events,
                        &mut next_event_id,
                        EngineEvent::SessionHandoff {
                            from_session_id: old_session_id,
                            to_session_id: compaction.new_session_id,
                        },
                    )?;
                    messages = compaction.messages;
                    persist_owned(&self.sink, &messages, reducer.state()).await?;
                    checkpoints = checkpoints.saturating_add(1);
                    continue;
                }
                Err(ProviderError::Refusal(_)) => break TurnStopReason::Refusal,
                Err(ProviderError::Transport(TransportError::ResponseTooLarge { .. })) => {
                    break TurnStopReason::ResponseLength;
                }
                Err(error) => {
                    emit(
                        self.observer.as_ref(),
                        &mut reducer,
                        &mut events,
                        &mut next_event_id,
                        EngineEvent::Lifecycle {
                            state: LifecycleState::Failed,
                            message: Some(error.to_string()),
                        },
                    )?;
                    persist_owned(&self.sink, &messages, reducer.state()).await?;
                    return Err(EngineError::Provider(error));
                }
            };
            steps = steps.saturating_add(1);
            context_tokens = completion
                .usage
                .input_tokens
                .saturating_add(completion.usage.output_tokens);
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(completion.usage.input_tokens);
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(completion.usage.output_tokens);
            price_micros = priced_tokens(
                usage.input_tokens,
                self.limits.input_price_per_million_micros,
            )
            .saturating_add(priced_tokens(
                usage.output_tokens,
                self.limits.output_price_per_million_micros,
            ));
            emit(
                self.observer.as_ref(),
                &mut reducer,
                &mut events,
                &mut next_event_id,
                EngineEvent::Stats {
                    context_tokens,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                },
            )?;
            let assistant_message = ModelMessage::Assistant {
                content: completion.text.clone(),
                reasoning: completion.reasoning.clone(),
                reasoning_signature: completion.reasoning_signature.clone(),
                reasoning_state: completion.reasoning_state.clone(),
                tool_calls: completion.tool_calls.clone(),
            };
            if usage.input_tokens.saturating_add(usage.output_tokens) > self.limits.max_total_tokens
            {
                if completion.tool_calls.is_empty() {
                    messages.push(assistant_message);
                }
                break TurnStopReason::TokenLimit;
            }
            if price_micros > self.limits.max_price_micros {
                if completion.tool_calls.is_empty() {
                    messages.push(assistant_message);
                }
                break TurnStopReason::PriceLimit;
            }
            if completion.text.len() > self.limits.max_response_bytes {
                break TurnStopReason::ResponseLength;
            }

            messages.push(assistant_message);
            if completion.tool_calls.is_empty() {
                break TurnStopReason::Complete;
            }
            let tool_calls = completion.tool_calls;
            for call in &tool_calls {
                emit(
                    self.observer.as_ref(),
                    &mut reducer,
                    &mut events,
                    &mut next_event_id,
                    EngineEvent::ToolCall {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                )?;
            }
            let mut pending = FuturesUnordered::new();
            let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(256);
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
            let mut ordered_results = vec![None; tool_calls.len()];
            let mut cancellation_seen = false;
            while !pending.is_empty() {
                tokio::select! {
                    streamed = stream_rx.recv() => {
                        if let Some((index, chunk)) = streamed {
                            emit(
                                self.observer.as_ref(),
                                &mut reducer,
                                &mut events,
                                &mut next_event_id,
                                EngineEvent::ToolStream {
                                    call_id: tool_calls[index].id.clone(),
                                    chunk,
                                },
                            )?;
                        }
                    }
                    next = pending.next() => {
                        let Some((index, started, result)) = next else {
                            break;
                        };
                        while let Ok((stream_index, chunk)) = stream_rx.try_recv() {
                            emit(
                                self.observer.as_ref(),
                                &mut reducer,
                                &mut events,
                                &mut next_event_id,
                                EngineEvent::ToolStream {
                                    call_id: tool_calls[stream_index].id.clone(),
                                    chunk,
                                },
                            )?;
                        }
                        let duration_ms = u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX);
                        let (output, is_error) = match result {
                            Ok(output) => (output, false),
                            Err(message) => (
                                ToolExecutionOutput::text(bounded_tool_error(&message)),
                                true,
                            ),
                        };
                        emit(
                            self.observer.as_ref(),
                            &mut reducer,
                            &mut events,
                            &mut next_event_id,
                            EngineEvent::ToolResult {
                                call_id: tool_calls[index].id.clone(),
                                content: output.model_text.clone(),
                                typed_result: output.typed_result,
                                display: output.display,
                                duration_ms,
                                is_error,
                                cancelled: false,
                            },
                        )?;
                        ordered_results[index] = Some((output.model_text, is_error));
                    }
                    () = cancellation.cancelled(), if !cancellation_seen => {
                        cancellation_seen = true;
                        break;
                    }
                }
            }
            drop(pending);
            while let Ok((stream_index, chunk)) = stream_rx.try_recv() {
                emit(
                    self.observer.as_ref(),
                    &mut reducer,
                    &mut events,
                    &mut next_event_id,
                    EngineEvent::ToolStream {
                        call_id: tool_calls[stream_index].id.clone(),
                        chunk,
                    },
                )?;
            }
            if cancellation_seen {
                for (index, result) in ordered_results.iter_mut().enumerate() {
                    if result.is_some() {
                        continue;
                    }
                    let content = "Tool execution interrupted".to_owned();
                    emit(
                        self.observer.as_ref(),
                        &mut reducer,
                        &mut events,
                        &mut next_event_id,
                        EngineEvent::ToolResult {
                            call_id: tool_calls[index].id.clone(),
                            content: content.clone(),
                            typed_result: Value::Null,
                            display: Value::Null,
                            duration_ms: 0,
                            is_error: true,
                            cancelled: true,
                        },
                    )?;
                    *result = Some((content, true));
                }
            }
            for (call, result) in tool_calls.into_iter().zip(ordered_results) {
                let (content, is_error) =
                    result.unwrap_or_else(|| ("Tool execution interrupted".to_owned(), true));
                messages.push(ModelMessage::Tool {
                    call_id: call.id,
                    content,
                    is_error,
                });
            }
            persist_owned(&self.sink, &messages, reducer.state()).await?;
            checkpoints = checkpoints.saturating_add(1);
            if cancellation.is_cancelled() {
                break TurnStopReason::Cancelled;
            }
        };

        if !finalized {
            let lifecycle = match stop_reason {
                TurnStopReason::Complete
                | TurnStopReason::MaxSteps
                | TurnStopReason::TokenLimit
                | TurnStopReason::PriceLimit => LifecycleState::Completed,
                TurnStopReason::Cancelled => LifecycleState::Cancelled,
                TurnStopReason::Failed
                | TurnStopReason::Refusal
                | TurnStopReason::ResponseLength => LifecycleState::Failed,
            };
            emit(
                self.observer.as_ref(),
                &mut reducer,
                &mut events,
                &mut next_event_id,
                EngineEvent::Lifecycle {
                    state: lifecycle,
                    message: Some(stop_message(&stop_reason).to_owned()),
                },
            )?;
            finalized = true;
        }
        if finalized {
            persist_owned(&self.sink, &messages, reducer.state()).await?;
            persist_stats_owned(
                &self.sink,
                &SessionStats {
                    usage: usage.clone(),
                    context_tokens,
                    steps,
                },
            )
            .await?;
        }
        Ok(TurnOutcome {
            session_id: reducer.state().session_id.clone(),
            events,
            snapshot: reducer.state().clone(),
            messages,
            usage,
            context_tokens,
            price_micros,
            steps,
            checkpoints,
            stop_reason,
        })
    }
}

async fn persist_owned(
    sink: &impl TranscriptSink,
    messages: &[ModelMessage],
    snapshot: &ProjectionSnapshot,
) -> Result<(), EngineError> {
    sink.persist(messages, snapshot)
        .await
        .map_err(EngineError::Persistence)
}

async fn persist_stats_owned(
    sink: &impl TranscriptSink,
    stats: &SessionStats,
) -> Result<(), EngineError> {
    sink.persist_stats(stats)
        .await
        .map_err(EngineError::Persistence)
}

fn priced_tokens(tokens: u64, price_per_million_micros: u64) -> u64 {
    const MILLION: u64 = 1_000_000;
    tokens
        .checked_div(MILLION)
        .unwrap_or_default()
        .saturating_mul(price_per_million_micros)
        .saturating_add(
            tokens
                .checked_rem(MILLION)
                .unwrap_or_default()
                .saturating_mul(price_per_million_micros)
                .checked_div(MILLION)
                .unwrap_or_default(),
        )
}

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn emit(
    observer: &dyn EventObserver,
    reducer: &mut ProjectionReducer,
    events: &mut Vec<EventEnvelope>,
    next_event_id: &mut u64,
    event: EngineEvent,
) -> Result<(), EngineError> {
    let envelope = EventEnvelope {
        session_id: reducer.state().session_id.clone(),
        turn_id: reducer.state().turn_id.clone(),
        emitted_at: current_time_millis(),
        event_id: *next_event_id,
        event,
    };
    reducer.apply(&envelope)?;
    observer
        .observe(&envelope)
        .map_err(EngineError::Observation)?;
    events.push(envelope);
    *next_event_id = next_event_id.saturating_add(1);
    Ok(())
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

fn bounded_tool_error(message: &str) -> String {
    const MAX_TOOL_ERROR_BYTES: usize = 16_384;
    if message.len() <= MAX_TOOL_ERROR_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_TOOL_ERROR_BYTES;
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &message[..end])
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
    #[error(transparent)]
    Storage(#[from] StorageError),
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
