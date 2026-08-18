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

use crate::compaction::{CompactionFailure, CompactionFailureReason, CompactionStatus};
use crate::events::{
    EngineEvent, EventEnvelope, LifecycleState, ModelMessage, ModelToolCall, ProjectionError,
    ProjectionSnapshot, SessionHandoffCause,
};
use crate::middleware::{
    AutoCompactMiddleware, CompactionSettings, ConversationContext, ConversationMiddleware,
    MiddlewareAction, MiddlewarePipeline, ResetReason,
};
use crate::provider::{
    AssistantMessage, ModelCallDescriptor, ProviderChunk, ProviderError, ProviderInput,
    ProviderStream, TransportError, Usage, aggregate_provider_chunks,
};
use crate::text::bounded_utf8;
use crate::tools::{MAX_TOOL_ERROR_BYTES, ToolExecutionOutput};
use crate::tracing::{
    AgentSpan, BackendFailure, ModelCallSpan, ToolSpan, TracedError, agent_span, model_call_span,
    set_model_call_response_metadata, set_model_call_usage, set_tool_result, tool_span,
};

mod contracts;
mod ledger;

use contracts::ChannelRetrySink;
pub use contracts::{
    Compactor, CompletionProvider, CompositeEventObserver, EventObserver, NoTools,
    NoopEventObserver, NoopTranscriptSink, RejectCompaction, SessionTranscriptSink, ToolExecutor,
    TranscriptSink,
};
use ledger::{
    TurnLedger, TurnRecorder, current_time_millis, lifecycle_for, new_compaction_id, persist,
    persist_stats, stop_message, title_from_messages,
};

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AssistantMessage, ProviderError>> + Send + 'a>>;
pub type ProviderStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderStream, ProviderError>> + Send + 'a>>;
pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, String>> + Send + 'a>>;
pub type ToolStreamSink = Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;
pub type CompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionResult, CompactionFailure>> + Send + 'a>>;
pub type PersistenceFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Buffered tool output chunks awaiting projection.
const TOOL_STREAM_CAPACITY: usize = 256;
/// Stands in for a tool result the turn was cancelled before receiving.
const INTERRUPTED_TOOL_RESULT: &str = "Tool execution interrupted";
/// The profile a turn runs under when its caller names none. Reference
/// `BuiltinAgentName.DEFAULT`.
pub const DEFAULT_AGENT_PROFILE: &str = "default";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub usage: Usage,
    pub context_tokens: u64,
    pub steps: u32,
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
        /// Whether a steer naming a user-invocable skill appends the synthetic
        /// pair, which is the wire's `injectInvokedSkill` on `turn/steer`.
        inject_invoked_skill: bool,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionResult {
    pub new_session_id: String,
    pub summary: String,
    pub messages: Vec<ModelMessage>,
    /// What the summarization spent. The reference makes every compaction call
    /// through the same accounted path a turn's own requests go through, so the
    /// token and price ceilings an operator set cover them too.
    pub usage: Usage,
    /// The classified failure a compaction degraded from, when its summary is
    /// the placeholder. A compaction that produced a real summary carries none.
    pub failure: Option<CompactionFailureReason>,
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

/// What a turn runs under, independently of who provides, tools, compacts or
/// persists it.
///
/// The four collaborators are type parameters, so swapping one rebuilds the
/// engine's type. Keeping everything that is *not* a collaborator in one value
/// means [`ConversationEngine::with_tools`], [`ConversationEngine::with_compactor`]
/// and [`ConversationEngine::with_sink`] carry it across in one move, and a new
/// setting is declared once instead of in three rebuild sites.
struct TurnSettings {
    limits: EngineLimits,
    baseline: SessionStats,
    observer: Arc<dyn EventObserver>,
    /// Policies registered after the budget ones, which is where the reference
    /// registers automatic compaction and everything that follows it.
    middleware: Vec<Arc<dyn ConversationMiddleware>>,
    compaction: CompactionSettings,
    /// How a slash-invoked skill resolves, when the session publishes one.
    /// Absent, a `/name` prompt is an ordinary message.
    invoked_skills: Option<Arc<dyn crate::skills::InvokedSkillResolver>>,
    /// The agent profile this turn runs under, which every request and tool
    /// event reports. Reference `self.agent_profile.name`, whose default
    /// profile is named `default`.
    agent_profile: String,
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            limits: EngineLimits::default(),
            baseline: SessionStats::default(),
            observer: Arc::new(NoopEventObserver),
            middleware: Vec::new(),
            compaction: CompactionSettings::default(),
            invoked_skills: None,
            agent_profile: DEFAULT_AGENT_PROFILE.to_owned(),
        }
    }
}

pub struct ConversationEngine<P, T = NoTools, C = RejectCompaction, S = NoopTranscriptSink> {
    provider: P,
    tools: T,
    compactor: C,
    sink: S,
    settings: TurnSettings,
}

impl<P> ConversationEngine<P> {
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            tools: NoTools,
            compactor: RejectCompaction,
            sink: NoopTranscriptSink,
            settings: TurnSettings::default(),
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
            settings: self.settings,
        }
    }

    #[must_use]
    pub fn with_compactor<C2>(self, compactor: C2) -> ConversationEngine<P, T, C2, S> {
        ConversationEngine {
            provider: self.provider,
            tools: self.tools,
            compactor,
            sink: self.sink,
            settings: self.settings,
        }
    }

    #[must_use]
    pub fn with_sink<S2>(self, sink: S2) -> ConversationEngine<P, T, C, S2> {
        ConversationEngine {
            provider: self.provider,
            tools: self.tools,
            compactor: self.compactor,
            sink,
            settings: self.settings,
        }
    }

    #[must_use]
    pub fn with_limits(mut self, limits: EngineLimits) -> Self {
        self.settings.limits = limits;
        self
    }

    /// Registers one conversation policy after the budget policies.
    #[must_use]
    pub fn with_middleware(mut self, middleware: Arc<dyn ConversationMiddleware>) -> Self {
        self.settings.middleware.push(middleware);
        self
    }

    #[must_use]
    pub fn with_compaction_settings(mut self, compaction: CompactionSettings) -> Self {
        self.settings.compaction = compaction;
        self
    }

    #[must_use]
    pub fn with_baseline(mut self, baseline: SessionStats) -> Self {
        self.settings.baseline = baseline;
        self
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.settings.observer = observer;
        self
    }

    #[must_use]
    pub fn with_invoked_skills(
        mut self,
        resolver: Arc<dyn crate::skills::InvokedSkillResolver>,
    ) -> Self {
        self.settings.invoked_skills = Some(resolver);
        self
    }

    /// Names the agent profile this turn runs under. Absent, the turn reports
    /// the reference's default profile.
    #[must_use]
    pub fn with_agent_profile(mut self, profile: impl Into<String>) -> Self {
        self.settings.agent_profile = profile.into();
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

    /// Runs one turn under the span the whole trace hangs off. Reference
    /// `_loop.py` opens `agent_span` around its run loop, which is what puts
    /// every model call, tool call and hook of the turn under one parent and
    /// publishes the conversation id the descendants read back.
    pub async fn run_turn_controlled(
        &self,
        session_id: impl Into<String>,
        input: ProviderInput,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
    ) -> Result<TurnOutcome, EngineError> {
        let session_id = session_id.into();
        let model = self.resolved_model(&input);
        agent_span(
            AgentSpan {
                model: model.as_deref(),
                session_id: Some(&session_id),
            },
            self.run_traced_turn(
                session_id.clone(),
                input,
                prompt.into(),
                cancellation,
                controls,
            ),
        )
        .await
    }

    async fn run_traced_turn(
        &self,
        session_id: String,
        mut input: ProviderInput,
        prompt: String,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
    ) -> Result<TurnOutcome, EngineError> {
        let mut recorder = TurnRecorder::new(
            self.settings.observer.as_ref(),
            session_id,
            input.turn_id.as_deref(),
        );
        let mut messages = input.messages.clone();
        let mut ledger = TurnLedger::new(&self.settings.baseline, &self.settings.limits);
        let mut checkpoints = 0_u32;
        // Reference `_reactive_recovery_used`, reset at the top of every run:
        // an overflow is recovered from at most once per user turn, so a
        // transcript that still overflows after a compaction is reported
        // instead of compacting again.
        let mut reactive_recovery_used = false;
        // The budget policies answer twice per cycle: once at the top, where
        // they are part of the full pipeline, and once mid-cycle, where a
        // reached limit must not leave a tool call unanswered. Both readings
        // come from the same middlewares, so there is one budget authority.
        let budget = MiddlewarePipeline::from_limits(&self.settings.limits);
        let mut pipeline = budget.clone();
        // Automatic compaction is registered after the budget policies and
        // before anything a caller added, which is the reference's order in
        // `_setup_middleware`: a cycle that reaches a limit and the threshold at
        // once stops instead of compacting.
        pipeline.add(Arc::new(AutoCompactMiddleware));
        for middleware in &self.settings.middleware {
            pipeline.add(Arc::clone(middleware));
        }
        pipeline.reset(ResetReason::Stop);

        recorder.emit(EngineEvent::UserMessage {
            content: prompt.clone(),
        })?;
        // Reference `_current_user_message_id`: every request and tool event of
        // this turn reports the operator's message, as the projection published
        // it.
        let message_id = recorder.last_entry_id();
        messages.push(ModelMessage::user(prompt.clone()));
        self.inject_invoked_skill(&mut recorder, &mut messages, &prompt)?;
        recorder.emit(EngineEvent::Title {
            title: title_from_messages(&messages),
        })?;
        persist(&self.sink, &messages, recorder.state()).await?;
        checkpoints = checkpoints.saturating_add(1);

        let stop_reason = loop {
            // Cancellation answers before any policy does: a cancelled turn has
            // no budget question left to ask.
            if cancellation.is_cancelled() {
                break TurnStopReason::Cancelled;
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
            let policy = pipeline.before_turn(&ConversationContext {
                messages: &messages,
                stats: &ledger.session_stats(),
                price_micros: ledger.price_micros,
                compaction: &self.settings.compaction,
            });
            match policy.action {
                // A policy that stops without naming a public status ends the
                // turn the way a finished conversation does.
                MiddlewareAction::Stop => {
                    break policy.stop_reason.unwrap_or(TurnStopReason::Complete);
                }
                MiddlewareAction::Compact => {
                    match self
                        .compact(
                            &mut recorder,
                            &mut messages,
                            &pipeline,
                            &mut ledger,
                            &cancellation,
                        )
                        .await?
                    {
                        // The reference does not re-enter the pipeline after a
                        // compaction: the policy that asked for it would read
                        // the same context size and ask again.
                        Some(()) => {
                            persist(&self.sink, &messages, recorder.state()).await?;
                            checkpoints = checkpoints.saturating_add(1);
                        }
                        None => break TurnStopReason::Cancelled,
                    }
                }
                MiddlewareAction::InjectMessage => {
                    if let Some(content) = policy.message {
                        recorder.emit(EngineEvent::ContextInjected {
                            content: content.clone(),
                            as_message: false,
                        })?;
                        messages.push(ModelMessage::injected_user(content));
                    }
                }
                MiddlewareAction::Continue => {}
            }
            input.messages.clone_from(&messages);
            self.record_request(&mut recorder, &input, &prompt, message_id.clone())?;
            let completion = match self
                .stream_completion(&mut recorder, &input, &cancellation)
                .await?
            {
                StreamOutcome::Cancelled => break TurnStopReason::Cancelled,
                StreamOutcome::Completed(Ok(completion)) => *completion,
                StreamOutcome::Completed(Err(ProviderError::ContextOverflow)) => {
                    // Reference `_should_self_heal`: one recovery per turn, and
                    // none at all in strict mode, where the operator asked for
                    // the overflow rather than for a silent repair.
                    if reactive_recovery_used
                        || self.settings.compaction.raise_on_compaction_failure
                    {
                        recorder.emit(EngineEvent::Lifecycle {
                            state: LifecycleState::Failed,
                            message: Some(ProviderError::ContextOverflow.to_string()),
                        })?;
                        persist(&self.sink, &messages, recorder.state()).await?;
                        return Err(EngineError::Provider(ProviderError::ContextOverflow));
                    }
                    reactive_recovery_used = true;
                    match self
                        .compact(
                            &mut recorder,
                            &mut messages,
                            &pipeline,
                            &mut ledger,
                            &cancellation,
                        )
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

            ledger.record_completion(&completion.usage, &self.settings.limits);
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
            if let Some(reason) = self.exhausted_budget(&budget, &messages, &ledger, &cancellation)
            {
                if completion.tool_calls.is_empty() {
                    messages.push(assistant_message);
                }
                break reason;
            }
            if completion.text.len() > self.settings.limits.max_response_bytes {
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
        persist_stats(&self.sink, &ledger.session_stats()).await?;
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

    /// The model this request addresses.
    ///
    /// A turn's own override wins, and the provider answers for the session's
    /// active model otherwise. The trace span, the request event and the model
    /// call span all report this one answer, so a client never sees three
    /// spellings of the same request.
    fn resolved_model(&self, input: &ProviderInput) -> Option<String> {
        input
            .model_override
            .clone()
            .or_else(|| self.provider.model().map(ToOwned::to_owned))
    }

    /// Reports the limit that ends the turn, if any is already reached.
    ///
    /// The answer comes from the budget middlewares rather than from a second
    /// copy of their arithmetic, so mid-cycle and top-of-cycle can never
    /// disagree about whether a limit is reached.
    fn exhausted_budget(
        &self,
        budget: &MiddlewarePipeline,
        messages: &[ModelMessage],
        ledger: &TurnLedger,
        cancellation: &CancellationToken,
    ) -> Option<TurnStopReason> {
        if cancellation.is_cancelled() {
            return Some(TurnStopReason::Cancelled);
        }
        let result = budget.before_turn(&ConversationContext {
            messages,
            stats: &ledger.session_stats(),
            price_micros: ledger.price_micros,
            compaction: &self.settings.compaction,
        });
        match result.action {
            MiddlewareAction::Stop => result.stop_reason,
            _ => None,
        }
    }

    /// Appends the synthetic `skill` call pair when the message is a slash
    /// invocation, reproducing reference `_inject_invoked_skill`: the model
    /// reads the same conversation whether the operator or one of its own tool
    /// calls loaded the skill, and a repeat is acknowledged rather than
    /// rendered again.
    fn inject_invoked_skill(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        content: &str,
    ) -> Result<(), EngineError> {
        let Some(resolver) = &self.settings.invoked_skills else {
            return Ok(());
        };
        let Some(invoked) = resolver.resolve(content) else {
            return Ok(());
        };
        let appended = crate::skills::append_invoked_skill(messages, &invoked);
        recorder.emit(EngineEvent::ToolCall {
            call_id: appended.call_id.clone(),
            name: "skill".to_owned(),
            arguments: appended.arguments,
        })?;
        recorder.emit(EngineEvent::ToolResult {
            call_id: appended.call_id,
            content: appended.output.model_text,
            typed_result: appended.output.typed_result,
            display: appended.output.display,
            duration_ms: 0,
            is_error: false,
            cancelled: false,
        })?;
        Ok(())
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
                TurnControl::Steer {
                    content,
                    inject_invoked_skill,
                } => {
                    recorder.emit(EngineEvent::UserSteer {
                        content: content.clone(),
                    })?;
                    messages.push(ModelMessage::user(content.clone()));
                    if inject_invoked_skill {
                        self.inject_invoked_skill(recorder, messages, &content)?;
                    }
                }
                TurnControl::InjectContext {
                    content,
                    as_message,
                } => {
                    recorder.emit(EngineEvent::ContextInjected {
                        content: content.clone(),
                        as_message,
                    })?;
                    // The reference appends a real user turn when the caller
                    // asks for one and marks the message injected otherwise
                    // (`vibe/core/agent_loop/_loop.py:1104`), which is what
                    // decides whether a compaction preserves it.
                    messages.push(if as_message {
                        ModelMessage::user(content)
                    } else {
                        ModelMessage::injected_user(content)
                    });
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
                    messages.push(ModelMessage::user(continuation));
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
    /// Reports the request the turn is about to make, which is what the
    /// reference's agent loop hands its telemetry client one call earlier.
    fn record_request(
        &self,
        recorder: &mut TurnRecorder<'_>,
        input: &ProviderInput,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<(), EngineError> {
        let model = self.resolved_model(input).unwrap_or_default();
        recorder.emit(EngineEvent::RequestSent {
            model,
            agent_profile: self.settings.agent_profile.clone(),
            nb_context_chars: input
                .messages
                .iter()
                .map(|message| message.content().chars().count() as u64)
                .sum(),
            nb_context_messages: input.messages.len() as u64,
            nb_prompt_chars: prompt.chars().count() as u64,
            nb_images: input.images.len() as u64,
            // Every provider this port ships accepts images on the request it
            // builds; a backend that refuses one refuses it at the wire, which
            // is not a telemetry decision.
            supports_images: true,
            message_id,
        })
    }

    /// Streams one completion under the span that reports it. Reference opens
    /// `model_call_span` inside its backend; here the turn opens it and the
    /// backend fills in the HTTP status from inside, which is the same span
    /// either way because the request is polled under it.
    async fn stream_completion(
        &self,
        recorder: &mut TurnRecorder<'_>,
        input: &ProviderInput,
        cancellation: &CancellationToken,
    ) -> Result<StreamOutcome, EngineError> {
        let descriptor = self
            .provider
            .call_descriptor()
            .unwrap_or(ModelCallDescriptor {
                provider_name: String::new(),
                api_style: String::new(),
                endpoint: String::new(),
            });
        let model = self.resolved_model(input).unwrap_or_default();
        let outcome = model_call_span(
            ModelCallSpan {
                provider_name: &descriptor.provider_name,
                provider_api_style: &descriptor.api_style,
                model: &model,
                streaming: input.stream,
                temperature: input
                    .limits
                    .temperature_millis
                    .map(|thousandths| f64::from(thousandths) / 1000.0),
                max_tokens: Some(i64::from(input.limits.max_tokens)),
                session_id: None,
                call_type: input.metadata.get("call_type").map(String::as_str),
                message_id: input.metadata.get("message_id").map(String::as_str),
                http_method: None,
                http_url: Some(&descriptor.endpoint),
            },
            async {
                match self
                    .stream_traced_completion(recorder, input, cancellation, &model)
                    .await
                {
                    // A backend that refused is a failing span and an answered
                    // turn: the outcome the caller reads is unchanged, and the
                    // span carries the refusal it would otherwise never see.
                    Ok(StreamOutcome::Completed(Err(error))) => {
                        Err(ModelCallFailure::Provider(error))
                    }
                    Ok(outcome) => Ok(outcome),
                    Err(error) => Err(ModelCallFailure::Engine(error)),
                }
            },
        )
        .await;
        match outcome {
            Ok(outcome) => Ok(outcome),
            Err(ModelCallFailure::Provider(error)) => Ok(StreamOutcome::Completed(Err(error))),
            Err(ModelCallFailure::Engine(error)) => Err(error),
        }
    }

    async fn stream_traced_completion(
        &self,
        recorder: &mut TurnRecorder<'_>,
        input: &ProviderInput,
        cancellation: &CancellationToken,
        model: &str,
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
        let aggregated = aggregate_provider_chunks(chunks, stream.correlation_id);
        if let Ok(message) = &aggregated {
            // Reference `set_model_call_usage` and
            // `set_model_call_response_metadata`, both read off the answered
            // response rather than off the request.
            set_model_call_usage(message.usage.input_tokens, message.usage.output_tokens);
            set_model_call_response_metadata(&serde_json::json!({
                "model": model,
                "id": message.correlation_id,
                "choices": [{"finish_reason": message.stop_reason}],
            }));
        }
        Ok(StreamOutcome::Completed(aggregated.map(Box::new)))
    }

    /// Compacts the transcript, whether a policy asked for it or an overflow
    /// forced it.
    ///
    /// Both entries clear the pipeline's latched state here rather than at
    /// their own call site, because a policy measured against the transcript is
    /// stale either way and the reference resets from the one function both
    /// paths share.
    ///
    /// `None` means the turn was cancelled while compaction was running.
    ///
    /// The boundary events bracket the model call the way the reference's
    /// `_run_compaction` does: the start event is emitted before the compactor
    /// is asked for anything, the completed event only once there is a summary
    /// and a new identifier, and the outcome event on every path, because a
    /// compaction that failed or was cancelled still reports its status.
    async fn compact(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        pipeline: &MiddlewarePipeline,
        ledger: &mut TurnLedger,
        cancellation: &CancellationToken,
    ) -> Result<Option<()>, EngineError> {
        let context_tokens = ledger.context_tokens;
        let compaction_id = new_compaction_id();
        let threshold = self.settings.compaction.auto_compact_threshold;
        let old_session_id = recorder.state().session_id.clone();
        recorder.emit(EngineEvent::CompactionStarted {
            compaction_id: compaction_id.clone(),
            current_context_tokens: context_tokens,
            threshold,
        })?;
        let outcome = |status, reason| EngineEvent::CompactionOutcome {
            compaction_id: compaction_id.clone(),
            status,
            context_tokens_before: context_tokens,
            threshold,
            reason,
        };
        let compaction = tokio::select! {
            result = self.compactor.compact(&old_session_id, messages) => result,
            () = cancellation.cancelled() => {
                recorder.emit(outcome(CompactionStatus::Cancelled, None))?;
                return Ok(None);
            }
        };
        let compaction = match compaction {
            Ok(compaction) => compaction,
            Err(failure) => {
                // A failed compaction still made the calls it made, and the
                // transcript it read is left exactly as it was.
                ledger.record_compaction(&failure.usage, &self.settings.limits, false);
                recorder.emit(outcome(CompactionStatus::Failure, failure.reason))?;
                return Err(EngineError::Compaction(failure.message));
            }
        };
        ledger.record_compaction(&compaction.usage, &self.settings.limits, true);
        let summary_length = u64::try_from(compaction.summary.chars().count()).unwrap_or(u64::MAX);
        let new_session_id = compaction.new_session_id;
        // The completed event precedes the handoff because the handoff is what
        // raises `session/compacted` here, and the reference raises that
        // notification from the end event itself: the summary length has to be
        // known by the time the session rotates.
        recorder.emit(EngineEvent::CompactionCompleted {
            compaction_id: compaction_id.clone(),
            summary_length,
            old_session_id: old_session_id.clone(),
            new_session_id: new_session_id.clone(),
        })?;
        recorder.emit(EngineEvent::SessionHandoff {
            from_session_id: old_session_id,
            to_session_id: new_session_id,
            cause: SessionHandoffCause::Compaction,
        })?;
        // A compaction that degraded to the placeholder still succeeded: the
        // conversation is compacted. It reports the reason it degraded from, so
        // the failure record the reference sends alongside the success is
        // written here too.
        recorder.emit(outcome(CompactionStatus::Success, compaction.failure))?;
        *messages = compaction.messages;
        pipeline.reset(ResetReason::Compact);
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
                    // Reference `_loop.py` opens `tool_span` around the
                    // execution itself, so a tool that streams for a minute is
                    // one span rather than a point in the parent's timeline.
                    let result = tool_span(
                        ToolSpan {
                            tool_name: &call.name,
                            call_id: &call.id,
                            arguments: &call.arguments,
                        },
                        async {
                            let result = self
                                .tools
                                .execute_stream(&call.name, &call.arguments, output)
                                .await;
                            if let Ok(output) = &result {
                                set_tool_result(&output.model_text);
                            }
                            result
                        },
                    )
                    .await;
                    (index, started, result)
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

/// What one model call can fail with, so the span reads a refusal as a failure
/// while the turn still reads it as an answer.
#[derive(Debug, Error)]
enum ModelCallFailure {
    #[error(transparent)]
    Provider(ProviderError),
    #[error(transparent)]
    Engine(EngineError),
}

impl TracedError for ModelCallFailure {
    fn error_type(&self) -> &'static str {
        match self {
            Self::Provider(error) => error.error_type(),
            Self::Engine(error) => error.error_type(),
        }
    }

    fn backend_failure(&self) -> Option<BackendFailure> {
        match self {
            Self::Provider(error) => error.backend_failure(),
            Self::Engine(error) => error.backend_failure(),
        }
    }
}

impl TracedError for EngineError {
    fn error_type(&self) -> &'static str {
        "EngineError"
    }

    /// Reference `_backend_error_from` walks the cause chain; the only cause a
    /// turn failure carries here is the provider error it wraps.
    fn backend_failure(&self) -> Option<BackendFailure> {
        match self {
            Self::Provider(error) => error.backend_failure(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use crate::events::ProjectionReducer;
    use crate::provider::RetrySink;
    use crate::storage::SessionStore;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;
    use crate::events::ModelToolCall;
    use crate::middleware::MiddlewareResult;
    use crate::provider::{ProviderChunk, RequestLimits};
    use serde_json::json;

    #[derive(Default)]
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Result<AssistantMessage, ProviderError>>>,
        /// Every request's transcript, so a test can prove what the model was
        /// actually shown rather than what the turn ended up holding.
        requests: Mutex<Vec<Vec<ModelMessage>>>,
    }

    impl ScriptedProvider {
        fn new(
            responses: impl IntoIterator<Item = Result<AssistantMessage, ProviderError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl CompletionProvider for ScriptedProvider {
        fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .map_err(|_| ProviderError::MalformedStream("fake lock poisoned".to_owned()))?
                    .push(input.messages.clone());
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
                Ok(ToolExecutionOutput::new(format!("{name}-result"))
                    .displayed_as(json!({"summary": name}))
                    .typed(json!({"tool": name})))
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
                    usage: Usage {
                        input_tokens: 30,
                        output_tokens: 5,
                    },
                    failure: None,
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
                std::future::pending::<Result<CompactionResult, CompactionFailure>>().await
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

    /// US-016: the turn is one agent span, every model call and tool call the
    /// turn made hangs off it, the usage the aggregation read reaches the model
    /// call span, and the conversation id the turn published reaches the tool
    /// call. Reference `_loop.py` opens `agent_span` around the run loop and
    /// `tool_span` around each execution.
    #[tokio::test]
    async fn a_turn_hangs_every_call_off_one_agent_span() {
        let _exclusive = crate::tracing::harness::exclusive();
        let harness = crate::tracing::harness::Harness::install();
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
        let mut input = provider_input();
        input.model_override = Some("model-1".to_owned());
        let outcome = ConversationEngine::new(provider)
            .with_tools(FakeTools)
            .run_turn("session-1", input, "hello", CancellationToken::default())
            .await
            .expect("turn completes");
        assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
        let drained = harness.drain();
        drop(harness);
        // The provider is global, so a test running beside this one exports
        // into the same collector. This turn is one trace, and the trace is
        // what tells its spans apart from everyone else's.
        let trace = drained
            .iter()
            .find(|span| {
                span.name == "invoke_agent mistral-vibe"
                    && span.attributes.iter().any(|attribute| {
                        attribute.key.as_str() == "gen_ai.request.model"
                            && attribute.value.as_str() == "model-1"
                    })
            })
            .map(|span| span.span_context.trace_id())
            .expect("the turn opened an agent span");
        let spans = drained
            .iter()
            .filter(|span| span.span_context.trace_id() == trace)
            .collect::<Vec<_>>();

        let agents = spans
            .iter()
            .filter(|span| span.name == "invoke_agent mistral-vibe")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(agents.len(), 1, "one turn is one agent span");
        let parent = agents[0].span_context.span_id();
        let attribute = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
            span.attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == key)
                .map(|attribute| attribute.value.to_string())
        };
        assert_eq!(
            attribute(agents[0], "gen_ai.request.model"),
            Some("model-1".to_owned())
        );

        let calls = spans
            .iter()
            .filter(|span| span.name.starts_with("chat "))
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "both cycles opened a model call span");
        for call in &calls {
            assert_eq!(
                call.parent_span_id, parent,
                "every model call is a descendant"
            );
            assert_eq!(
                attribute(call, "gen_ai.usage.input_tokens"),
                Some("2".to_owned()),
                "the usage the aggregation read reached the span it ran under"
            );
            assert_eq!(
                attribute(call, "gen_ai.response.finish_reasons"),
                Some("[\"stop\"]".to_owned())
            );
        }

        let tools = spans
            .iter()
            .filter(|span| span.name == "execute_tool read")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].parent_span_id, parent);
        assert_eq!(
            attribute(tools[0], "gen_ai.conversation.id"),
            Some("session-1".to_owned()),
            "the tool call read the conversation id out of the turn's baggage"
        );
        assert_eq!(
            attribute(tools[0], "gen_ai.tool.call.id"),
            Some("call-1".to_owned())
        );
    }

    /// US-016: a cancelled turn leaves no span open and closes the agent span
    /// the way the reference closes it. Reference `_conversation_loop` returns
    /// normally on a user cancellation (`_loop.py:1668`), so `_safe_span` reads
    /// no exception and sets `OK`; only a cancellation that unwinds the turn
    /// leaves the status unset, because `_safe_span` decides a status for an
    /// `Exception` and nothing else.
    #[tokio::test]
    async fn a_cancelled_turn_closes_its_agent_span() {
        let _exclusive = crate::tracing::harness::exclusive();
        let harness = crate::tracing::harness::Harness::install();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let outcome = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
            "unreached",
            Vec::new(),
        ))]))
        .run_turn("session-cancelled", provider_input(), "hello", cancellation)
        .await
        .expect("cancellation is an outcome");
        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        let spans = harness.drain();
        drop(harness);
        let agent = spans
            .iter()
            .filter(|span| {
                span.name == "invoke_agent mistral-vibe"
                    && span.attributes.iter().any(|attribute| {
                        attribute.key.as_str() == "gen_ai.conversation.id"
                            && attribute.value.as_str() == "session-cancelled"
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(agent.len(), 1, "the turn closed exactly one agent span");
        assert_eq!(agent[0].status, opentelemetry::trace::Status::Ok);
        let trace = agent[0].span_context.trace_id();
        let opened = spans
            .iter()
            .filter(|span| span.span_context.trace_id() == trace)
            .count();
        assert_eq!(
            opened, 1,
            "a cancelled turn opened no model call and no tool call"
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

    /// US-009: every model call reports the request it is about to make, with
    /// the model, the profile and the user message the events of that request
    /// carry. Reference `send_request_sent`'s call site in the agent loop.
    #[tokio::test]
    async fn every_model_call_reports_the_request_it_makes() {
        let provider = ScriptedProvider::new([
            Ok(completion(
                "",
                vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "first".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            )),
            Ok(completion("done", Vec::new())),
        ]);
        let observer = Arc::new(RecordingObserver::default());
        let recorded = Arc::clone(&observer);
        let mut input = provider_input();
        input.turn_id = Some("turn-1".to_owned());
        input.model_override = Some("oracle-model".to_owned());
        ConversationEngine::new(provider)
            .with_tools(FakeTools)
            .with_agent_profile("oracle-profile")
            .with_observer(observer)
            .run_turn("session-1", input, "hello", CancellationToken::default())
            .await
            .expect("the turn runs");

        let events = recorded.events.lock().expect("observer lock").clone();
        let requests = events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::RequestSent {
                    model,
                    agent_profile,
                    nb_context_messages,
                    nb_prompt_chars,
                    message_id,
                    ..
                } => Some((
                    model.clone(),
                    agent_profile.clone(),
                    *nb_context_messages,
                    *nb_prompt_chars,
                    message_id.clone(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            requests.len(),
            2,
            "one request is reported per model call, the tool answer included"
        );
        let (model, profile, messages, prompt_chars, message_id) = &requests[0];
        assert_eq!(model, "oracle-model");
        assert_eq!(profile, "oracle-profile");
        assert_eq!(*messages, 2, "the system prompt and the operator's turn");
        assert_eq!(*prompt_chars, 5);
        assert_eq!(
            message_id.as_deref(),
            Some("entry-turn-1-1"),
            "the identifier is the entry the operator's message was projected as"
        );
        assert!(
            requests[1].2 > *messages,
            "the second call carries the assistant turn and the tool answer too"
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

    /// US-155: the calls a compaction makes are credited to the turn, so the
    /// token and price ceilings an operator set cover every request the tool
    /// makes rather than only the ones the operator asked for. The compaction
    /// still spends no step, and the context size it zeroed is recomputed by the
    /// completion that follows it.
    #[tokio::test]
    async fn a_compaction_credits_its_own_calls_to_the_turn_ledger() {
        let provider = ScriptedProvider::new([
            Err(ProviderError::ContextOverflow),
            Ok(completion("after compact", Vec::new())),
        ]);
        let outcome = ConversationEngine::new(provider)
            .with_compactor(FakeCompactor)
            .with_limits(EngineLimits {
                input_price_per_million_micros: 1_000_000,
                output_price_per_million_micros: 2_000_000,
                ..EngineLimits::default()
            })
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("compacted turn completes");

        // The one completion spends 2 and 3; the compaction spends 30 and 5.
        assert_eq!(
            outcome.usage,
            Usage {
                input_tokens: 32,
                output_tokens: 8,
            },
            "the summarization is part of what the turn spent"
        );
        assert_eq!(
            outcome.price_micros,
            32 + 16,
            "the price ceiling is evaluated against a total that includes the compaction"
        );
        assert_eq!(outcome.steps, 1, "a compaction advances no step budget");
        assert_eq!(
            outcome.context_tokens, 5,
            "the compaction zeroed the context size and the completion recomputed it"
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
                ModelMessage::user("Plan approved. Switch to code mode.".to_owned()),
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
        // US-151: a cancelled compaction reports its status and publishes no
        // end event, so nothing renders a handoff that never happened.
        assert!(outcome.events.iter().any(|envelope| matches!(
            envelope.event,
            EngineEvent::CompactionOutcome {
                status: CompactionStatus::Cancelled,
                ..
            }
        )));
        assert!(!outcome.events.iter().any(|envelope| matches!(
            envelope.event,
            EngineEvent::CompactionCompleted { .. } | EngineEvent::SessionHandoff { .. }
        )));
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
        let first = ModelMessage::user("first".to_owned());
        let second = ModelMessage::user("second".to_owned());

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
                    ModelMessage::User { content, .. } => Some(content.as_str()),
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
            Some(ModelMessage::User { content, .. }) if content == "first"
        ));
    }

    /// US-158: the reference's `keep_parent` distinction, taken from the same
    /// persistence path both rotations use. A compacted session continues the
    /// one it summarized; a cleared session continues nothing, because what it
    /// would point at was discarded.
    #[tokio::test]
    async fn a_compaction_keeps_the_parent_a_clearing_does_not() {
        struct ClearingTools {
            controls: TurnControlHandle,
        }

        impl ToolExecutor for ClearingTools {
            fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
                Box::pin(async move {
                    self.controls
                        .send(TurnControl::ClearContext {
                            continuation: "start over".to_owned(),
                            plan_file_path: None,
                        })
                        .map_err(|error| error.to_string())?;
                    Ok(ToolExecutionOutput::text("cleared"))
                })
            }
        }

        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("parenting");

        // The threshold fires the compaction before the first request, so the
        // rotation the store sees is the compaction's.
        let compacted_metadata = store
            .create("session-compacted", "/workspace", None, 10)
            .expect("session creates");
        ConversationEngine::new(ScriptedProvider::new([Ok(completion("done", Vec::new()))]))
            .with_compactor(FakeCompactor)
            .with_sink(SessionTranscriptSink::new(
                store.clone(),
                compacted_metadata,
            ))
            .with_baseline(SessionStats {
                context_tokens: 1_000,
                ..SessionStats::default()
            })
            .with_compaction_settings(CompactionSettings {
                auto_compact_threshold: 100,
                ..CompactionSettings::default()
            })
            .run_turn(
                "session-compacted",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("compacted turn completes");
        assert_eq!(
            store
                .load("session-2")
                .expect("the compacted session is durable")
                .metadata
                .parent_session_id
                .as_deref(),
            Some("session-compacted"),
        );

        let cleared_metadata = store
            .create("session-cleared", "/workspace", None, 20)
            .expect("session creates");
        let controls = TurnControlHandle::default();
        ConversationEngine::new(ScriptedProvider::new([
            Ok(completion(
                "",
                vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "exit_plan_mode".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            )),
            Ok(completion("restarted", Vec::new())),
        ]))
        .with_tools(ClearingTools {
            controls: controls.clone(),
        })
        .with_compactor(FakeCompactor)
        .with_sink(SessionTranscriptSink::new(store.clone(), cleared_metadata))
        .run_turn_controlled(
            "session-cleared",
            provider_input(),
            "write a plan",
            CancellationToken::default(),
            controls,
        )
        .await
        .expect("cleared turn completes");
        assert_eq!(
            store
                .load("session-cleared-cleared")
                .expect("the cleared session is durable")
                .metadata
                .parent_session_id,
            None,
            "a clearing retains no parent, matching `_reset_session(keep_parent=False)`"
        );
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

    /// A policy whose answer is spent on the first poll, so a turn that would
    /// otherwise ask for the same thing on every cycle terminates.
    struct OnceThen {
        first: Mutex<Option<MiddlewareResult>>,
        resets: Mutex<Vec<ResetReason>>,
    }

    impl OnceThen {
        fn new(first: MiddlewareResult) -> Arc<Self> {
            Arc::new(Self {
                first: Mutex::new(Some(first)),
                resets: Mutex::new(Vec::new()),
            })
        }

        fn resets(&self) -> Vec<ResetReason> {
            self.resets
                .lock()
                .expect("reset log is not poisoned")
                .clone()
        }
    }

    impl ConversationMiddleware for OnceThen {
        fn before_turn(&self, _context: &ConversationContext<'_>) -> MiddlewareResult {
            self.first
                .lock()
                .expect("policy state is not poisoned")
                .take()
                .unwrap_or_default()
        }

        fn reset(&self, reset_reason: ResetReason) {
            self.resets
                .lock()
                .expect("reset log is not poisoned")
                .push(reset_reason);
        }
    }

    #[tokio::test]
    async fn a_policy_stop_ends_the_turn_before_any_request_is_built() {
        // The scripted provider holds no response, so reaching it at all would
        // fail the turn rather than stop it.
        let outcome = ConversationEngine::new(ScriptedProvider::default())
            .with_middleware(OnceThen::new(MiddlewareResult::stop(
                "policy said so",
                TurnStopReason::PriceLimit,
            )))
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("a policy stop is an outcome, not an error");

        assert_eq!(outcome.stop_reason, TurnStopReason::PriceLimit);
        assert_eq!(outcome.steps, 0);
        assert!(
            !outcome
                .events
                .iter()
                .any(|envelope| matches!(envelope.event, EngineEvent::ModelText { .. })),
            "no request was built, so no model text was produced"
        );
    }

    #[tokio::test]
    async fn cancellation_answers_before_any_policy_does() {
        let policy = OnceThen::new(MiddlewareResult::stop(
            "policy said so",
            TurnStopReason::PriceLimit,
        ));
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let outcome = ConversationEngine::new(ScriptedProvider::default())
            .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>)
            .run_turn("session-1", provider_input(), "hello", cancellation)
            .await
            .expect("a cancelled turn is an outcome");

        assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
        assert!(
            policy
                .first
                .lock()
                .expect("policy state is not poisoned")
                .is_some(),
            "the policy was never polled"
        );
    }

    #[tokio::test]
    async fn an_injected_policy_message_rides_the_next_request() {
        let engine = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
            "answered",
            Vec::new(),
        ))]))
        .with_middleware(OnceThen::new(MiddlewareResult::inject("half the window")));
        let outcome = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("the turn runs");

        assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
        assert!(
            outcome.events.iter().any(|envelope| matches!(
                &envelope.event,
                EngineEvent::ContextInjected { content, as_message }
                    if content == "half the window" && !*as_message
            )),
            "the injection is marked on the wire rather than told as a user turn"
        );
        let requested = engine
            .provider
            .requests
            .lock()
            .expect("request log is not poisoned")
            .clone();
        assert_eq!(requested.len(), 1);
        assert!(
            requested[0].iter().any(|message| matches!(
                message,
                ModelMessage::User { content, .. } if content == "half the window"
            )),
            "the request that follows the injection carries it: {requested:?}"
        );
    }

    #[tokio::test]
    async fn a_policy_compact_runs_before_any_request_and_resets_the_pipeline() {
        let policy = OnceThen::new(MiddlewareResult::compact());
        let engine = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
            "answered",
            Vec::new(),
        ))]))
        .with_compactor(FakeCompactor)
        .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>);
        let outcome = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("the turn runs");

        assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
        let compaction_index = outcome
            .events
            .iter()
            .position(|envelope| matches!(envelope.event, EngineEvent::CompactionStarted { .. }))
            .expect("the compaction path was entered");
        let first_text = outcome
            .events
            .iter()
            .position(|envelope| matches!(envelope.event, EngineEvent::ModelText { .. }))
            .expect("a request was built afterward");
        assert!(
            compaction_index < first_text,
            "compaction happens before the request it makes room for"
        );
        let requested = engine
            .provider
            .requests
            .lock()
            .expect("request log is not poisoned")
            .clone();
        assert_eq!(
            requested,
            vec![vec![ModelMessage::System {
                content: "summary".to_owned(),
            }]],
            "the only request was built from the compacted transcript"
        );
        assert_eq!(
            policy.resets(),
            vec![ResetReason::Stop, ResetReason::Compact],
            "the turn opened with a stop reset and the compaction added its own"
        );
    }

    #[tokio::test]
    async fn a_reactive_compaction_resets_the_pipeline_too() {
        // An overflow recovery replaces the transcript exactly like a policy
        // compaction does, so anything a policy latched against the old one is
        // just as stale.
        let policy = OnceThen::new(MiddlewareResult::default());
        let engine = ConversationEngine::new(ScriptedProvider::new([
            Err(ProviderError::ContextOverflow),
            Ok(completion("answered", Vec::new())),
        ]))
        .with_compactor(FakeCompactor)
        .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>);
        let outcome = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("the turn recovers");

        assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
        assert_eq!(
            policy.resets(),
            vec![ResetReason::Stop, ResetReason::Compact]
        );
    }

    #[tokio::test]
    async fn every_user_turn_opens_with_a_stop_reset() {
        let policy = OnceThen::new(MiddlewareResult::default());
        let engine = ConversationEngine::new(ScriptedProvider::new([
            Ok(completion("first", Vec::new())),
            Ok(completion("second", Vec::new())),
        ]))
        .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>);

        for prompt in ["hello", "again"] {
            engine
                .run_turn(
                    "session-1",
                    provider_input(),
                    prompt,
                    CancellationToken::default(),
                )
                .await
                .expect("the turn runs");
        }

        assert_eq!(policy.resets(), vec![ResetReason::Stop, ResetReason::Stop]);
    }

    /// US-149: the threshold fires before the request is built, from the
    /// context size the ledger carries into the turn.
    #[tokio::test]
    async fn the_threshold_compacts_before_the_first_request() {
        let engine = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
            "after compact",
            Vec::new(),
        ))]))
        .with_compactor(FakeCompactor)
        .with_baseline(SessionStats {
            context_tokens: 150_000,
            ..SessionStats::default()
        })
        .with_compaction_settings(CompactionSettings {
            auto_compact_threshold: 120_000,
            ..CompactionSettings::default()
        });
        let outcome = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("the turn runs");

        assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
        assert_eq!(
            outcome.session_id, "session-2",
            "the threshold compaction rotated the session"
        );
        assert_eq!(
            engine
                .provider
                .requests
                .lock()
                .expect("request log is not poisoned")
                .clone(),
            vec![vec![ModelMessage::System {
                content: "summary".to_owned(),
            }]],
            "the only request was built from the compacted transcript, so no \
             provider overflow was ever reached"
        );
    }

    /// US-149: below the threshold nothing compacts, which is what makes the
    /// test above a threshold test rather than a compactor test.
    #[tokio::test]
    async fn a_context_below_the_threshold_builds_its_request_untouched() {
        let outcome = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
            "answered",
            Vec::new(),
        ))]))
        .with_compactor(FakeCompactor)
        .with_baseline(SessionStats {
            context_tokens: 119_999,
            ..SessionStats::default()
        })
        .with_compaction_settings(CompactionSettings {
            auto_compact_threshold: 120_000,
            ..CompactionSettings::default()
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn runs");

        assert_eq!(outcome.session_id, "session-1");
        assert!(
            !outcome
                .events
                .iter()
                .any(|event| matches!(event.event, EngineEvent::CompactionStarted { .. }))
        );
    }

    /// US-149: the value the policy reads is the one `record_completion` wrote,
    /// which is the reference's `context_tokens`: the last completion's prompt
    /// and completion tokens, not the session totals.
    #[tokio::test]
    async fn the_threshold_reads_the_last_completion_usage() {
        let engine = ConversationEngine::new(ScriptedProvider::new([
            Ok(completion(
                "",
                vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            )),
            Ok(completion("done", Vec::new())),
        ]))
        .with_tools(FakeTools)
        .with_compactor(FakeCompactor)
        // Two completions of 5 tokens each spend 10 in total, and the context
        // is 5: a threshold of 6 must not fire.
        .with_compaction_settings(CompactionSettings {
            auto_compact_threshold: 6,
            ..CompactionSettings::default()
        });
        let outcome = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("the turn runs");

        assert_eq!(
            outcome.context_tokens, 5,
            "input plus output of the last completion"
        );
        assert_eq!(outcome.usage.input_tokens + outcome.usage.output_tokens, 10);
        assert!(
            !outcome
                .events
                .iter()
                .any(|event| matches!(event.event, EngineEvent::CompactionStarted { .. })),
            "the policy compares the context size, not the session total"
        );
    }

    /// US-150: an overflow is recovered from once per turn. A second one is
    /// reported instead of compacting again.
    #[tokio::test]
    async fn a_second_overflow_in_one_turn_is_reported_rather_than_compacted() {
        let engine = ConversationEngine::new(ScriptedProvider::new([
            Err(ProviderError::ContextOverflow),
            Err(ProviderError::ContextOverflow),
            Ok(completion("never reached", Vec::new())),
        ]))
        .with_compactor(FakeCompactor);
        let error = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect_err("the second overflow is not recovered from");
        assert!(
            matches!(error, EngineError::Provider(ProviderError::ContextOverflow)),
            "{error}"
        );
        assert_eq!(
            engine
                .provider
                .requests
                .lock()
                .expect("request log is not poisoned")
                .len(),
            2,
            "the turn stopped after the second overflow rather than retrying"
        );
    }

    /// US-150: the guard is per turn, so the next user turn may recover again.
    #[tokio::test]
    async fn a_new_turn_may_recover_from_an_overflow_again() {
        let engine = ConversationEngine::new(ScriptedProvider::new([
            Err(ProviderError::ContextOverflow),
            Ok(completion("first", Vec::new())),
            Err(ProviderError::ContextOverflow),
            Ok(completion("second", Vec::new())),
        ]))
        .with_compactor(FakeCompactor);
        for prompt in ["hello", "again"] {
            engine
                .run_turn(
                    "session-1",
                    provider_input(),
                    prompt,
                    CancellationToken::default(),
                )
                .await
                .unwrap_or_else(|error| panic!("the `{prompt}` turn recovers: {error}"));
        }
    }

    /// US-150: strict mode refuses the recovery outright, which is what
    /// `_should_self_heal` reads `raise_on_compaction_failure` for.
    #[tokio::test]
    async fn strict_mode_reports_an_overflow_without_recovering() {
        let engine = ConversationEngine::new(ScriptedProvider::new([
            Err(ProviderError::ContextOverflow),
            Ok(completion("never reached", Vec::new())),
        ]))
        .with_compactor(FakeCompactor)
        .with_compaction_settings(CompactionSettings {
            raise_on_compaction_failure: true,
            ..CompactionSettings::default()
        });
        let error = engine
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect_err("strict mode surfaces the overflow");
        assert!(
            matches!(error, EngineError::Provider(ProviderError::ContextOverflow)),
            "{error}"
        );
        assert_eq!(
            engine
                .provider
                .requests
                .lock()
                .expect("request log is not poisoned")
                .len(),
            1,
            "no compaction and no retry was attempted"
        );
    }

    /// US-151: the boundary pair brackets the model call, and the outcome
    /// reports the status the telemetry record carries.
    #[tokio::test]
    async fn a_compaction_publishes_its_boundary_pair_and_its_outcome() {
        let outcome = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
            "after compact",
            Vec::new(),
        ))]))
        .with_compactor(FakeCompactor)
        .with_baseline(SessionStats {
            context_tokens: 150_000,
            ..SessionStats::default()
        })
        .with_compaction_settings(CompactionSettings {
            auto_compact_threshold: 120_000,
            ..CompactionSettings::default()
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn runs");

        let compaction_events = outcome
            .events
            .iter()
            .filter(|envelope| {
                matches!(
                    envelope.event,
                    EngineEvent::CompactionStarted { .. }
                        | EngineEvent::CompactionCompleted { .. }
                        | EngineEvent::CompactionOutcome { .. }
                )
            })
            .map(|envelope| envelope.event.clone())
            .collect::<Vec<_>>();
        let [started, completed, reported] = compaction_events.as_slice() else {
            panic!("a compaction publishes three events, got {compaction_events:?}");
        };
        let EngineEvent::CompactionStarted {
            compaction_id,
            current_context_tokens,
            threshold,
        } = started
        else {
            unreachable!("the first event is the start");
        };
        assert_eq!(*current_context_tokens, 150_000);
        assert_eq!(*threshold, 120_000);
        assert_eq!(
            completed,
            &EngineEvent::CompactionCompleted {
                compaction_id: compaction_id.clone(),
                summary_length: 15,
                old_session_id: "session-1".to_owned(),
                new_session_id: "session-2".to_owned(),
            },
            "the pair is correlated and the length is the summary's characters"
        );
        assert_eq!(
            reported,
            &EngineEvent::CompactionOutcome {
                compaction_id: compaction_id.clone(),
                status: CompactionStatus::Success,
                context_tokens_before: 150_000,
                threshold: 120_000,
                reason: None,
            }
        );
        // The start is emitted before the model call the compaction makes, so a
        // client renders the operation rather than its aftermath.
        let started_at = outcome
            .events
            .iter()
            .position(|envelope| matches!(envelope.event, EngineEvent::CompactionStarted { .. }));
        let handoff_at = outcome
            .events
            .iter()
            .position(|envelope| matches!(envelope.event, EngineEvent::SessionHandoff { .. }));
        assert!(started_at < handoff_at);
    }

    /// US-151: a failed compaction reports its classified reason and publishes
    /// no completed event, so nothing renders a handoff that never happened.
    #[tokio::test]
    async fn a_failed_compaction_reports_its_reason_and_no_end_event() {
        struct FailingCompactor;

        impl Compactor for FailingCompactor {
            fn compact<'a>(
                &'a self,
                _current_session_id: &'a str,
                _messages: &'a [ModelMessage],
            ) -> CompactionFuture<'a> {
                Box::pin(async {
                    Err(CompactionFailure::classified(
                        CompactionFailureReason::EmptySummary,
                        "the summarizer answered with nothing",
                    ))
                })
            }

            fn cleared_session_id(&self, _current_session_id: &str) -> Result<String, String> {
                Err("unused".to_owned())
            }
        }

        let recorded = Arc::new(RecordingObserver::default());
        let error =
            ConversationEngine::new(ScriptedProvider::new([Err(ProviderError::ContextOverflow)]))
                .with_compactor(FailingCompactor)
                .with_observer(Arc::clone(&recorded) as Arc<dyn EventObserver>)
                .run_turn(
                    "session-1",
                    provider_input(),
                    "hello",
                    CancellationToken::default(),
                )
                .await
                .expect_err("a failed compaction fails the turn");
        assert!(matches!(error, EngineError::Compaction(_)), "{error}");

        let events = recorded.events.lock().expect("observer lock").clone();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::CompactionStarted { .. })),
            "the start was published before the failure"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::CompactionCompleted { .. })),
            "a failure publishes no end event"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::SessionHandoff { .. })),
            "a failure records no handoff"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::CompactionOutcome {
                    status: CompactionStatus::Failure,
                    reason: Some(CompactionFailureReason::EmptySummary),
                    ..
                }
            )),
            "the outcome carries the classified reason: {events:?}"
        );
    }

    #[test]
    fn a_transcript_written_before_the_pipeline_still_deserializes() {
        // The pipeline added no event variant and no message field, so every
        // envelope a previous release wrote still reads back.
        for stored in [
            json!({"sessionId": "s", "eventId": 1, "type": "user_message", "content": "hello"}),
            json!({"sessionId": "s", "eventId": 2, "type": "context_injected", "content": "note", "as_message": false}),
            json!({"sessionId": "s", "eventId": 3, "type": "compaction", "summary": "short"}),
        ] {
            serde_json::from_value::<EventEnvelope>(stored.clone())
                .unwrap_or_else(|error| panic!("{stored} still deserializes: {error}"));
        }
        let message: ModelMessage =
            serde_json::from_value(json!({"role": "user", "content": "hello"}))
                .expect("a stored user message still deserializes");
        assert_eq!(message, ModelMessage::user("hello".to_owned()));
    }

    /// The invoked-skill resolver the injection tests drive: `/probe` is the
    /// one user-invocable skill, resolved the way the builtin resolver does.
    struct StubSkills;

    impl crate::skills::InvokedSkillResolver for StubSkills {
        fn resolve(&self, prompt: &str) -> Option<crate::skills::InvokedSkill> {
            let name = prompt
                .trim()
                .strip_prefix('/')?
                .split_whitespace()
                .next()?
                .to_ascii_lowercase();
            if name != "probe" {
                return None;
            }
            let marker = crate::skills::skill_content_marker("probe");
            Some(crate::skills::InvokedSkill {
                name: "probe".to_owned(),
                loaded: ToolExecutionOutput::new(format!(
                        "name: probe\ncontent: {marker}\nDo the probing.\n</skill_content>\nskill_dir: None"
                    ))
                            .displayed_as(json!({"kind": "skill", "name": "probe"}))
                            .typed(json!({"name": "probe"})),
                already_loaded: ToolExecutionOutput::new("name: probe\ncontent: already loaded; reuse those instructions\nskill_dir: None"
                        .to_owned())
                                    .displayed_as(json!({"kind": "skill", "name": "probe"}))
                                    .typed(json!({"name": "probe"})),
            })
        }
    }

    /// US-172: a `/name` prompt appends the synthetic pair the reference
    /// writes: an assistant message whose only content is one `skill` tool
    /// call, then the tool message carrying the rendering, both before the
    /// first model request, which therefore reads them.
    #[tokio::test]
    async fn a_slash_invocation_appends_the_synthetic_skill_pair() {
        let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
        let observer = Arc::new(RecordingObserver::default());
        let outcome = ConversationEngine::new(provider)
            .with_observer(observer.clone())
            .with_invoked_skills(Arc::new(StubSkills))
            .run_turn(
                "session-1",
                provider_input(),
                "/probe extra instructions here",
                CancellationToken::default(),
            )
            .await
            .expect("turn completes");

        let messages = &outcome.messages;
        assert_eq!(
            messages[1],
            ModelMessage::user("/probe extra instructions here".to_owned()),
            "the trailing text stays the operator's message"
        );
        let ModelMessage::Assistant {
            content,
            tool_calls,
            ..
        } = &messages[2]
        else {
            panic!("the pair opens with an assistant message: {messages:?}");
        };
        assert_eq!(content, "", "the assistant message carries no text");
        assert_eq!(tool_calls.len(), 1, "exactly one call: {tool_calls:?}");
        assert_eq!(tool_calls[0].name, "skill");
        assert_eq!(
            serde_json::from_str::<Value>(&tool_calls[0].arguments).expect("arguments are JSON"),
            json!({"name": "probe"}),
        );
        let id = tool_calls[0].id.as_str();
        let shape = id.split('-').map(str::len).collect::<Vec<_>>();
        assert_eq!(shape, [8, 4, 4, 4, 12], "the locally minted id shape: {id}");
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{id}"
        );
        let ModelMessage::Tool {
            call_id,
            content,
            is_error,
        } = &messages[3]
        else {
            panic!("the call is answered by a tool message: {messages:?}");
        };
        assert_eq!(call_id, id, "the result answers the minted call");
        assert!(!is_error);
        assert!(
            content.contains(&crate::skills::skill_content_marker("probe")),
            "the first load renders the body: {content}"
        );
        assert!(
            content.starts_with("name: probe\ncontent:"),
            "the result is the fields one per line: {content}"
        );

        let events = observer.events.lock().expect("events");
        let call_index = events
            .iter()
            .position(
                |event| matches!(event, EngineEvent::ToolCall { name, .. } if name == "skill"),
            )
            .expect("the synthetic call is a turn event");
        assert!(
            matches!(
                &events[call_index + 1],
                EngineEvent::ToolResult {
                    is_error: false,
                    ..
                }
            ),
            "the result event follows the call: {events:?}"
        );
    }

    /// US-172: the second invocation of a skill already in the stored history
    /// is acknowledged rather than rendered again, decided by searching the
    /// stored tool messages of `skill` calls for the content marker.
    #[tokio::test]
    async fn a_second_invocation_is_acknowledged_from_the_stored_history() {
        let marker = crate::skills::skill_content_marker("probe");
        let mut input = provider_input();
        input.messages.extend([
            ModelMessage::Assistant {
                content: String::new(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: vec![ModelToolCall {
                    id: "earlier-call".to_owned(),
                    name: "skill".to_owned(),
                    arguments: "{\"name\": \"probe\"}".to_owned(),
                }],
            },
            ModelMessage::Tool {
                call_id: "earlier-call".to_owned(),
                content: format!("name: probe\ncontent: {marker}\nDo the probing."),
                is_error: false,
            },
        ]);
        let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
        let outcome = ConversationEngine::new(provider)
            .with_invoked_skills(Arc::new(StubSkills))
            .run_turn("session-1", input, "/probe", CancellationToken::default())
            .await
            .expect("turn completes");

        let repeat = outcome
            .messages
            .iter()
            .filter(|message| matches!(message, ModelMessage::Tool { .. }))
            .nth(1)
            .expect("the invocation still appends a tool message");
        let ModelMessage::Tool { content, .. } = repeat else {
            unreachable!()
        };
        assert!(
            content.contains("already loaded"),
            "the repeat is acknowledged: {content}"
        );
        assert!(
            !content.contains(&marker),
            "the body is not rendered again: {content}"
        );
    }

    /// US-172: the marker alone does not mark a skill loaded; it has to sit in
    /// the answer of a `skill` call, so a file read that happens to contain the
    /// marker text does not swallow a later invocation.
    #[tokio::test]
    async fn a_marker_in_an_unrelated_tool_answer_does_not_count_as_loaded() {
        let mut input = provider_input();
        input.messages.extend([
            ModelMessage::Assistant {
                content: String::new(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: vec![ModelToolCall {
                    id: "read-call".to_owned(),
                    name: "read".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            },
            ModelMessage::Tool {
                call_id: "read-call".to_owned(),
                content: crate::skills::skill_content_marker("probe"),
                is_error: false,
            },
        ]);
        let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
        let outcome = ConversationEngine::new(provider)
            .with_invoked_skills(Arc::new(StubSkills))
            .run_turn("session-1", input, "/probe", CancellationToken::default())
            .await
            .expect("turn completes");

        let appended = outcome
            .messages
            .iter()
            .filter(|message| matches!(message, ModelMessage::Tool { .. }))
            .nth(1)
            .expect("the pair is appended");
        let ModelMessage::Tool { content, .. } = appended else {
            unreachable!()
        };
        assert!(
            !content.contains("already loaded"),
            "the unrelated marker did not count: {content}"
        );
    }

    /// US-172: a slash word naming no skill, or a session publishing no
    /// resolver, leaves the prompt an ordinary message with no pair.
    #[tokio::test]
    async fn a_slash_word_naming_no_skill_is_an_ordinary_prompt() {
        let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
        let observer = Arc::new(RecordingObserver::default());
        let outcome = ConversationEngine::new(provider)
            .with_observer(observer.clone())
            .with_invoked_skills(Arc::new(StubSkills))
            .run_turn(
                "session-1",
                provider_input(),
                "/unknown do things",
                CancellationToken::default(),
            )
            .await
            .expect("turn completes");

        assert!(
            !outcome
                .messages
                .iter()
                .any(|message| matches!(message, ModelMessage::Tool { .. })),
            "no pair was appended: {:?}",
            outcome.messages
        );
        assert!(
            !observer
                .events
                .lock()
                .expect("events")
                .iter()
                .any(|event| matches!(event, EngineEvent::ToolCall { .. })),
            "no synthetic call was emitted"
        );
    }

    /// US-173: the steer control carries the wire's `injectInvokedSkill`, so a
    /// steer naming a skill appends the pair when the flag is true and stays a
    /// plain message when it is false.
    #[tokio::test]
    async fn a_steer_injects_the_skill_only_when_its_flag_says_so() {
        for (inject, expected_pairs) in [(true, 1_usize), (false, 0_usize)] {
            let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
            let controls = TurnControlHandle::default();
            controls
                .send(TurnControl::Steer {
                    content: "/probe".to_owned(),
                    inject_invoked_skill: inject,
                })
                .expect("queue steer");
            let outcome = ConversationEngine::new(provider)
                .with_invoked_skills(Arc::new(StubSkills))
                .run_turn_controlled(
                    "session-1",
                    provider_input(),
                    "first ask",
                    CancellationToken::default(),
                    controls,
                )
                .await
                .expect("turn completes");

            let pairs = outcome
                .messages
                .iter()
                .filter(|message| matches!(message, ModelMessage::Tool { .. }))
                .count();
            assert_eq!(
                pairs, expected_pairs,
                "injectInvokedSkill={inject}: {:?}",
                outcome.messages
            );
        }
    }

    /// US-172: the pair round-trips through serialization unchanged, so a
    /// persisted transcript carrying one reloads as the conversation it was.
    #[test]
    fn the_synthetic_pair_round_trips_through_serialization() {
        use crate::skills::InvokedSkillResolver as _;
        let mut messages = vec![ModelMessage::user("/probe".to_owned())];
        let invoked = StubSkills.resolve("/probe").expect("resolves");
        crate::skills::append_invoked_skill(&mut messages, &invoked);
        let stored = serde_json::to_value(&messages).expect("serializes");
        let reloaded: Vec<ModelMessage> = serde_json::from_value(stored).expect("deserializes");
        assert_eq!(reloaded, messages);
    }
}
