//! Voice input lifecycle and deterministic state boundary.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::chat_input::{InputEffect, InputEvent};

mod realtime;
mod recorder;
mod session;
mod state;

pub use state::VoicePhase;
pub(crate) use state::{VoiceCommand, VoiceState, VoiceUpdate, VoiceUpdateOutcome};

use realtime::VoiceConfig;
use session::ProductionVoiceSessionFactory;

const UPDATE_QUEUE_CAPACITY: usize = 128;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VoiceControl {
    Running,
    Stop,
    Cancel,
}

struct ActiveVoice {
    generation: u64,
    control: watch::Sender<VoiceControl>,
    task: JoinHandle<()>,
}

pub(super) trait VoiceSessionFactory: Send + Sync {
    fn spawn(
        &self,
        generation: u64,
        updates: mpsc::Sender<InputEvent>,
        control: watch::Receiver<VoiceControl>,
    ) -> JoinHandle<()>;
}

/// Owns generation-aware voice sessions without exposing device or network
/// work to the deterministic composer reducer.
pub(super) struct VoiceManager {
    enabled: bool,
    factory: Arc<dyn VoiceSessionFactory>,
    updates_tx: mpsc::Sender<InputEvent>,
    updates_rx: mpsc::Receiver<InputEvent>,
    active: Option<ActiveVoice>,
    retiring: Vec<JoinHandle<()>>,
    pending_start: Option<u64>,
}

impl VoiceManager {
    pub(super) fn production(
        credential: String,
        api_base: &str,
        enabled: bool,
    ) -> Result<Self, String> {
        let config = VoiceConfig::from_api_base(api_base)?;
        Ok(Self::new(
            Arc::new(ProductionVoiceSessionFactory::new(credential, config)),
            enabled,
        ))
    }

    fn new(factory: Arc<dyn VoiceSessionFactory>, enabled: bool) -> Self {
        let (updates_tx, updates_rx) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
        Self {
            enabled,
            factory,
            updates_tx,
            updates_rx,
            active: None,
            retiring: Vec::new(),
            pending_start: None,
        }
    }

    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.pending_start = None;
            self.cancel_active();
        }
    }

    pub(super) fn apply_effects(&mut self, effects: &[InputEffect], generation: u64) {
        self.reap_finished();
        for effect in effects {
            match effect {
                InputEffect::RecordingStartRequested => self.start(generation),
                InputEffect::RecordingStopRequested => self.control(VoiceControl::Stop),
                InputEffect::RecordingCancelRequested => self.cancel_active(),
                _ => {}
            }
        }
    }

    pub(super) fn try_next_event(&mut self) -> Option<InputEvent> {
        let event = self.updates_rx.try_recv().ok();
        if event
            .as_ref()
            .and_then(terminal_generation)
            .is_some_and(|generation| {
                self.active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
            })
        {
            self.retire_active();
        }
        self.reap_finished();
        event
    }

    pub(super) async fn shutdown(&mut self) {
        self.pending_start = None;
        self.cancel_active();
        for mut task in self.retiring.drain(..) {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }

    fn start(&mut self, generation: u64) {
        if !self.enabled {
            let _ = self.updates_tx.try_send(InputEvent::VoiceStartResolved {
                generation,
                error: Some("Voice mode is disabled".to_owned()),
            });
            return;
        }
        if self.active.is_some() {
            return;
        }
        if !self.retiring.is_empty() {
            self.pending_start = Some(generation);
            return;
        }
        self.start_now(generation);
    }

    fn start_now(&mut self, generation: u64) {
        let (control, receiver) = watch::channel(VoiceControl::Running);
        let task = self
            .factory
            .spawn(generation, self.updates_tx.clone(), receiver);
        self.active = Some(ActiveVoice {
            generation,
            control,
            task,
        });
    }

    fn control(&self, control: VoiceControl) {
        if let Some(active) = self.active.as_ref() {
            let _ = active.control.send(control);
        }
    }

    fn cancel_active(&mut self) {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            let _ = active.control.send(VoiceControl::Cancel);
            self.retiring.push(active.task);
        }
    }

    fn retire_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.retiring.push(active.task);
        }
    }

    fn reap_finished(&mut self) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.task.is_finished())
        {
            self.retire_active();
        }
        self.retiring.retain(|task| !task.is_finished());
        if self.active.is_none()
            && self.retiring.is_empty()
            && let Some(generation) = self.pending_start.take()
        {
            self.start_now(generation);
        }
    }
}

fn terminal_generation(event: &InputEvent) -> Option<u64> {
    match event {
        InputEvent::VoiceDone { generation }
        | InputEvent::VoiceStartResolved {
            generation,
            error: Some(_),
        }
        | InputEvent::VoiceStopResolved {
            generation,
            error: Some(_),
        } => Some(*generation),
        _ => None,
    }
}

impl Drop for VoiceManager {
    fn drop(&mut self) {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            let _ = active.control.send(VoiceControl::Cancel);
            active.task.abort();
        }
        for task in self.retiring.drain(..) {
            task.abort();
        }
    }
}

#[cfg(test)]
#[path = "voice/tests.rs"]
mod tests;
