//! Dictation and narration for an editor.
//!
//! Reference `VoiceController` (`vibe/acp/voice.py`): a thin adapter over the
//! terminal client's voice and narrator managers, built the first time an
//! editor asks for either and kept for the life of the process. Every
//! callback the managers raise becomes a `_voice/*` notification: a
//! transcription delta or error, and the narration's preparation, completion
//! or failure as reference `_NarratorListener` reads them off the narrator's
//! state changes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use vibe_core::telemetry::TelemetryRecord;
use vibe_voice::capture::{AudioRecorder, CpalRecorder};
use vibe_voice::identity::no_metadata;
use vibe_voice::settings::{CredentialLookup, ambient_credentials};
use vibe_voice::transcribe::{TranscribeClient, client_from_view};
use vibe_voice::{
    NarratorEffect, NarratorManager, NarratorState, SpeechEvent, SpeechManager, StartRequest,
    TranscribeState, VoiceEvent, VoiceManager,
};

pub(crate) const TRANSCRIPTION_DELTA_METHOD: &str = "_voice/transcriptionDelta";
pub(crate) const NARRATION_PREP_METHOD: &str = "_voice/narrationPrep";
pub(crate) const NARRATION_DONE_METHOD: &str = "_voice/narrationDone";
pub(crate) const NARRATION_ERROR_METHOD: &str = "_voice/narrationError";

/// What a narration that never spoke reports.
const PREPARATION_FAILED: &str = "Narration failed during preparation";

/// A notification for the editor: its method and its parameters.
pub(crate) type Notice = (&'static str, Value);
/// Sends a notification to the editor.
pub(crate) type Notify = Arc<dyn Fn(&str, Value) + Send + Sync>;
pub(crate) type SummaryFuture = Pin<Box<dyn Future<Output = Option<String>> + Send>>;
/// Reference `NarrationResource.summarize` for the user message and the
/// assistant text.
pub(crate) type Summarizer = Arc<dyn Fn(String, String) -> SummaryFuture + Send + Sync>;
/// Reference `TelemetryResource.log` for the session the managers serve.
pub(crate) type Recorder = Arc<dyn Fn(&TelemetryRecord) + Send + Sync>;

/// What the managers are built against: the session's configuration as it
/// stands, with voice mode and narration forced on, its narration resource
/// and its telemetry.
pub(crate) struct VoiceSession {
    pub(crate) config: Value,
    pub(crate) summarizer: Summarizer,
    pub(crate) record: Recorder,
}

/// Builds the transcription client a configuration resolves to, if any.
pub type TranscribeFactory = Arc<dyn Fn(&Value) -> Option<Arc<dyn TranscribeClient>> + Send + Sync>;
/// Builds the speech transport a configuration resolves to.
pub type SpeechFactory = Arc<dyn Fn(&Value) -> SpeechManager + Send + Sync>;

/// The audio devices and remote clients the managers run on.
#[derive(Clone)]
pub struct VoiceBackends {
    pub recorder: Arc<dyn AudioRecorder>,
    pub transcribe: TranscribeFactory,
    pub speech: SpeechFactory,
    pub credentials: CredentialLookup,
}

impl VoiceBackends {
    /// The default microphone and output device, and the configured endpoints.
    /// The reference builds both clients without request metadata.
    #[must_use]
    pub fn production(vibe_home: &std::path::Path) -> Self {
        let credentials = ambient_credentials(vibe_home);
        let transcribe_credentials = Arc::clone(&credentials);
        let speech_credentials = Arc::clone(&credentials);
        Self {
            recorder: Arc::new(CpalRecorder::new()),
            transcribe: Arc::new(move |view: &Value| {
                client_from_view(view, &transcribe_credentials, no_metadata())
            }),
            speech: Arc::new(move |view: &Value| {
                SpeechManager::production(view, Arc::clone(&speech_credentials), no_metadata())
            }),
            credentials,
        }
    }
}

/// Reference `_NarratorListener`: which notification a narrator state change
/// stands for.
#[derive(Debug, Default)]
struct NarratorListener {
    last: NarratorState,
    reached_speaking: bool,
    canceling: bool,
}

impl NarratorListener {
    fn observe(&mut self, state: NarratorState) -> Option<Notice> {
        if state == self.last {
            return None;
        }
        self.last = state;
        match state {
            NarratorState::Summarizing => {
                self.reached_speaking = false;
                self.canceling = false;
                Some((NARRATION_PREP_METHOD, json!({})))
            }
            NarratorState::Speaking => {
                self.reached_speaking = true;
                None
            }
            NarratorState::Idle if self.canceling => {
                self.canceling = false;
                Some((NARRATION_DONE_METHOD, json!({})))
            }
            NarratorState::Idle if self.reached_speaking => {
                Some((NARRATION_DONE_METHOD, json!({})))
            }
            NarratorState::Idle => {
                Some((NARRATION_ERROR_METHOD, json!({"error": PREPARATION_FAILED})))
            }
        }
    }
}

enum Command {
    Narrate {
        user_message: String,
        assistant_text: String,
        reply: oneshot::Sender<Vec<Notice>>,
    },
    Cancel {
        reply: oneshot::Sender<Vec<Notice>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

struct Managers {
    voice: VoiceManager,
    narration: mpsc::UnboundedSender<Command>,
    tasks: Vec<JoinHandle<()>>,
}

/// Reference `VoiceController`.
pub struct VoiceController {
    backends: VoiceBackends,
    managers: tokio::sync::Mutex<Option<Managers>>,
}

impl VoiceController {
    #[must_use]
    pub fn new(backends: VoiceBackends) -> Self {
        Self {
            backends,
            managers: tokio::sync::Mutex::new(None),
        }
    }

    /// Reference `_ensure_managers`: built once, against the session that
    /// asked first.
    fn ensure<'a>(
        &self,
        managers: &'a mut Option<Managers>,
        session: &VoiceSession,
        notify: &Notify,
    ) -> &'a Managers {
        managers.get_or_insert_with(|| self.build(session, notify))
    }

    fn build(&self, session: &VoiceSession, notify: &Notify) -> Managers {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let voice = VoiceManager::new(
            Arc::clone(&self.backends.recorder),
            (self.backends.transcribe)(&session.config),
            Arc::clone(&self.backends.credentials),
            events_tx,
        );
        let speech = (self.backends.speech)(&session.config);
        let narrator = NarratorManager::new(true, speech.available());
        let (narration, commands) = mpsc::unbounded_channel();
        let tasks = vec![
            tokio::spawn(relay_voice(
                events_rx,
                Arc::clone(notify),
                Arc::clone(&session.record),
            )),
            tokio::spawn(run_narration(Narration {
                narrator,
                speech,
                listener: NarratorListener::default(),
                summarizer: Arc::clone(&session.summarizer),
                record: Arc::clone(&session.record),
                notify: Arc::clone(notify),
                summary: None,
                commands,
            })),
        ];
        Managers {
            voice,
            narration,
            tasks,
        }
    }

    /// Reference `transcribe_start`.
    pub(crate) async fn transcribe_start(
        &self,
        session: Option<VoiceSession>,
        notify: &Notify,
    ) -> Value {
        let Some(session) = session else {
            return no_session();
        };
        let voice = {
            let mut managers = self.managers.lock().await;
            self.ensure(&mut managers, &session, notify).voice.clone()
        };
        if voice.state() != TranscribeState::Idle || voice.is_starting() {
            return json!({"ok": false, "error": "Transcription already in progress"});
        }
        let request = StartRequest {
            tag: 0,
            sample_rate: session
                .config
                .pointer("/transcription/model/sampleRate")
                .and_then(Value::as_u64)
                .and_then(|rate| u32::try_from(rate).ok())
                .unwrap_or(16_000),
            api_key_env_var: session
                .config
                .pointer("/transcription/provider/apiKeyEnvVar")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        match voice.start_recording(request).await {
            Ok(()) => json!({"ok": true}),
            Err(error) => json!({"ok": false, "error": error.0}),
        }
    }

    /// Reference `transcribe_stop`: the transcription is drained, and its text
    /// has already reached the editor as deltas.
    pub(crate) async fn transcribe_stop(&self) -> Value {
        let voice = self.voice().await;
        if let Some(voice) = voice {
            voice.stop_recording().await;
        }
        json!({"ok": true, "text": ""})
    }

    /// Reference `transcribe_cancel`.
    pub(crate) async fn transcribe_cancel(&self) -> Value {
        if let Some(voice) = self.voice().await {
            voice.cancel_recording();
        }
        json!({"ok": true})
    }

    async fn voice(&self) -> Option<VoiceManager> {
        self.managers
            .lock()
            .await
            .as_ref()
            .map(|managers| managers.voice.clone())
    }

    /// Reference `narrate`: a running narration is cancelled, then the turn is
    /// summarized and spoken. The notifications the call raises itself are
    /// answered for the caller to send after its response.
    pub(crate) async fn narrate(
        &self,
        session: Option<VoiceSession>,
        notify: &Notify,
        user_message: String,
        assistant_text: String,
    ) -> (Value, Vec<Notice>) {
        let Some(session) = session else {
            return (no_session(), Vec::new());
        };
        let narration = {
            let mut managers = self.managers.lock().await;
            self.ensure(&mut managers, &session, notify)
                .narration
                .clone()
        };
        let (reply, answer) = oneshot::channel();
        let _ = narration.send(Command::Narrate {
            user_message,
            assistant_text,
            reply,
        });
        (json!({"ok": true}), answer.await.unwrap_or_default())
    }

    /// Reference `narrate_cancel`: a narration under way ends as done, and
    /// done is sent once more whatever was running, so the editor clears its
    /// narrating state.
    pub(crate) async fn narrate_cancel(&self) -> (Value, Vec<Notice>) {
        let narration = self
            .managers
            .lock()
            .await
            .as_ref()
            .map(|managers| managers.narration.clone());
        let mut notices = Vec::new();
        if let Some(narration) = narration {
            let (reply, answer) = oneshot::channel();
            let _ = narration.send(Command::Cancel { reply });
            notices = answer.await.unwrap_or_default();
        }
        notices.push((NARRATION_DONE_METHOD, json!({})));
        (json!({"ok": true}), notices)
    }

    /// Reference `close`.
    pub(crate) async fn close(&self) {
        let Some(managers) = self.managers.lock().await.take() else {
            return;
        };
        managers.voice.close().await;
        let (reply, answer) = oneshot::channel();
        if managers.narration.send(Command::Close { reply }).is_ok() {
            let _ = answer.await;
        }
        for task in managers.tasks {
            task.abort();
        }
    }
}

fn no_session() -> Value {
    json!({"ok": false, "error": "No active session"})
}

/// Reference `_VoiceListener`: text and errors reach the editor, the events
/// reach telemetry, and nothing else is relayed.
async fn relay_voice(
    mut events: mpsc::UnboundedReceiver<VoiceEvent>,
    notify: Notify,
    record: Recorder,
) {
    while let Some(event) = events.recv().await {
        match event {
            VoiceEvent::Text { text, .. } => {
                notify(TRANSCRIPTION_DELTA_METHOD, json!({"text": text}));
            }
            VoiceEvent::Error { message, .. } => {
                notify(TRANSCRIPTION_DELTA_METHOD, json!({"error": message}));
            }
            VoiceEvent::Telemetry(event) => record(&event),
            VoiceEvent::State { .. } | VoiceEvent::Notice { .. } => {}
        }
    }
}

/// The narrator and what it drives, owned by one task so a summary, a
/// playback and an editor's request are applied one at a time.
struct Narration {
    narrator: NarratorManager,
    speech: SpeechManager,
    listener: NarratorListener,
    summarizer: Summarizer,
    record: Recorder,
    notify: Notify,
    summary: Option<AbortHandle>,
    commands: mpsc::UnboundedReceiver<Command>,
}

impl Narration {
    /// Reference `NarratorManager.cancel`, which always lands in idle.
    fn cancel(&mut self, notices: &mut Vec<Notice>) {
        if let Some(NarratorEffect::Stop) = self.narrator.cancel() {
            self.speech.stop();
        }
        if let Some(summary) = self.summary.take() {
            summary.abort();
        }
        self.observe(notices);
    }

    fn observe(&mut self, notices: &mut Vec<Notice>) {
        notices.extend(self.listener.observe(self.narrator.state()));
        for event in self.narrator.take_telemetry() {
            (self.record)(&event);
        }
    }

    fn send(&self, notices: Vec<Notice>) {
        for (method, params) in notices {
            (self.notify)(method, params);
        }
    }
}

async fn run_narration(mut narration: Narration) {
    let (summaries_tx, mut summaries) = mpsc::unbounded_channel::<(u64, Option<String>)>();
    loop {
        tokio::select! {
            command = narration.commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Narrate { user_message, assistant_text, reply } => {
                        let mut notices = Vec::new();
                        narration.cancel(&mut notices);
                        narration.narrator.on_turn_start(&user_message);
                        narration.narrator.on_assistant_text(&assistant_text);
                        if let Some(NarratorEffect::Summarize {
                            generation,
                            user_message,
                            assistant_text,
                            ..
                        }) = narration.narrator.on_turn_end()
                        {
                            let summary = (narration.summarizer)(user_message, assistant_text);
                            let summaries = summaries_tx.clone();
                            let task = tokio::spawn(async move {
                                let _ = summaries.send((generation, summary.await));
                            });
                            narration.summary = Some(task.abort_handle());
                        }
                        narration.observe(&mut notices);
                        let _ = reply.send(notices);
                    }
                    Command::Cancel { reply } => {
                        narration.listener.canceling = true;
                        let mut notices = Vec::new();
                        narration.cancel(&mut notices);
                        let _ = reply.send(notices);
                    }
                    Command::Close { reply } => {
                        let mut notices = Vec::new();
                        narration.cancel(&mut notices);
                        narration.speech.shutdown().await;
                        let _ = reply.send(());
                        break;
                    }
                }
            }
            Some((generation, summary)) = summaries.recv() => {
                narration.summary = None;
                if let Some(NarratorEffect::Speak { generation, text }) =
                    narration.narrator.apply_summary(generation, summary)
                {
                    narration.speech.speak(generation, text);
                }
                let mut notices = Vec::new();
                narration.observe(&mut notices);
                narration.send(notices);
            }
            Some(event) = narration.speech.next_event() => {
                match event {
                    SpeechEvent::PlaybackStarted { generation } => {
                        narration.narrator.playback_started(generation);
                    }
                    SpeechEvent::Finished { generation, error: Some(failure) } => {
                        narration.narrator.fail(generation, failure.class);
                    }
                    SpeechEvent::Finished { generation, error: None } => {
                        narration.narrator.settle(generation);
                    }
                }
                let mut notices = Vec::new();
                narration.observe(&mut notices);
                narration.send(notices);
            }
        }
    }
}

#[cfg(test)]
mod voice_parity_tests;
#[cfg(test)]
mod voice_tests;
