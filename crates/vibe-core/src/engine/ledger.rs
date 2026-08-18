//! What a turn accumulates, and what it publishes as it goes.
//!
//! [`TurnLedger`] is the arithmetic: tokens, price and steps, carried forward
//! from the session's baseline so a budget spans the conversation rather than
//! the turn. A compaction credits what it spent without spending a step, which
//! is what lets a reactive recovery retry the request it made room for.
//!
//! [`TurnRecorder`] is the publication: the single place an engine event is
//! stamped, reduced into the projection, handed to the observer and retained.
//! Nothing else in the engine touches the projection, so an event that reaches
//! a client and an event that reaches the transcript are always the same event.

use crate::events::{
    EngineEvent, EventEnvelope, LifecycleState, ModelMessage, ProjectionReducer, ProjectionSnapshot,
};
use crate::provider::Usage;

use super::{
    EngineError, EngineLimits, EventObserver, SessionStats, TranscriptSink, TurnStopReason,
};

/// Accumulates everything a turn spends against [`EngineLimits`].
pub(super) struct TurnLedger {
    pub(super) usage: Usage,
    pub(super) context_tokens: u64,
    pub(super) price_micros: u64,
    pub(super) steps: u32,
}

impl TurnLedger {
    pub(super) fn new(baseline: &SessionStats, limits: &EngineLimits) -> Self {
        Self {
            price_micros: total_price_micros(&baseline.usage, limits),
            usage: baseline.usage.clone(),
            context_tokens: baseline.context_tokens,
            steps: baseline.steps,
        }
    }

    /// The stats a conversation policy reads, and the stats a turn persists.
    pub(super) fn session_stats(&self) -> SessionStats {
        SessionStats {
            usage: self.usage.clone(),
            context_tokens: self.context_tokens,
            steps: self.steps,
        }
    }

    pub(super) fn record_completion(&mut self, usage: &Usage, limits: &EngineLimits) {
        self.steps = self.steps.saturating_add(1);
        self.context_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.price_micros = total_price_micros(&self.usage, limits);
    }

    /// Credits what a compaction spent, without spending a step.
    ///
    /// A compaction is not a turn of the conversation, so it never advances the
    /// step budget, which is what lets a reactive recovery retry the request it
    /// made room for. `context_tokens` is not set from this usage either: the
    /// transcript it described no longer exists. The reference zeroes it and
    /// lets the next completion recompute it from real usage, which is the one
    /// number nothing can approximate without a request.
    pub(super) fn record_compaction(
        &mut self,
        usage: &Usage,
        limits: &EngineLimits,
        replaced: bool,
    ) {
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.price_micros = total_price_micros(&self.usage, limits);
        if replaced {
            self.context_tokens = 0;
        }
    }
}

/// Collects a turn's events while keeping the public projection in step.
///
/// Every engine event flows through [`TurnRecorder::emit`], which is the single
/// place where an event is stamped, reduced, observed, and retained.
pub(super) struct TurnRecorder<'a> {
    observer: &'a dyn EventObserver,
    reducer: ProjectionReducer,
    events: Vec<EventEnvelope>,
    next_event_id: u64,
}

impl<'a> TurnRecorder<'a> {
    pub(super) fn new(
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

    pub(super) fn state(&self) -> &ProjectionSnapshot {
        self.reducer.state()
    }

    /// The public entry identifier the projection gave the event just emitted,
    /// which is what a client reads as `id`.
    pub(super) fn last_entry_id(&self) -> Option<String> {
        let event_id = self.next_event_id.checked_sub(1)?;
        Some(self.state().turn_id.as_ref().map_or_else(
            || format!("entry-{event_id}"),
            |turn_id| format!("entry-{turn_id}-{event_id}"),
        ))
    }

    pub(super) fn has_callback(&self, callback_id: &str) -> bool {
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

    pub(super) fn emit(&mut self, event: EngineEvent) -> Result<(), EngineError> {
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

    pub(super) fn finish(self) -> (Vec<EventEnvelope>, ProjectionSnapshot) {
        (self.events, self.reducer.into_state())
    }
}

pub(super) async fn persist(
    sink: &impl TranscriptSink,
    messages: &[ModelMessage],
    snapshot: &ProjectionSnapshot,
) -> Result<(), EngineError> {
    sink.persist(messages, snapshot)
        .await
        .map_err(EngineError::Persistence)
}

pub(super) async fn persist_stats(
    sink: &impl TranscriptSink,
    stats: &SessionStats,
) -> Result<(), EngineError> {
    sink.persist_stats(stats)
        .await
        .map_err(EngineError::Persistence)
}

/// Total price of `usage` under `limits`, in micros.
pub(super) fn total_price_micros(usage: &Usage, limits: &EngineLimits) -> u64 {
    priced_tokens(usage.input_tokens, limits.input_price_per_million_micros).saturating_add(
        priced_tokens(usage.output_tokens, limits.output_price_per_million_micros),
    )
}

/// The lifecycle state a stop reason lands in.
pub(super) const fn lifecycle_for(reason: &TurnStopReason) -> LifecycleState {
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
pub(super) fn priced_tokens(tokens: u64, price_per_million_micros: u64) -> u64 {
    const MILLION: u128 = 1_000_000;
    u64::try_from(u128::from(tokens).saturating_mul(u128::from(price_per_million_micros)) / MILLION)
        .unwrap_or(u64::MAX)
}

pub(super) fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The identifier that correlates a compaction's boundary events.
///
/// The reference mints a UUID; what matters here is that two compactions in one
/// session never collide, so the identifier carries real entropy and falls back
/// to the event stream's own ordering when the platform has none.
pub(super) fn new_compaction_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        bytes[..16].copy_from_slice(&stamp.to_le_bytes());
    }
    let hexadecimal = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("compaction-{hexadecimal}")
}

pub(super) fn title_from_messages(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            ModelMessage::User { content, .. } => Some(content),
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

pub(super) fn stop_message(reason: &TurnStopReason) -> &'static str {
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
