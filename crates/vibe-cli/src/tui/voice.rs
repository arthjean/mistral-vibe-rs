//! Voice input between the composer and the shared voice manager.
//!
//! Reference `LazyVoiceManager` (`vibe/cli/lazy_audio_managers.py`): the
//! manager, and with it its transcribe client, is built the first time voice
//! mode is on, from the configuration as it stands then, and kept afterward.
//! Every start reads the sample rate and the credential variable afresh. The
//! manager's callbacks are translated into the composer's event protocol,
//! tagged with the composer generation the recording was started under, so a
//! late event from a cancelled recording is recognized as stale.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use vibe_core::config::DotenvValues;
use vibe_core::telemetry::TelemetryRecord;
use vibe_voice::capture::{AudioRecorder, CpalRecorder};
use vibe_voice::identity::{MetadataGetter, audio_request_metadata};
use vibe_voice::settings::CredentialLookup;
use vibe_voice::transcribe::{TranscribeClient, client_from_view};
use vibe_voice::{StartRequest, TranscribeState, VoiceEvent};

use super::chat_input::{InputEffect, InputEvent};
use super::setup::PersistedCredentialStore;

mod state;

pub use state::VoicePhase;
pub(crate) use state::{VoiceCommand, VoiceState, VoiceUpdate, VoiceUpdateOutcome};
pub(crate) use vibe_voice::{SpeechEvent, SpeechManager};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Reference `PEAK_BLOCKS`: how many levels the recording indicator draws.
const PEAK_LEVELS: f32 = 8.0;

/// Builds the transcribe client from a configuration view, or answers `None`
/// when the view resolves to none, which every start then reports.
pub(super) type ClientFactory =
    Arc<dyn Fn(&Value) -> Option<Arc<dyn TranscribeClient>> + Send + Sync>;

/// Reference `resolve_api_key`: the environment first, then the stored
/// credential. The vibe home's dotenv stands in for the environment the
/// reference loads it into at startup.
pub(super) fn audio_credentials(vibe_home: &Path) -> CredentialLookup {
    let vibe_home = vibe_home.to_path_buf();
    Arc::new(move |name: &str| {
        DotenvValues::global(&vibe_home)
            .variable(name)
            .filter(|credential| !credential.is_empty())
            .or_else(|| {
                PersistedCredentialStore::new(vibe_core::config::global_env_file(&vibe_home))
                    .resolve(name)
            })
    })
}

/// Reference `build_audio_request_metadata` for the session `session_id`
/// holds when a request is made; the terminal runs no subagent session, so
/// there is no parent to report.
pub(super) fn audio_metadata(session_id: &Arc<std::sync::Mutex<String>>) -> MetadataGetter {
    let session_id = Arc::clone(session_id);
    Arc::new(move || {
        let session_id = session_id
            .lock()
            .map(|session_id| session_id.clone())
            .unwrap_or_default();
        audio_request_metadata(&session_id, None)
    })
}

pub(super) struct VoiceManager {
    enabled: bool,
    /// The configuration as last published, which a start reads from.
    view: Value,
    recorder: Arc<dyn AudioRecorder>,
    client_factory: ClientFactory,
    credentials: CredentialLookup,
    manager: Option<vibe_voice::VoiceManager>,
    events_tx: mpsc::UnboundedSender<VoiceEvent>,
    events_rx: mpsc::UnboundedReceiver<VoiceEvent>,
    replies_tx: mpsc::UnboundedSender<InputEvent>,
    replies_rx: mpsc::UnboundedReceiver<InputEvent>,
    /// Composer events read off the channel while telemetry was drained.
    pending: VecDeque<InputEvent>,
    /// The start that is opening the microphone; a start asked for meanwhile
    /// waits for it, so a recording cancelled while it opened never swallows
    /// the next one.
    starting: Option<JoinHandle<()>>,
    stopping: Vec<JoinHandle<()>>,
    telemetry: Vec<TelemetryRecord>,
}

impl VoiceManager {
    pub(super) fn production(
        config_view: &Value,
        credentials: CredentialLookup,
        metadata: MetadataGetter,
        enabled: bool,
    ) -> Self {
        Self::new(
            config_view,
            Arc::new(CpalRecorder::new()),
            {
                let credentials = Arc::clone(&credentials);
                Arc::new(move |view: &Value| {
                    client_from_view(view, &credentials, Arc::clone(&metadata))
                })
            },
            credentials,
            enabled,
        )
    }

    pub(super) fn new(
        config_view: &Value,
        recorder: Arc<dyn AudioRecorder>,
        client_factory: ClientFactory,
        credentials: CredentialLookup,
        enabled: bool,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (replies_tx, replies_rx) = mpsc::unbounded_channel();
        let mut manager = Self {
            enabled,
            view: config_view.clone(),
            recorder,
            client_factory,
            credentials,
            manager: None,
            events_tx,
            events_rx,
            replies_tx,
            replies_rx,
            pending: VecDeque::new(),
            starting: None,
            stopping: Vec::new(),
            telemetry: Vec::new(),
        };
        // Reference `LazyVoiceManager.__init__`.
        if enabled {
            manager.materialize();
        }
        manager
    }

    fn materialize(&mut self) -> vibe_voice::VoiceManager {
        if let Some(manager) = &self.manager {
            return manager.clone();
        }
        let manager = vibe_voice::VoiceManager::new(
            Arc::clone(&self.recorder),
            (self.client_factory)(&self.view),
            Arc::clone(&self.credentials),
            self.events_tx.clone(),
        );
        self.manager = Some(manager.clone());
        manager
    }

    /// Records the configuration as it stands now. A manager already built
    /// keeps its client, as the reference's does; a start reads its sample
    /// rate and credential variable from this view.
    pub(super) fn resync(&mut self, config_view: &Value) {
        self.view = config_view.clone();
    }

    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Reference `apply_enabled`.
    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if self.manager.is_none() && !enabled {
            return;
        }
        let manager = self.materialize();
        if !enabled {
            manager.cancel_recording();
        }
    }

    pub(super) fn apply_effects(&mut self, effects: &[InputEffect], generation: u64) {
        for effect in effects {
            match effect {
                InputEffect::RecordingStartRequested => self.start(generation),
                InputEffect::RecordingStopRequested => self.stop(),
                InputEffect::RecordingCancelRequested => {
                    if let Some(manager) = &self.manager {
                        manager.cancel_recording();
                    }
                }
                _ => {}
            }
        }
    }

    fn start(&mut self, generation: u64) {
        if !self.enabled {
            let _ = self.replies_tx.send(InputEvent::VoiceStartResolved {
                generation,
                error: Some("Voice mode is disabled".to_owned()),
            });
            return;
        }
        let manager = self.materialize();
        let request = StartRequest {
            tag: generation,
            sample_rate: self
                .view
                .pointer("/transcription/model/sampleRate")
                .and_then(Value::as_u64)
                .and_then(|rate| u32::try_from(rate).ok())
                .unwrap_or(16_000),
            api_key_env_var: self
                .view
                .pointer("/transcription/provider/apiKeyEnvVar")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let previous = self.starting.take();
        let replies = self.replies_tx.clone();
        self.starting = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if let Err(error) = manager.start_recording(request).await {
                let _ = replies.send(InputEvent::VoiceStartResolved {
                    generation,
                    error: Some(error.0),
                });
            }
        }));
    }

    fn stop(&mut self) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        self.stopping.retain(|task| !task.is_finished());
        self.stopping.push(tokio::spawn(async move {
            manager.stop_recording().await;
        }));
    }

    /// Reference `peak`, as the recording indicator draws it.
    pub(super) fn peak_level(&self) -> u8 {
        let peak = self
            .manager
            .as_ref()
            .map_or(0.0, vibe_voice::VoiceManager::peak);
        // Reference `min(int(peak * len(PEAK_BLOCKS)), len(PEAK_BLOCKS) - 1)`.
        (peak * PEAK_LEVELS).clamp(0.0, PEAK_LEVELS - 1.0) as u8
    }

    pub(super) fn try_next_event(&mut self) -> Option<InputEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        while let Ok(event) = self.events_rx.try_recv() {
            if let Some(event) = self.translate(event) {
                return Some(event);
            }
        }
        self.replies_rx.try_recv().ok()
    }

    /// The composer event a manager event stands for; telemetry is kept for
    /// [`Self::take_telemetry`] instead.
    fn translate(&mut self, event: VoiceEvent) -> Option<InputEvent> {
        Some(match event {
            VoiceEvent::Telemetry(record) => {
                self.telemetry.push(record);
                return None;
            }
            VoiceEvent::State { tag, state } => match state {
                TranscribeState::Recording => InputEvent::VoiceStartResolved {
                    generation: tag,
                    error: None,
                },
                TranscribeState::Flushing => InputEvent::VoiceStopResolved {
                    generation: tag,
                    error: None,
                },
                TranscribeState::Idle => InputEvent::VoiceDone { generation: tag },
            },
            VoiceEvent::Text { tag, text } => InputEvent::VoiceTranscriptDelta {
                text,
                generation: tag,
            },
            VoiceEvent::Error { tag, message } => InputEvent::VoiceError {
                generation: tag,
                message,
            },
            VoiceEvent::Notice { tag, message } => InputEvent::VoiceNotice {
                generation: tag,
                message,
            },
        })
    }

    /// The audio events produced since the last drain, in the order they
    /// fired. The caller hands them to the session's telemetry client, which
    /// is where `enable_telemetry` decides whether anything is sent.
    pub(crate) fn take_telemetry(&mut self) -> Vec<TelemetryRecord> {
        while let Ok(event) = self.events_rx.try_recv() {
            if let Some(event) = self.translate(event) {
                self.pending.push_back(event);
            }
        }
        std::mem::take(&mut self.telemetry)
    }

    /// Reference `close`.
    pub(super) async fn shutdown(&mut self) {
        if let Some(task) = self.starting.take() {
            task.abort();
        }
        for task in self.stopping.drain(..) {
            task.abort();
        }
        if let Some(manager) = &self.manager {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, manager.close()).await;
        }
    }
}

impl Drop for VoiceManager {
    fn drop(&mut self) {
        if let Some(task) = self.starting.take() {
            task.abort();
        }
        for task in self.stopping.drain(..) {
            task.abort();
        }
        if let Some(manager) = &self.manager {
            manager.cancel_recording();
        }
    }
}

#[cfg(test)]
#[path = "voice/tests.rs"]
mod tests;
