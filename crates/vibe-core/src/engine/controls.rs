//! What a client queues into a running turn, and how the turn takes it.
//!
//! A steer joins the transcript the moment it arrives, a model reply or a tool
//! round in flight included; every other control waits for the next cycle
//! boundary, where `apply_controls` drains the queue.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::ledger::TurnRecorder;
use super::{
    Compactor, CompletionProvider, ConversationEngine, EngineError, ToolExecutor, TranscriptSink,
    TurnStopReason,
};
use crate::events::{EngineEvent, ModelMessage, SessionHandoffCause};
use crate::provider::ProviderInput;

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

#[derive(Debug, Clone, PartialEq)]
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
    /// leaving the turn to continue from `continuation` alone, or from nothing
    /// but the system prompt when it is empty.
    ClearContext {
        continuation: String,
        plan_file_path: Option<String>,
    },
    /// Moves the turn to another agent profile at the next cycle boundary:
    /// the next request offers `tools`. Reference `AgentLoop.switch_agent`,
    /// which reloads the tool manager in the middle of a turn.
    SwitchAgent {
        agent_name: String,
        tools: Vec<crate::provider::ToolDefinition>,
        /// The profile as the session's record stores it.
        profile: serde_json::Value,
    },
}

/// What draining the control queue leaves the turn to do.
pub(super) enum ControlOutcome {
    /// The turn continues. `true` when the transcript was cleared and the
    /// session rotated, which the caller checkpoints before the next request.
    Continue(bool),
    Stop(TurnStopReason),
    /// A steer's mention read failed the turn with this message.
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct TurnControlHandle {
    queue: Arc<Mutex<VecDeque<TurnControl>>>,
    /// Wakes a turn that is waiting on its model or its tools, which takes a
    /// steer the moment it arrives.
    arrived: Arc<Notify>,
}

impl TurnControlHandle {
    pub fn send(&self, control: TurnControl) -> Result<(), EngineError> {
        self.queue
            .lock()
            .map_err(|_| EngineError::ControlStatePoisoned)?
            .push_back(control);
        self.arrived.notify_one();
        Ok(())
    }

    pub(super) async fn arrived(&self) {
        self.arrived.notified().await;
    }

    /// Takes the queued steers, leaving every other control for the next
    /// cycle boundary.
    pub(super) fn take_steers(&self) -> Result<Vec<(String, bool)>, EngineError> {
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| EngineError::ControlStatePoisoned)?;
        let mut steers = Vec::new();
        queue.retain(|control| match control {
            TurnControl::Steer {
                content,
                inject_invoked_skill,
            } => {
                steers.push((content.clone(), *inject_invoked_skill));
                false
            }
            _ => true,
        });
        Ok(steers)
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

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    /// Reference `inject_user_context(as_message=True)`, which `turn/steer`
    /// calls: the message joins the transcript the moment it arrives, a tool
    /// round in flight included, and with `inject_implicit` the skill it
    /// invokes and the files it mentions are injected right behind it. The
    /// answer is the message of a mention read that failed the turn.
    ///
    /// Boxed: the mentions run through a tool round, which is where a steer
    /// can arrive from.
    pub(super) fn apply_steer<'a>(
        &'a self,
        recorder: &'a mut TurnRecorder<'_>,
        messages: &'a mut Vec<ModelMessage>,
        content: String,
        inject_implicit: bool,
        cancellation: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, EngineError>> + Send + 'a>> {
        Box::pin(async move {
            let message_id = crate::session_id::uuid_v4();
            recorder.emit(EngineEvent::UserSteer {
                content: content.clone(),
                message_id: Some(message_id.clone()),
            })?;
            messages.push(ModelMessage::User {
                content: content.clone(),
                injected: false,
                message_id: Some(message_id),
                attachments: Vec::new(),
                manual_shell: None,
                compaction_boundary: false,
                input_text: None,
                user_display_content: None,
            });
            if inject_implicit {
                self.inject_invoked_skill(recorder, messages, &content)?;
                if let Some(failure) = self
                    .inject_mentioned_files(recorder, messages, &content, cancellation)
                    .await?
                {
                    return Ok(Some(failure));
                }
            }
            recorder.emit(EngineEvent::SteerApplied)?;
            Ok(None)
        })
    }

    /// Drains queued steering, context injection, callback resolutions and
    /// context clearings.
    pub(super) async fn apply_controls(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        input: &mut ProviderInput,
        controls: &TurnControlHandle,
        cancellation: &CancellationToken,
    ) -> Result<ControlOutcome, EngineError> {
        let mut cleared = false;
        for control in controls.drain()? {
            match control {
                TurnControl::Steer {
                    content,
                    inject_invoked_skill,
                } => {
                    if let Some(message) = self
                        .apply_steer(
                            recorder,
                            messages,
                            content,
                            inject_invoked_skill,
                            cancellation,
                        )
                        .await?
                    {
                        return Ok(ControlOutcome::Failed(message));
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
                    if !continuation.is_empty() {
                        messages.push(ModelMessage::injected_user(continuation));
                    }
                    recorder.emit(EngineEvent::SessionHandoff {
                        from_session_id,
                        to_session_id,
                        cause: SessionHandoffCause::ContextCleared { plan_file_path },
                    })?;
                    cleared = true;
                }
                TurnControl::SwitchAgent {
                    agent_name,
                    tools,
                    profile,
                } => {
                    input.tools = tools;
                    self.sink.record_agent(&profile);
                    recorder.emit(EngineEvent::AgentChanged { agent_name })?;
                }
            }
        }
        Ok(ControlOutcome::Continue(cleared))
    }
}

/// What a model call or a tool round needs to take a steer while it runs.
pub(super) struct Steering<'a> {
    pub(super) controls: &'a TurnControlHandle,
    pub(super) messages: &'a mut Vec<ModelMessage>,
}

/// Resolves when a steer may have arrived, and never without a handle.
pub(super) async fn steer_arrived(steering: Option<&Steering<'_>>) {
    match steering {
        Some(steering) => steering.controls.arrived().await,
        None => std::future::pending().await,
    }
}
