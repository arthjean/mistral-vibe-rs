//! What a turn emits, what a client reads, and what the model reads.
//!
//! An [`EngineEvent`] is one thing a turn did. [`ProjectionReducer`] folds the
//! events into the public history a client reads (`public`), and the turn
//! keeps the transcript the model reads as [`ModelMessage`]s (`message`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::compaction::{CompactionFailureReason, CompactionStatus};

mod message;
mod public;
mod reduce;

use reduce::reduce_event;

pub mod detail;

pub use message::{
    ManualShellRecord, ModelMessage, ModelToolCall, NO_RESPONSE_TOOL_RESULT, PersistedToolResult,
    fill_missing_tool_responses,
};
pub use public::{
    EffectApproval, PublicCallbackState, PublicContentBlock, PublicEffectState,
    PublicEntryGenerationStatus, PublicEntryMetadata, PublicError, PublicHistoryEntry,
    PublicMessageRole, PublicMessageSource, PublicNoticeLevel, PublicTurn, PublicTurnStatus,
    PublicTurnStopReason, TurnErrorCode, canonical_user_resource, prompt_with_resources,
};

pub use detail::{
    ApprovalDecision, ApprovalDecisionType, CallbackDetail, CallbackOutput, EffectCallDisplay,
    EffectDetail, EffectResultDisplay, HookNotice, HookScope, HookSeverity, NoticeDetail,
    QuestionChoice, RemoteSettlement, RemoteToolKind, RemoteToolOrigin, TodoEffectPriority,
    TodoEffectStatus, ToolEffectKind, UserAnswer, UserQuestion, UserQuestionRequest,
    UserQuestionResult,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub session_id: String,
    #[serde(skip)]
    pub turn_id: Option<String>,
    #[serde(skip)]
    pub emitted_at: u64,
    /// Where the session sat when the event was emitted, which is what a file
    /// path in a tool header is displayed against. Reference `AgentLoop` binds
    /// its session's directory to every presentation it computes; this port
    /// computes presentation wherever an event is reduced, so the directory
    /// travels with the event rather than living in one reducer. [`None`]
    /// displays against the process directory, as the reference does when no
    /// session is bound.
    #[serde(skip)]
    pub working_directory: Option<PathBuf>,
    pub event_id: u64,
    #[serde(flatten)]
    pub event: EngineEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    UserMessage {
        content: String,
        /// The identifier the client gave the message, which the entry takes
        /// as its own. Reference `UserMessageEvent.message_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        /// The images and resources the operator attached, which the entry
        /// shows after the text. Reference `project_message_content`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PublicContentBlock>,
        /// What the client asked the entry to be shown as. Reference
        /// `UserMessageEvent.user_display_content`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_display_content: Option<Value>,
    },
    UserSteer {
        content: String,
        /// The identity the steered message is saved under, which its entry
        /// takes as well.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    ContextInjected {
        content: String,
        as_message: bool,
    },
    ModelText {
        text: String,
        /// The identifier of the assistant message this text belongs to,
        /// which the entry the first chunk opens takes as its own. Reference
        /// `LLMMessage.message_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    ModelReasoning {
        text: String,
        /// Reference `LLMMessage.reasoning_message_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    /// A call the model named, before its arguments are read.
    ToolCallAnnounced {
        call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<RemoteToolOrigin>,
    },
    /// A call naming no tool this turn can run, opened only to be settled by
    /// the failed result that follows. Reference `FailedToolCall`
    /// (`vibe/core/llm/format.py`).
    ToolCallUnresolved {
        call_id: String,
        name: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
        /// The remote the session proxies this call to, absent for a tool it
        /// implements itself. The registry is the only holder of the name the
        /// server published, and the projection that renders the call has no
        /// access to it, so the event carries it across.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<RemoteToolOrigin>,
    },
    ToolStream {
        call_id: String,
        chunk: String,
    },
    /// A steer the turn took has joined the transcript, the skill and files
    /// it named injected behind it. Reference `turn/steer` answers here and
    /// then bumps the session, as for any accepted user activity.
    SteerApplied,
    /// A model step ran to completion, its calls answered and the transcript
    /// saved. Reference `_conversation_loop` schedules the background title
    /// here; `turn_completing` is whether the step ended on the model's
    /// answer rather than on a tool result.
    StepCompleted {
        turn_completing: bool,
    },
    /// A call still waiting on its permission gate when its turn ended. The
    /// reference rejects the pending request once the turn's closing
    /// accounting is published (`_finalize_turn` in
    /// `vibe/app_server/_turns.py`), so the call fails after it and no client
    /// reads its result: the turn's finalization closes the effect.
    ToolCallAbandoned {
        call_id: String,
    },
    /// The session a running delegation opened, named on its call's effect.
    ToolChildSession {
        call_id: String,
        child_session_id: String,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        typed_result: Value,
        /// The document a client renders, or null when the tool published no
        /// second projection and the typed result is what reaches the UI.
        #[serde(default)]
        projected_result: Value,
        #[serde(default)]
        display: Value,
        #[serde(default)]
        duration_ms: u64,
        is_error: bool,
        #[serde(default)]
        cancelled: bool,
        /// The call never ran: its approval was refused. Reference
        /// `ToolResultEvent.skipped`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        skipped: bool,
        /// How the permission gate settled the call. Reference
        /// `ToolResultEvent.decision`, `approval_type` and `approval_source`,
        /// each absent when the gate did not reach it.
        #[serde(default, skip_serializing_if = "EffectApproval::is_unset")]
        approval: EffectApproval,
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
    /// One step of a hook run. Reference `HookEvent`, which the app-server
    /// projects as a notice.
    Hook {
        event: crate::hooks::HookEvent,
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
    /// The session moved to another agent profile under its running turn,
    /// which the next request already runs with. Reference
    /// `AgentProfileChangedEvent`.
    AgentChanged {
        agent_name: String,
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
        /// The part of `input_tokens` served from the provider's cache.
        #[serde(default)]
        cached_tokens: u64,
    },
    /// A provider request the backend is retrying. Like [`EngineEvent::Stats`]
    /// it carries no history: a client renders it as a transient state, not as
    /// a transcript entry.
    Retrying {
        /// Reference `RetryCategory`: `rate_limited`, `server_error`,
        /// `timed_out`, `connection` or `unknown`.
        category: String,
        /// The raw cause, `HTTP 503` or the transport failure's name.
        detail: String,
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
    /// What every telemetry event of the turn reports about it, published as
    /// the turn opens so the calls it makes before its first request, a
    /// mentioned file's read among them, report it too. Reference
    /// `_handle_tool_response` reads the same three off the agent loop.
    TurnOpened {
        model: String,
        agent_profile: String,
        #[serde(default)]
        message_id: Option<String>,
    },
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
        /// The first request of a user turn is the main call; every request
        /// after it, a summarization included, is secondary. Reference
        /// `_is_user_prompt_call`.
        #[serde(default)]
        call_type: crate::telemetry::TelemetryCallType,
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

    /// Refuses `event` unless a turn is running or waiting on a callback.
    fn require_active(self, event: &'static str) -> Result<(), ProjectionError> {
        if self.is_active() {
            Ok(())
        } else {
            Err(ProjectionError::IllegalTransition { from: self, event })
        }
    }

    /// Refuses `event` unless the lifecycle is one of `allowed`.
    fn require_one_of(self, allowed: &[Self], event: &'static str) -> Result<(), ProjectionError> {
        if allowed.contains(&self) {
            Ok(())
        } else {
            Err(ProjectionError::IllegalTransition { from: self, event })
        }
    }

    /// Refuses a lifecycle event that moves to `to` from a state the turn
    /// cannot leave that way.
    fn transition_to(self, to: Self) -> Result<(), ProjectionError> {
        let valid = matches!(
            (self, to),
            (Self::Idle, Self::Running)
                | (Self::Running, Self::WaitingCallback)
                | (Self::WaitingCallback, Self::Running)
                | (
                    Self::Running | Self::WaitingCallback,
                    Self::Completed | Self::Failed | Self::Cancelled
                )
                | (Self::Completed, Self::Running)
        );
        if valid {
            Ok(())
        } else {
            Err(ProjectionError::IllegalTransition {
                from: self,
                event: "lifecycle",
            })
        }
    }
}

/// The status label reference `EventProjector.finalize` closes an effect
/// with when its turn ends before the call answers: `Turn interrupted` when
/// the operator stopped the turn, `Turn ended` otherwise.
#[must_use]
pub const fn unfinished_effect_reason(cancelled: bool) -> &'static str {
    if cancelled {
        "Turn interrupted"
    } else {
        "Turn ended"
    }
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
            envelope.working_directory.as_deref(),
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
mod events_tests;
