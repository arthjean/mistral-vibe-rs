//! One live session: its canonical service, what it is currently doing, and
//! the cancellation every waiter observes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::task::AbortHandle;
use vibe_app_server::client::{HeadlessService, TurnDriver};
use vibe_app_server::experiments::SessionExperiments;

use crate::client_tools::UpdateBarrier;
use crate::protocol::AcpError;

/// What a session is currently doing. Reserving has no turn to interrupt yet,
/// which is why cancellation only latches a flag there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum ActivePhase {
    #[default]
    Idle,
    Reserving,
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
    /// The last version of every public history entry the client was told
    /// about, which a revision is projected against.
    entries: Mutex<BTreeMap<String, Value>>,
    /// Work spawned for this session, stopped when it closes.
    tasks: Mutex<Vec<AbortHandle>>,
    /// What the session is titled, what its first message reads, and the
    /// label the client was last told, which a title update is measured
    /// against.
    title: Mutex<DisplayTitle>,
    /// What orders a delegated tool request behind the updates before it.
    pub(crate) barrier: UpdateBarrier,
}

#[derive(Debug, Default)]
struct DisplayTitle {
    title: Option<String>,
    preview: Option<String>,
    shown: Option<String>,
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
            entries: Mutex::new(BTreeMap::new()),
            barrier: UpdateBarrier::default(),
            tasks: Mutex::new(Vec::new()),
            title: Mutex::new(DisplayTitle::default()),
            experiments: None,
        })
    }

    /// The identity the app server knows the session by, which is the one
    /// the client addresses.
    pub(crate) fn canonical_id(&self) -> String {
        self.session_id.clone()
    }

    /// Records `entry` as the version the client now holds, and returns the
    /// one it replaces.
    pub(crate) fn remember_entry(&self, entry: &Value) -> Option<Value> {
        // An effect is identified by its tool call, which a later turn may
        // reuse, so the turn is part of the key.
        let id = entry.get("id")?.as_str()?;
        let turn = entry
            .get("turnId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.entries
            .lock()
            .ok()?
            .insert(format!("{turn}\u{0}{id}"), entry.clone())
    }

    /// The last version of entry `id` of `turn` the client was told about.
    pub(crate) fn remembered_entry(&self, turn: &str, id: &str) -> Option<Value> {
        self.entries
            .lock()
            .ok()?
            .get(&format!("{turn}\u{0}{id}"))
            .cloned()
    }

    /// Records a title the client set, as both the title and the label it
    /// was last told.
    pub(crate) fn set_display_title(&self, value: &str) {
        if let Ok(mut title) = self.title.lock() {
            title.title = Some(value.to_owned());
            title.shown = Some(value.to_owned());
        }
    }

    /// Records the title and preview the session already has, which is what
    /// the client was told when it was opened.
    pub(crate) fn seed_display_title(&self, session: &Value) {
        let text = |key: &str| {
            session
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        };
        if let Ok(mut title) = self.title.lock() {
            title.title = text("title");
            title.preview = text("preview");
            title.shown = title.title.clone().or_else(|| title.preview.clone());
        }
    }

    /// Folds `entry` into the session's label, and returns the label when it
    /// changed. Reference `session_updates_for_event` on `SessionUpdated`: a
    /// title notice names the session, and its first operator message
    /// previews it until one does.
    pub(crate) fn observe_display_title(&self, entry: &Value) -> Option<String> {
        let mut title = self.title.lock().ok()?;
        match entry.get("type").and_then(Value::as_str) {
            Some("notice")
                if entry.pointer("/detail/kind").and_then(Value::as_str)
                    == Some("session_title_updated") =>
            {
                title.title = entry
                    .pointer("/detail/title")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(ToOwned::to_owned);
            }
            Some("message")
                if title.preview.is_none()
                    && entry.get("role").and_then(Value::as_str) == Some("user")
                    && entry.get("source").and_then(Value::as_str) != Some("harness") =>
            {
                let text = entry
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.is_empty() {
                    title.preview = Some(text.chars().take(160).collect());
                }
            }
            _ => return None,
        }
        let shown = title.title.clone().or_else(|| title.preview.clone());
        if shown == title.shown {
            return None;
        }
        title.shown.clone_from(&shown);
        shown
    }

    /// Keeps `handle` so closing the session stops the work behind it.
    pub(crate) fn track(&self, handle: AbortHandle) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|task| !task.is_finished());
            tasks.push(handle);
        }
    }

    /// Stops every task spawned for this session.
    pub(crate) fn abort_tasks(&self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
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

    /// Claims the session for new work, or reports the conflict.
    ///
    /// Clearing the cancellation is part of the claim and happens under the
    /// phase lock, so a cancel racing the end of the previous claim cannot
    /// latch onto the next one.
    pub(crate) fn begin(&self, phase: ActivePhase) -> Result<(), AcpError> {
        let mut active = self.active.lock().map_err(|_| AcpError::StatePoisoned)?;
        if *active != ActivePhase::Idle {
            return Err(AcpError::Internal(format!(
                "session `{}` is already running a prompt",
                self.session_id
            )));
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
            ActivePhase::Idle | ActivePhase::Reserving => None,
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
}
