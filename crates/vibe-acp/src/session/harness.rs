//! One live session: its canonical service, what it is currently doing, and
//! the cancellation every waiter observes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use vibe_app_server::client::{HeadlessService, TurnDriver};
use vibe_app_server::experiments::SessionExperiments;

use crate::protocol::AcpError;
use crate::session::settings::SessionSettings;

/// What a session is currently doing. Reserving and Builtin have no canonical
/// turn to interrupt yet, which is why cancellation only latches a flag there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum ActivePhase {
    #[default]
    Idle,
    Reserving,
    Builtin,
    Running(String),
}

pub(crate) struct AcpHarness<D>
where
    D: TurnDriver,
{
    pub(crate) service: tokio::sync::Mutex<HeadlessService<D>>,
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    /// This session's enrollment, held so the lookup it detached is cancelled
    /// before the session stops. Reference `AgentLoop` holds its experiments
    /// task for the same reason.
    pub(crate) experiments: Option<Arc<SessionExperiments>>,
    /// What the session is doing, and the lock that makes claiming work and
    /// requesting cancellation mutually exclusive.
    active: Mutex<ActivePhase>,
    /// Whether the current claim has been cancelled. A watch rather than a
    /// flag plus a notification: every waiter observes the same edge, and a
    /// waiter that arrives after the edge still sees the level.
    cancel: tokio::sync::watch::Sender<bool>,
    settings: Mutex<SessionSettings>,
    user_message_anchors: Mutex<BTreeMap<String, usize>>,
    next_id: AtomicU64,
}

impl<D> AcpHarness<D>
where
    D: TurnDriver,
{
    pub(crate) fn adopt(
        mut service: HeadlessService<D>,
        session_id: &str,
    ) -> Result<Self, AcpError> {
        let view = service.session(session_id)?;
        Ok(Self {
            service: tokio::sync::Mutex::new(service),
            session_id: session_id.to_owned(),
            cwd: view.working_directory,
            active: Mutex::new(ActivePhase::Idle),
            cancel: tokio::sync::watch::Sender::new(false),
            settings: Mutex::new(SessionSettings::from_intent(
                view.intent.mode.as_deref(),
                view.intent.thinking,
                view.intent.reasoning_effort.as_deref(),
            )),
            user_message_anchors: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            experiments: None,
        })
    }

    /// Attaches the enrollment this session resolves, and starts the lookup.
    ///
    /// Reference `start_initialize_experiments` detaches the lookup as soon as
    /// the session exists, so an editor session reports the same enrollment a
    /// terminal one does without waiting for it.
    pub(crate) fn resolving_experiments(mut self, experiments: Arc<SessionExperiments>) -> Self {
        experiments.start(&self.session_id);
        self.experiments = Some(experiments);
        self
    }

    pub(crate) fn settings(&self) -> Result<SessionSettings, AcpError> {
        Ok(*self.settings.lock().map_err(|_| AcpError::StatePoisoned)?)
    }

    pub(crate) fn update_settings(
        &self,
        update: impl FnOnce(&mut SessionSettings),
    ) -> Result<(), AcpError> {
        let mut settings = self.settings.lock().map_err(|_| AcpError::StatePoisoned)?;
        update(&mut settings);
        Ok(())
    }

    /// Claims the session for new work, or reports the conflict.
    ///
    /// Clearing the cancellation is part of the claim and happens under the
    /// phase lock, so a cancel racing the end of the previous claim cannot
    /// latch onto the next one.
    pub(crate) fn begin(&self, phase: ActivePhase) -> Result<(), AcpError> {
        let mut active = self.active.lock().map_err(|_| AcpError::StatePoisoned)?;
        if *active != ActivePhase::Idle {
            return Err(AcpError::SessionBusy(self.session_id.clone()));
        }
        *active = phase;
        self.cancel.send_replace(false);
        Ok(())
    }

    pub(crate) fn set_phase(&self, phase: ActivePhase) -> Result<(), AcpError> {
        *self.active.lock().map_err(|_| AcpError::StatePoisoned)? = phase;
        Ok(())
    }

    pub(crate) fn release(&self) -> Result<(), AcpError> {
        self.set_phase(ActivePhase::Idle)
    }

    pub(crate) fn phase(&self) -> Result<ActivePhase, AcpError> {
        Ok(self
            .active
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .clone())
    }

    /// Turn ID of a canonically reserved turn, which is the only phase where
    /// the driver has something to interrupt.
    pub(crate) fn running_turn_id(&self) -> Result<Option<String>, AcpError> {
        Ok(match self.phase()? {
            ActivePhase::Running(turn_id) => Some(turn_id),
            ActivePhase::Idle | ActivePhase::Reserving | ActivePhase::Builtin => None,
        })
    }

    /// Cancels whatever the session is doing and reports the phase the request
    /// observed, which is what tells the caller whether a canonical turn still
    /// needs interrupting. An idle session latches nothing: there is no claim
    /// to cancel, and latching would leak into the next one.
    pub(crate) fn request_cancel(&self) -> Result<ActivePhase, AcpError> {
        let active = self.active.lock().map_err(|_| AcpError::StatePoisoned)?;
        if *active != ActivePhase::Idle {
            self.cancel.send_replace(true);
        }
        Ok(active.clone())
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Resolves once the current claim is cancelled. Safe to await from more
    /// than one place at a time.
    pub(crate) async fn cancelled(&self) {
        let mut cancelled = self.cancel.subscribe();
        // The sender outlives every receiver it hands out, so the wait only
        // ends on the value this asks for.
        let _ = cancelled.wait_for(|cancelled| *cancelled).await;
    }

    /// Monotonic identifier for locally generated messages and tool calls, so
    /// two updates emitted in the same millisecond never collide.
    pub(crate) fn next_local_id(&self, prefix: &str) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{sequence}", self.session_id)
    }

    pub(crate) fn next_ephemeral_user_message_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{}:user:{sequence}", self.session_id)
    }

    pub(crate) fn record_user_message_anchor(
        &self,
        message_id: String,
        history_before: usize,
        history_after: &[Value],
        prompt: &str,
    ) -> Result<(), AcpError> {
        let anchor = history_after
            .iter()
            .enumerate()
            .skip(history_before)
            .rev()
            .find(|(_, entry)| {
                entry.get("role").and_then(Value::as_str) == Some("user")
                    && entry.get("content").and_then(Value::as_str) == Some(prompt)
            })
            .map(|(index, _)| index);
        if let Some(anchor) = anchor {
            self.user_message_anchors
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?
                .insert(message_id, anchor);
        }
        Ok(())
    }

    pub(crate) fn user_message_anchor(&self, message_id: &str) -> Result<Option<usize>, AcpError> {
        Ok(self
            .user_message_anchors
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .get(message_id)
            .copied())
    }
}
