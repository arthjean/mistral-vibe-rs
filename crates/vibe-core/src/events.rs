use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::compaction::{CompactionFailureReason, CompactionStatus};

mod reduce;

use reduce::reduce_event;

pub mod detail;

pub use detail::{
    ApprovalDecision, ApprovalDecisionType, CallbackDetail, CallbackOutput, EffectCallDisplay,
    EffectDetail, EffectResultDisplay, HookNotice, HookScope, HookSeverity, NoticeDetail,
    QuestionChoice, TodoEffectItem, TodoEffectPriority, TodoEffectStatus, ToolEffectKind,
    UserAnswer, UserQuestion, UserQuestionRequest, UserQuestionResult,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub session_id: String,
    #[serde(skip)]
    pub turn_id: Option<String>,
    #[serde(skip)]
    pub emitted_at: u64,
    pub event_id: u64,
    #[serde(flatten)]
    pub event: EngineEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    UserMessage {
        content: String,
    },
    UserSteer {
        content: String,
    },
    ContextInjected {
        content: String,
        as_message: bool,
    },
    ModelText {
        text: String,
    },
    ModelReasoning {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolStream {
        call_id: String,
        chunk: String,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        typed_result: Value,
        #[serde(default)]
        display: Value,
        #[serde(default)]
        duration_ms: u64,
        is_error: bool,
        #[serde(default)]
        cancelled: bool,
    },
    CallbackRequested {
        callback_id: String,
        kind: CallbackKind,
        prompt: String,
    },
    CallbackResolved {
        callback_id: String,
        accepted: bool,
        #[serde(default)]
        value: Option<String>,
    },
    Hook {
        name: String,
        message: String,
    },
    /// A finished compaction, as this port published it before it emitted the
    /// reference's pair.
    ///
    /// Nothing emits it any more. It stays declared, and stays projected, so a
    /// transcript written before [`EngineEvent::CompactionStarted`] existed
    /// still replays; the precedent is [`SessionHandoffCause`], which kept its
    /// default for the same reason.
    Compaction {
        summary: String,
    },
    /// A compaction that is about to call the model. Reference
    /// `CompactStartEvent`.
    CompactionStarted {
        /// Correlates the pair, and is the identifier the projected entry is
        /// patched through. The reference calls it a tool call identifier
        /// because it borrows the tool-call channel to reach a client.
        compaction_id: String,
        current_context_tokens: u64,
        threshold: u64,
    },
    /// A compaction that replaced the transcript. Reference `CompactEndEvent`.
    CompactionCompleted {
        compaction_id: String,
        /// The summary's length in characters, which is what the reference
        /// publishes; the summary itself reaches a client as the transcript.
        summary_length: u64,
        old_session_id: String,
        new_session_id: String,
    },
    /// How a compaction ended, whatever the outcome.
    ///
    /// It carries no history, like [`EngineEvent::Stats`]: the projection
    /// ignores it and only a live observer reads it. It exists because this
    /// port reports telemetry from the event stream where the reference calls
    /// an injected client, and the reference reports the status of a compaction
    /// that failed or was cancelled, neither of which ends with a completed
    /// event.
    CompactionOutcome {
        compaction_id: String,
        status: CompactionStatus,
        context_tokens_before: u64,
        threshold: u64,
        /// The classified summarization failure, when the summarizer named one.
        #[serde(default)]
        reason: Option<CompactionFailureReason>,
    },
    Title {
        title: String,
    },
    SessionHandoff {
        from_session_id: String,
        to_session_id: String,
        /// Why the session rotated. Absent in transcripts written before
        /// clearing existed, which all recorded a compaction.
        #[serde(default)]
        cause: SessionHandoffCause,
    },
    Lifecycle {
        state: LifecycleState,
        #[serde(default)]
        message: Option<String>,
    },
    /// Usage observed after a model completion. It carries no history, so the
    /// projection is untouched and only live observers react to it.
    Stats {
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A provider request the backend is retrying. Like [`EngineEvent::Stats`]
    /// it carries no history: a client renders it as a transient state, not as
    /// a transcript entry.
    Retrying {
        reason: String,
    },
    /// A backend request the turn is about to make, and what it is made of.
    ///
    /// It carries no history, like [`EngineEvent::Stats`]: the projection
    /// ignores it and only a live observer reads it. The reference has no
    /// counterpart because its agent loop calls a telemetry client in place;
    /// this port reports telemetry from the event stream, so what that client
    /// would have read travels here. It carries more than any one payload
    /// needs: the profile and the image support are what the *tool* events this
    /// request produces report, resolved once where they are known.
    RequestSent {
        /// The model the request addresses, resolved from the turn's override
        /// or from the provider itself.
        model: String,
        /// The active agent profile, reference `self.agent_profile.name`.
        agent_profile: String,
        /// Characters across every message the request carries.
        nb_context_chars: u64,
        nb_context_messages: u64,
        /// Characters of the operator's own prompt for this turn.
        nb_prompt_chars: u64,
        /// Images the request carries, before the provider's support is read.
        nb_images: u64,
        /// Whether the provider serving the request accepts images at all,
        /// which is the reference's `supports_images` gate on the attachment
        /// counts.
        supports_images: bool,
        /// The public entry the operator's message was projected as, which is
        /// what the reference calls `_current_user_message_id`.
        #[serde(default)]
        message_id: Option<String>,
    },
}

/// What rotated the session under an active turn.
///
/// Both causes hand the turn a fresh identifier and a shorter transcript, and a
/// client tells them apart to explain the transcript it just saw shrink: a
/// compaction summarized it, a clearing dropped it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionHandoffCause {
    #[default]
    Compaction,
    ContextCleared {
        /// The plan whose acceptance cleared the context, when one did.
        #[serde(default)]
        plan_file_path: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    Approval,
    UserInput,
    ConnectorAuth,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    #[default]
    Idle,
    Running,
    WaitingCallback,
    Completed,
    Failed,
    Cancelled,
}

impl LifecycleState {
    fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::WaitingCallback)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEntryMetadata {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub generation_status: PublicEntryGenerationStatus,
    #[serde(default)]
    pub related_entry_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicEntryGenerationStatus {
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicMessageRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicMessageSource {
    TurnStart,
    TurnSteer,
    Harness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicContentBlock {
    Text { text: String },
    Image { attachment: Value },
    Resource { resource: Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PublicEffectState {
    Pending,
    Running {
        #[serde(default)]
        output_text: String,
    },
    Blocked {
        callback_id: String,
        #[serde(default)]
        output_text: String,
    },
    Completed {
        #[serde(default)]
        output: Value,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: u64,
        display: EffectResultDisplay,
    },
    Failed {
        error: PublicError,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: u64,
        display: EffectResultDisplay,
    },
    Cancelled {
        reason: String,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: u64,
        /// A cancellation that produced no result carries no display, which is
        /// the one settled state the reference lets publish a null one.
        #[serde(default)]
        display: Option<EffectResultDisplay>,
    },
    Skipped {
        reason: String,
        display: EffectResultDisplay,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicError {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicTurnStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicTurnStopReason {
    Limit,
}

/// Why a turn failed, in the vocabulary a client branches on.
///
/// A failing turn always carries one of these: the message explains, the code
/// is what a UI can act on, so it is classified from the failure's type rather
/// than from the text it rendered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnErrorCode {
    RateLimit,
    ContextTooLong,
    ResponseTooLong,
    Refusal,
    InvalidImageAttachment,
    ImagesNotSupported,
    CompactionFailed,
    BackendError,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTurn {
    pub id: String,
    pub session_id: String,
    pub status: PublicTurnStatus,
    pub started_at: u64,
    #[serde(default)]
    pub completed_at: Option<u64>,
    #[serde(default)]
    pub error: Option<PublicError>,
    #[serde(default)]
    pub stop_reason: Option<PublicTurnStopReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PublicCallbackState {
    Open,
    Answered { output: CallbackOutput },
    Cancelled { reason: String },
    Expired { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicNoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PublicHistoryEntry {
    Message {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        role: PublicMessageRole,
        content: Vec<PublicContentBlock>,
        #[serde(default)]
        source: Option<PublicMessageSource>,
        #[serde(default)]
        user_display_content: Option<Value>,
    },
    Reasoning {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        text: String,
        #[serde(default)]
        summary: Vec<String>,
    },
    Effect {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        title: String,
        /// Boxed because the call and result displays it carries are what make
        /// an effect entry three times the size of every other variant.
        detail: Box<EffectDetail>,
        state: PublicEffectState,
        /// Which model tool call this effect projects.
        ///
        /// The reference does not publish it and a conforming client rejects a
        /// surplus field, so it stays off the wire and lives here, where the
        /// reducer correlates a stream chunk or a result with the call it
        /// belongs to.
        #[serde(skip)]
        tool_call_id: String,
    },
    Callback {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        callback_id: String,
        title: String,
        detail: CallbackDetail,
        state: PublicCallbackState,
    },
    Checkpoint {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        kind: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        details: Value,
    },
    Notice {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        level: PublicNoticeLevel,
        message: String,
        detail: NoticeDetail,
    },
}

impl PublicHistoryEntry {
    #[must_use]
    pub fn metadata(&self) -> &PublicEntryMetadata {
        match self {
            Self::Message { metadata, .. }
            | Self::Reasoning { metadata, .. }
            | Self::Effect { metadata, .. }
            | Self::Callback { metadata, .. }
            | Self::Checkpoint { metadata, .. }
            | Self::Notice { metadata, .. } => metadata,
        }
    }

    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.metadata().generation_status == PublicEntryGenerationStatus::Completed
    }

    pub fn rebind_session(&mut self, session_id: impl Into<String>) {
        self.metadata_mut().session_id = session_id.into();
    }

    fn metadata_mut(&mut self) -> &mut PublicEntryMetadata {
        match self {
            Self::Message { metadata, .. }
            | Self::Reasoning { metadata, .. }
            | Self::Effect { metadata, .. }
            | Self::Callback { metadata, .. }
            | Self::Checkpoint { metadata, .. }
            | Self::Notice { metadata, .. } => metadata,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ModelMessage {
    System {
        content: String,
    },
    User {
        content: String,
        /// Whether the harness wrote this turn rather than the operator.
        ///
        /// The reference carries the same flag on every message and compaction
        /// reads it twice: an injected turn is not preserved through a
        /// compaction, and the envelope compaction writes is itself injected,
        /// which is what lets a second compaction tell its own envelope from a
        /// real turn. It defaults on read, so a transcript persisted before the
        /// flag existed loads as operator-authored, which is what it was.
        #[serde(default)]
        injected: bool,
    },
    Assistant {
        content: String,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        reasoning_signature: Option<String>,
        #[serde(default)]
        reasoning_state: Vec<String>,
        #[serde(default)]
        tool_calls: Vec<ModelToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl ModelMessage {
    /// A user turn the operator wrote.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            injected: false,
        }
    }

    /// A user turn the harness wrote: a policy injection, or the envelope a
    /// compaction leaves behind.
    #[must_use]
    pub fn injected_user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            injected: true,
        }
    }

    /// The text this message carries, which every variant has.
    #[must_use]
    pub fn content(&self) -> &str {
        match self {
            Self::System { content }
            | Self::User { content, .. }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        }
    }

    /// Whether this is a user turn, which is where a round begins.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// Whether the harness wrote this message. Only a user turn can be
    /// injected; every other role is authored by the model or the tool that
    /// answered.
    #[must_use]
    pub fn is_injected(&self) -> bool {
        matches!(self, Self::User { injected: true, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionSnapshot {
    pub session_id: String,
    #[serde(skip)]
    pub turn_id: Option<String>,
    /// Why the session last rotated under this turn, when one did.
    ///
    /// Not serialized: it describes the write the rotation still owes rather
    /// than the projected state. A sink reads it for the reference's
    /// `keep_parent` distinction, which retains the previous identifier as the
    /// parent of a compacted session and retains nothing for a cleared one.
    #[serde(skip)]
    pub handoff_cause: Option<SessionHandoffCause>,
    pub watermark: u64,
    pub lifecycle: LifecycleState,
    pub title: Option<String>,
    pub history: Vec<PublicHistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionReducer {
    state: ProjectionSnapshot,
}

impl ProjectionReducer {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            state: ProjectionSnapshot {
                session_id: session_id.into(),
                turn_id: None,
                handoff_cause: None,
                watermark: 0,
                lifecycle: LifecycleState::Idle,
                title: None,
                history: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn for_turn(session_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        let mut reducer = Self::new(session_id);
        reducer.state.turn_id = Some(turn_id.into());
        reducer
    }

    #[must_use]
    pub fn state(&self) -> &ProjectionSnapshot {
        &self.state
    }

    /// Consumes the reducer, yielding the projection it accumulated.
    #[must_use]
    pub fn into_state(self) -> ProjectionSnapshot {
        self.state
    }

    pub fn apply(&mut self, envelope: &EventEnvelope) -> Result<ApplyOutcome, ProjectionError> {
        if envelope.event_id == 0 {
            return Err(ProjectionError::InvalidEventId);
        }
        if envelope.session_id != self.state.session_id {
            return Err(ProjectionError::ForeignSession {
                expected: self.state.session_id.clone(),
                actual: envelope.session_id.clone(),
            });
        }
        if envelope.event_id <= self.state.watermark {
            return Ok(ApplyOutcome::Duplicate);
        }
        let expected = self.state.watermark.saturating_add(1);
        if envelope.event_id != expected {
            return Err(ProjectionError::Gap {
                expected,
                actual: envelope.event_id,
            });
        }

        // Reduced in place rather than into a copy: every arm of
        // [`reduce_event`] answers its refusals before it touches the state, so
        // a rejected event leaves the projection exactly as it was without the
        // whole history being cloned once per event. `a_rejected_event_leaves_
        // the_projection_untouched` holds that invariant.
        reduce_event(
            &mut self.state,
            envelope.event_id,
            envelope.emitted_at,
            &envelope.event,
        )?;
        self.state.watermark = envelope.event_id;
        Ok(ApplyOutcome::Applied)
    }

    pub fn restore(
        &mut self,
        snapshot: ProjectionSnapshot,
        expected_watermark: u64,
    ) -> Result<(), ProjectionError> {
        if snapshot.session_id != self.state.session_id {
            return Err(ProjectionError::ForeignSession {
                expected: self.state.session_id.clone(),
                actual: snapshot.session_id,
            });
        }
        if snapshot.watermark != expected_watermark {
            return Err(ProjectionError::WatermarkMismatch {
                expected: expected_watermark,
                actual: snapshot.watermark,
            });
        }
        self.state = snapshot;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectionError {
    #[error("event IDs must be positive")]
    InvalidEventId,
    #[error("event gap: expected {expected}, received {actual}")]
    Gap { expected: u64, actual: u64 },
    #[error("foreign session: expected `{expected}`, received `{actual}`")]
    ForeignSession { expected: String, actual: String },
    #[error("snapshot watermark mismatch: expected {expected}, received {actual}")]
    WatermarkMismatch { expected: u64, actual: u64 },
    #[error("illegal lifecycle transition from {from:?} through `{event}`")]
    IllegalTransition {
        from: LifecycleState,
        event: &'static str,
    },
    #[error("callback `{0}` is not pending")]
    CallbackNotPending(String),
    #[error("handoff source `{actual}` does not match current session `{expected}`")]
    InvalidHandoff { expected: String, actual: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: u64, event: EngineEvent) -> EventEnvelope {
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: None,
            emitted_at: id,
            event_id: id,
            event,
        }
    }

    /// A projection that refuses an event is byte-identical to the one before
    /// it, for every refusal [`reduce_event`] can raise.
    ///
    /// [`ProjectionReducer::apply`] reduces in place, so this is the invariant
    /// that keeps a rejected event from leaving a half-applied entry behind. An
    /// arm that grows a check after a mutation fails here.
    #[test]
    fn a_rejected_event_leaves_the_projection_untouched() {
        let mut reducer = ProjectionReducer::new("session-1");
        // A refused event never advances the watermark, so the next one reuses
        // its identifier: the sequence has no gap for the reducer to reject
        // before it reaches the arm under test.
        let mut next_id = 1_u64;
        let mut apply = |reducer: &mut ProjectionReducer, projected: EngineEvent| {
            let outcome = reducer.apply(&event(next_id, projected));
            if outcome.is_ok() {
                next_id += 1;
            }
            outcome
        };

        // Every refusal an idle projection can raise, before anything is
        // projected at all: the empty state must survive them.
        let idle = reducer.state().clone();
        for refused in [
            EngineEvent::UserSteer {
                content: "steering a turn that never started".to_owned(),
            },
            EngineEvent::ModelText {
                text: "text with no turn".to_owned(),
            },
            EngineEvent::Title {
                title: "a title with no turn".to_owned(),
            },
        ] {
            let error = apply(&mut reducer, refused).expect_err("an idle turn refuses this");
            assert!(
                matches!(error, ProjectionError::IllegalTransition { .. }),
                "{error:?}"
            );
            assert_eq!(reducer.state(), &idle, "a refusal projected something");
        }

        apply(
            &mut reducer,
            EngineEvent::UserMessage {
                content: "start".to_owned(),
            },
        )
        .expect("the turn starts");
        apply(
            &mut reducer,
            EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "bash".to_owned(),
                arguments: "{}".to_owned(),
            },
        )
        .expect("the tool call projects");
        let running = reducer.state().clone();

        // A stream and a result naming a call the projection never saw, and a
        // handoff leaving a session this projection is not on.
        let stream = apply(
            &mut reducer,
            EngineEvent::ToolStream {
                call_id: "call-unknown".to_owned(),
                chunk: "orphan chunk".to_owned(),
            },
        )
        .expect_err("a stream without its call is refused");
        assert!(
            matches!(stream, ProjectionError::IllegalTransition { .. }),
            "{stream:?}"
        );
        assert_eq!(reducer.state(), &running);

        let result = apply(
            &mut reducer,
            EngineEvent::ToolResult {
                call_id: "call-unknown".to_owned(),
                content: "orphan result".to_owned(),
                typed_result: Value::Null,
                display: Value::Null,
                duration_ms: 1,
                is_error: false,
                cancelled: false,
            },
        )
        .expect_err("a result without its call is refused");
        assert!(
            matches!(result, ProjectionError::IllegalTransition { .. }),
            "{result:?}"
        );
        assert_eq!(reducer.state(), &running);

        let handoff = apply(
            &mut reducer,
            EngineEvent::SessionHandoff {
                from_session_id: "session-elsewhere".to_owned(),
                to_session_id: "session-2".to_owned(),
                cause: SessionHandoffCause::Compaction,
            },
        )
        .expect_err("a handoff from another session is refused");
        assert!(
            matches!(handoff, ProjectionError::InvalidHandoff { .. }),
            "{handoff:?}"
        );
        assert_eq!(
            reducer.state(),
            &running,
            "a refused handoff rebound the session"
        );

        // A callback answered under an identifier no open callback carries.
        apply(
            &mut reducer,
            EngineEvent::CallbackRequested {
                callback_id: "callback-1".to_owned(),
                kind: CallbackKind::Approval,
                prompt: "may I".to_owned(),
            },
        )
        .expect("the callback opens");
        let waiting = reducer.state().clone();
        let pending = apply(
            &mut reducer,
            EngineEvent::CallbackResolved {
                callback_id: "callback-unknown".to_owned(),
                accepted: true,
                value: None,
            },
        )
        .expect_err("an unknown callback is refused");
        assert!(
            matches!(pending, ProjectionError::CallbackNotPending(_)),
            "{pending:?}"
        );
        assert_eq!(reducer.state(), &waiting);

        // The turn still runs to completion afterward, so no refusal left the
        // projection in a state the next event cannot build on.
        apply(
            &mut reducer,
            EngineEvent::CallbackResolved {
                callback_id: "callback-1".to_owned(),
                accepted: true,
                value: None,
            },
        )
        .expect("the open callback resolves");
        apply(
            &mut reducer,
            EngineEvent::Lifecycle {
                state: LifecycleState::Completed,
                message: None,
            },
        )
        .expect("the turn completes");
        assert_eq!(reducer.state().lifecycle, LifecycleState::Completed);
    }

    #[test]
    fn reducer_is_monotonic_and_transactional() {
        let mut reducer = ProjectionReducer::new("session-1");
        assert_eq!(
            reducer
                .apply(&event(
                    1,
                    EngineEvent::UserMessage {
                        content: "hello".to_owned(),
                    },
                ))
                .expect("first event applies"),
            ApplyOutcome::Applied
        );
        let before = reducer.state().clone();
        assert_eq!(
            reducer
                .apply(&event(
                    1,
                    EngineEvent::ModelText {
                        text: "duplicate".to_owned(),
                    },
                ))
                .expect("duplicates are suppressed"),
            ApplyOutcome::Duplicate
        );
        assert_eq!(reducer.state(), &before);

        let foreign_duplicate = EventEnvelope {
            session_id: "session-2".to_owned(),
            turn_id: None,
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::ModelText {
                text: "foreign duplicate".to_owned(),
            },
        };
        assert!(matches!(
            reducer.apply(&foreign_duplicate),
            Err(ProjectionError::ForeignSession { .. })
        ));
        assert_eq!(reducer.state(), &before);

        assert_eq!(
            reducer.apply(&event(
                3,
                EngineEvent::ModelText {
                    text: "gap".to_owned(),
                }
            )),
            Err(ProjectionError::Gap {
                expected: 2,
                actual: 3
            })
        );
        assert_eq!(reducer.state(), &before);
    }

    #[test]
    fn invalid_events_leave_public_state_unchanged() {
        let mut reducer = ProjectionReducer::new("session-1");
        let before = reducer.state().clone();
        let foreign = EventEnvelope {
            session_id: "session-2".to_owned(),
            turn_id: None,
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::ModelText {
                text: "wrong".to_owned(),
            },
        };
        assert!(matches!(
            reducer.apply(&foreign),
            Err(ProjectionError::ForeignSession { .. })
        ));
        assert_eq!(reducer.state(), &before);

        assert!(matches!(
            reducer.apply(&event(
                1,
                EngineEvent::ModelText {
                    text: "illegal".to_owned(),
                }
            )),
            Err(ProjectionError::IllegalTransition { .. })
        ));
        assert_eq!(reducer.state(), &before);
    }

    #[test]
    fn callback_and_handoff_preserve_public_contract() {
        let mut reducer = ProjectionReducer::new("session-1");
        for envelope in [
            event(
                1,
                EngineEvent::UserMessage {
                    content: "ship it".to_owned(),
                },
            ),
            event(
                2,
                EngineEvent::CallbackRequested {
                    callback_id: "callback-1".to_owned(),
                    kind: CallbackKind::Approval,
                    prompt: "approve?".to_owned(),
                },
            ),
            event(
                3,
                EngineEvent::CallbackResolved {
                    callback_id: "callback-1".to_owned(),
                    accepted: true,
                    value: Some("yes".to_owned()),
                },
            ),
            event(
                4,
                EngineEvent::SessionHandoff {
                    from_session_id: "session-1".to_owned(),
                    to_session_id: "session-2".to_owned(),
                    cause: SessionHandoffCause::Compaction,
                },
            ),
        ] {
            reducer.apply(&envelope).expect("valid lifecycle event");
        }
        assert_eq!(reducer.state().session_id, "session-2");
        assert_eq!(reducer.state().watermark, 4);
        assert!(
            reducer
                .state()
                .history
                .iter()
                .all(|entry| entry.metadata().session_id == "session-2")
        );
        assert!(matches!(
            reducer.state().history.get(1),
            Some(PublicHistoryEntry::Callback {
                state: PublicCallbackState::Answered {
                    output: CallbackOutput::Approval { feedback, .. }
                },
                ..
            }) if feedback.as_deref() == Some("yes")
        ));
    }

    /// US-151: the boundary pair is one entry, created in progress and patched
    /// where the reference patches it.
    #[test]
    fn the_compaction_pair_projects_as_one_entry_that_is_patched() {
        let mut reducer = ProjectionReducer::new("session-1");
        for envelope in [
            event(
                1,
                EngineEvent::UserMessage {
                    content: "compact".to_owned(),
                },
            ),
            event(
                2,
                EngineEvent::CompactionStarted {
                    compaction_id: "compaction-1".to_owned(),
                    current_context_tokens: 150_000,
                    threshold: 120_000,
                },
            ),
        ] {
            reducer.apply(&envelope).expect("valid lifecycle event");
        }
        let entries = reducer.state().history.len();
        assert!(matches!(
            reducer.state().history.last(),
            Some(PublicHistoryEntry::Checkpoint { metadata, kind, message, .. })
                if kind == "compaction"
                    && metadata.generation_status == PublicEntryGenerationStatus::InProgress
                    && message.as_deref() == Some("Compacting context")
        ));

        reducer
            .apply(&event(
                3,
                EngineEvent::CompactionCompleted {
                    compaction_id: "compaction-1".to_owned(),
                    summary_length: 15,
                    old_session_id: "session-1".to_owned(),
                    new_session_id: "session-2".to_owned(),
                },
            ))
            .expect("the end event applies");
        assert_eq!(
            reducer.state().history.len(),
            entries,
            "the end event patches the entry the start created rather than adding one"
        );
        assert!(matches!(
            reducer.state().history.last(),
            Some(PublicHistoryEntry::Checkpoint { metadata, message, .. })
                if metadata.generation_status == PublicEntryGenerationStatus::Completed
                    && message.as_deref() == Some("Context compacted")
        ));

        // The outcome event carries no history, like the stats event.
        reducer
            .apply(&event(
                4,
                EngineEvent::CompactionOutcome {
                    compaction_id: "compaction-1".to_owned(),
                    status: CompactionStatus::Success,
                    context_tokens_before: 150_000,
                    threshold: 120_000,
                    reason: None,
                },
            ))
            .expect("the outcome applies");
        assert_eq!(reducer.state().history.len(), entries);
    }

    /// US-151: an end event whose start was never seen still leaves a coherent
    /// entry, which is what a late subscriber reads.
    #[test]
    fn an_end_event_without_its_start_still_projects_an_entry() {
        let mut reducer = ProjectionReducer::new("session-1");
        for envelope in [
            event(
                1,
                EngineEvent::UserMessage {
                    content: "compact".to_owned(),
                },
            ),
            event(
                2,
                EngineEvent::CompactionCompleted {
                    compaction_id: "compaction-1".to_owned(),
                    summary_length: 15,
                    old_session_id: "session-1".to_owned(),
                    new_session_id: "session-2".to_owned(),
                },
            ),
        ] {
            reducer.apply(&envelope).expect("valid lifecycle event");
        }
        assert!(matches!(
            reducer.state().history.last(),
            Some(PublicHistoryEntry::Checkpoint { metadata, kind, message, .. })
                if kind == "compaction"
                    && metadata.generation_status == PublicEntryGenerationStatus::Completed
                    && message.as_deref() == Some("Context compacted")
        ));
    }

    /// US-151: the variant this port emitted before the pair still projects the
    /// entry a stored transcript was written with.
    #[test]
    fn a_stored_compaction_event_projects_as_it_did() {
        let stored: EventEnvelope = serde_json::from_value(
            json!({"sessionId": "session-1", "eventId": 2, "type": "compaction", "summary": "short"}),
        )
        .expect("a stored compaction still deserializes");
        let mut reducer = ProjectionReducer::new("session-1");
        reducer
            .apply(&event(
                1,
                EngineEvent::UserMessage {
                    content: "compact".to_owned(),
                },
            ))
            .expect("the prompt applies");
        reducer.apply(&stored).expect("the stored event applies");
        assert!(matches!(
            reducer.state().history.last(),
            Some(PublicHistoryEntry::Checkpoint { metadata, kind, message, .. })
                if kind == "compaction"
                    && metadata.generation_status == PublicEntryGenerationStatus::Completed
                    && message.as_deref() == Some("short")
        ));
    }

    /// The reference publishes a clearing as a notice entry as well as a
    /// notification, and it names the plan whose acceptance cleared the
    /// context. A compaction publishes its checkpoint instead.
    #[test]
    fn a_cleared_context_leaves_a_notice_a_compaction_does_not() {
        let handoff = |id: u64, from: &str, to: &str, cause: SessionHandoffCause| EventEnvelope {
            session_id: from.to_owned(),
            turn_id: None,
            emitted_at: id,
            event_id: id,
            event: EngineEvent::SessionHandoff {
                from_session_id: from.to_owned(),
                to_session_id: to.to_owned(),
                cause,
            },
        };
        let mut reducer = ProjectionReducer::new("session-1");
        for envelope in [
            event(
                1,
                EngineEvent::UserMessage {
                    content: "clear it".to_owned(),
                },
            ),
            handoff(2, "session-1", "session-2", SessionHandoffCause::Compaction),
            handoff(
                3,
                "session-2",
                "session-3",
                SessionHandoffCause::ContextCleared {
                    plan_file_path: Some("/workspace/plan.md".to_owned()),
                },
            ),
        ] {
            reducer.apply(&envelope).expect("valid lifecycle event");
        }
        let notices = reducer
            .state()
            .history
            .iter()
            .filter_map(|entry| match entry {
                PublicHistoryEntry::Notice {
                    metadata, detail, ..
                } => Some((metadata.session_id.clone(), detail.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notices,
            vec![(
                "session-3".to_owned(),
                NoticeDetail::ContextCleared {
                    plan_file_path: Some("/workspace/plan.md".to_owned()),
                },
            )],
            "only the clearing publishes a notice, and it carries the new session"
        );
    }

    /// The subagent detail names the session a client may open, which the
    /// delegation result is the first thing to report.
    #[test]
    fn a_subagent_effect_publishes_the_child_session_it_ran() {
        let mut reducer = ProjectionReducer::new("session-1");
        for envelope in [
            event(
                1,
                EngineEvent::UserMessage {
                    content: "delegate".to_owned(),
                },
            ),
            event(
                2,
                EngineEvent::ToolCall {
                    call_id: "call-1".to_owned(),
                    name: "task".to_owned(),
                    arguments: r#"{"task":"audit","agent":"explore"}"#.to_owned(),
                },
            ),
            event(
                3,
                EngineEvent::ToolResult {
                    call_id: "call-1".to_owned(),
                    content: "done".to_owned(),
                    typed_result: json!({
                        "parentSessionId": "session-1",
                        "childSessionId": "session-child",
                        "publicSessionId": "session-child",
                        "status": "completed",
                        "result": "done",
                    }),
                    display: Value::Null,
                    duration_ms: 1,
                    is_error: false,
                    cancelled: false,
                },
            ),
        ] {
            reducer.apply(&envelope).expect("valid lifecycle event");
        }
        let Some(PublicHistoryEntry::Effect { detail, .. }) = reducer
            .state()
            .history
            .iter()
            .find(|entry| matches!(entry, PublicHistoryEntry::Effect { .. }))
        else {
            panic!("the delegation publishes an effect entry");
        };
        assert_eq!(detail.kind, ToolEffectKind::Subagent);
        assert_eq!(detail.child_session_id.as_deref(), Some("session-child"));
    }

    #[test]
    fn streaming_entries_seal_as_a_contiguous_tail() {
        let mut reducer = ProjectionReducer::new("session-1");
        for (id, emitted) in [
            (
                1,
                EngineEvent::UserMessage {
                    content: "go".to_owned(),
                },
            ),
            (
                2,
                EngineEvent::ModelText {
                    text: "thinking".to_owned(),
                },
            ),
            (
                3,
                EngineEvent::ToolCall {
                    call_id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ),
            (
                4,
                EngineEvent::ToolCall {
                    call_id: "call-2".to_owned(),
                    name: "read".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ),
        ] {
            reducer.apply(&event(id, emitted)).expect("valid event");
        }

        // Only the newest entry may still be generating: sealing walks back from
        // the tail and must not leave an earlier entry in progress.
        let statuses = reducer
            .state()
            .history
            .iter()
            .map(PublicHistoryEntry::is_completed)
            .collect::<Vec<_>>();
        assert_eq!(statuses, vec![true, true, true, false]);
    }

    #[test]
    fn public_projection_omits_private_reasoning_signature() {
        let private = ModelMessage::Assistant {
            content: "answer".to_owned(),
            reasoning: Some("private chain".to_owned()),
            reasoning_signature: Some("provider-signature".to_owned()),
            reasoning_state: vec!["provider-state".to_owned()],
            tool_calls: Vec::new(),
        };
        let encoded_private = serde_json::to_string(&private).expect("private message serializes");
        assert!(encoded_private.contains("provider-signature"));

        let mut reducer = ProjectionReducer::new("session-1");
        reducer
            .apply(&event(
                1,
                EngineEvent::UserMessage {
                    content: "question".to_owned(),
                },
            ))
            .expect("turn starts");
        reducer
            .apply(&event(
                2,
                EngineEvent::ModelReasoning {
                    text: "summary".to_owned(),
                },
            ))
            .expect("reasoning projects");
        let public = serde_json::to_string(reducer.state()).expect("public state serializes");
        assert!(!public.contains("provider-signature"));
    }
}
