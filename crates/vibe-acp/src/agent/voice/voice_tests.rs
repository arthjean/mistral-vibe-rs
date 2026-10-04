//! The voice controller against scripted devices and endpoints: what each
//! call answers and which `_voice/*` notifications it raises, in order.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;
use vibe_voice::capture::{AudioStream, RecorderError};
use vibe_voice::playback::{AudioOutput, DecodedAudio, Playback, PlaybackError};
use vibe_voice::speech::{SpeechClient, SpeechFuture};
use vibe_voice::transcribe::{TranscribeEvent, TranscribeFuture};

use super::*;

struct FakeRecorder {
    stream: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl AudioRecorder for FakeRecorder {
    fn start(&self, _sample_rate: u32) -> Result<AudioStream, RecorderError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.stream.lock().expect("the stream lock") = Some(sender);
        Ok(receiver)
    }

    fn stop(&self) -> std::time::Duration {
        self.cancel();
        Duration::from_secs(1)
    }

    fn cancel(&self) {
        self.stream.lock().expect("the stream lock").take();
    }

    fn peak(&self) -> f32 {
        0.0
    }

    fn has_signal(&self) -> bool {
        true
    }
}

/// An endpoint that says `words`, then fails with `error` or finishes once the
/// recording has.
struct FakeClient {
    words: &'static [&'static str],
    error: Option<&'static str>,
}

impl TranscribeClient for FakeClient {
    fn transcribe(
        &self,
        mut audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture {
        let words = self.words;
        let error = self.error;
        Box::pin(async move {
            let _ = events.send(TranscribeEvent::SessionCreated {
                request_id: "req-1".to_owned(),
            });
            for word in words {
                let _ = events.send(TranscribeEvent::TextDelta((*word).to_owned()));
            }
            if let Some(error) = error {
                let _ = events.send(TranscribeEvent::Error(error.to_owned()));
                return;
            }
            while audio.recv().await.is_some() {}
            let _ = events.send(TranscribeEvent::Done);
        })
    }
}

struct WavClient;

impl SpeechClient for WavClient {
    fn speak<'a>(&'a self, _text: &'a str) -> SpeechFuture<'a> {
        Box::pin(async { Ok(fixture_wav()) })
    }
}

/// A device that plays until `finish` is sent.
struct HeldOutput {
    finish: watch::Sender<bool>,
}

impl AudioOutput for HeldOutput {
    fn start(&self, _audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        Ok(Playback::new(self.finish.subscribe(), Box::new(())))
    }
}

fn fixture_wav() -> Vec<u8> {
    let data: Vec<u8> = (0..64_i16).flat_map(i16::to_le_bytes).collect();
    let mut body = b"WAVEfmt ".to_vec();
    body.extend_from_slice(&16_u32.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&16_000_u32.to_le_bytes());
    body.extend_from_slice(&32_000_u32.to_le_bytes());
    body.extend_from_slice(&2_u16.to_le_bytes());
    body.extend_from_slice(&16_u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&u32::try_from(data.len()).unwrap_or_default().to_le_bytes());
    body.extend_from_slice(&data);
    let mut container = b"RIFF".to_vec();
    container.extend_from_slice(&u32::try_from(body.len()).unwrap_or_default().to_le_bytes());
    container.extend_from_slice(&body);
    container
}

struct Fixture {
    controller: VoiceController,
    notify: Notify,
    sent: Arc<Mutex<Vec<Notice>>>,
    recorded: Arc<Mutex<Vec<&'static str>>>,
    finish: watch::Sender<bool>,
}

impl Fixture {
    fn new(client: FakeClient, speech_available: bool) -> Self {
        let client: Arc<dyn TranscribeClient> = Arc::new(client);
        let (finish, _) = watch::channel(false);
        let output_finish = finish.clone();
        let backends = VoiceBackends {
            recorder: Arc::new(FakeRecorder {
                stream: Mutex::new(None),
            }),
            transcribe: Arc::new(move |_: &Value| Some(Arc::clone(&client))),
            speech: Arc::new(move |_: &Value| {
                let output = Arc::new(HeldOutput {
                    finish: output_finish.clone(),
                });
                if speech_available {
                    SpeechManager::scripted(Arc::new(WavClient), output)
                } else {
                    SpeechManager::with_output(
                        &json!({}),
                        output,
                        Arc::new(|_: &str| None),
                        no_metadata(),
                    )
                }
            }),
            credentials: Arc::new(|_: &str| None),
        };
        let sent = Arc::new(Mutex::new(Vec::new()));
        let notify: Notify = {
            let sent = Arc::clone(&sent);
            Arc::new(move |method: &str, params: Value| {
                let method = match method {
                    TRANSCRIPTION_DELTA_METHOD => TRANSCRIPTION_DELTA_METHOD,
                    NARRATION_PREP_METHOD => NARRATION_PREP_METHOD,
                    NARRATION_DONE_METHOD => NARRATION_DONE_METHOD,
                    _ => NARRATION_ERROR_METHOD,
                };
                sent.lock().expect("the sent log").push((method, params));
            })
        };
        Self {
            controller: VoiceController::new(backends),
            notify,
            sent,
            recorded: Arc::new(Mutex::new(Vec::new())),
            finish,
        }
    }

    fn session(&self, summary: Option<&'static str>) -> Option<VoiceSession> {
        let recorded = Arc::clone(&self.recorded);
        Some(VoiceSession {
            config: json!({"transcription": {
                "model": {"sampleRate": 16_000},
                "provider": {"apiKeyEnvVar": ""},
            }}),
            summarizer: Arc::new(move |_, _| {
                Box::pin(async move { summary.map(ToOwned::to_owned) })
            }),
            record: Arc::new(move |event| {
                recorded
                    .lock()
                    .expect("the telemetry log")
                    .push(event.event().event_name());
            }),
        })
    }

    /// Waits until `count` notifications were sent outside a response.
    async fn sent(&self, count: usize) -> Vec<Notice> {
        for _ in 0..400 {
            let sent = self.sent.lock().expect("the sent log").clone();
            if sent.len() >= count {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "only {:?} was sent",
            self.sent.lock().expect("the sent log")
        );
    }
}

fn done() -> Notice {
    (NARRATION_DONE_METHOD, json!({}))
}

fn prep() -> Notice {
    (NARRATION_PREP_METHOD, json!({}))
}

#[tokio::test]
async fn every_call_needs_a_session_but_stopping_and_cancelling() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: None,
        },
        true,
    );
    let refused = json!({"ok": false, "error": "No active session"});
    assert_eq!(
        fixture
            .controller
            .transcribe_start(None, &fixture.notify)
            .await,
        refused
    );
    assert_eq!(
        fixture
            .controller
            .narrate(None, &fixture.notify, String::new(), String::new())
            .await,
        (refused, Vec::new())
    );
    assert_eq!(
        fixture.controller.transcribe_stop().await,
        json!({"ok": true, "text": ""})
    );
    assert_eq!(
        fixture.controller.transcribe_cancel().await,
        json!({"ok": true})
    );
    assert_eq!(
        fixture.controller.narrate_cancel().await,
        (json!({"ok": true}), vec![done()])
    );
}

#[tokio::test]
async fn dictation_streams_its_text_and_refuses_a_second_start() {
    let fixture = Fixture::new(
        FakeClient {
            words: &["hello", " world"],
            error: None,
        },
        true,
    );
    assert_eq!(
        fixture
            .controller
            .transcribe_start(fixture.session(None), &fixture.notify)
            .await,
        json!({"ok": true})
    );
    assert_eq!(
        fixture
            .controller
            .transcribe_start(fixture.session(None), &fixture.notify)
            .await,
        json!({"ok": false, "error": "Transcription already in progress"})
    );
    assert_eq!(
        fixture.sent(2).await,
        [
            (TRANSCRIPTION_DELTA_METHOD, json!({"text": "hello"})),
            (TRANSCRIPTION_DELTA_METHOD, json!({"text": " world"})),
        ]
    );
    assert_eq!(
        fixture.controller.transcribe_stop().await,
        json!({"ok": true, "text": ""})
    );
    assert_eq!(
        *fixture.recorded.lock().expect("the telemetry log"),
        [
            "vibe.audio.transcription.start",
            "vibe.audio.transcription.done"
        ]
    );
}

#[tokio::test]
async fn a_failed_transcription_is_relayed_as_an_error_delta() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: Some("quota exceeded"),
        },
        true,
    );
    fixture
        .controller
        .transcribe_start(fixture.session(None), &fixture.notify)
        .await;
    assert_eq!(
        fixture.sent(1).await,
        [(
            TRANSCRIPTION_DELTA_METHOD,
            json!({"error": "quota exceeded"})
        )]
    );
}

/// Reference `_NarratorListener`: preparation once the summary is asked for,
/// done once a narration that spoke ends.
#[tokio::test]
async fn a_narration_prepares_speaks_and_ends_done() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: None,
        },
        true,
    );
    let (answer, notices) = fixture
        .controller
        .narrate(
            fixture.session(Some("Parser written.")),
            &fixture.notify,
            "write the parser".to_owned(),
            "done".to_owned(),
        )
        .await;
    assert_eq!(answer, json!({"ok": true}));
    assert_eq!(notices, [prep()]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fixture.sent.lock().expect("the sent log").is_empty());
    let _ = fixture.finish.send(true);
    assert_eq!(fixture.sent(1).await, [done()]);
}

#[tokio::test]
async fn a_narration_without_a_summary_failed_during_preparation() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: None,
        },
        true,
    );
    let (_, notices) = fixture
        .controller
        .narrate(
            fixture.session(None),
            &fixture.notify,
            "hi".to_owned(),
            String::new(),
        )
        .await;
    assert_eq!(notices, [prep()]);
    assert_eq!(
        fixture.sent(1).await,
        [(
            NARRATION_ERROR_METHOD,
            json!({"error": "Narration failed during preparation"})
        )]
    );
}

/// Reference `narrate_cancel`: the cancelled narration ends as done, and done
/// is sent once more.
#[tokio::test]
async fn cancelling_a_narration_ends_it_done_twice() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: None,
        },
        true,
    );
    fixture
        .controller
        .narrate(
            fixture.session(Some("Parser written.")),
            &fixture.notify,
            "write the parser".to_owned(),
            String::new(),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fixture.controller.narrate_cancel().await,
        (json!({"ok": true}), vec![done(), done()])
    );
}

/// Reference `narrate`: the narration still preparing is cancelled first,
/// and the listener reads that as a failed preparation.
#[tokio::test]
async fn a_narration_replacing_one_in_preparation_reports_it_failed() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: None,
        },
        true,
    );
    let pending: Summarizer = Arc::new(|_, _| Box::pin(std::future::pending()));
    let mut session = fixture.session(None).expect("a session");
    session.summarizer = pending;
    fixture
        .controller
        .narrate(
            Some(session),
            &fixture.notify,
            "first".to_owned(),
            String::new(),
        )
        .await;
    let (_, notices) = fixture
        .controller
        .narrate(
            fixture.session(Some("Second.")),
            &fixture.notify,
            "second".to_owned(),
            String::new(),
        )
        .await;
    assert_eq!(
        notices,
        [
            (
                NARRATION_ERROR_METHOD,
                json!({"error": "Narration failed during preparation"})
            ),
            prep(),
        ]
    );
}

/// Reference `tts_client is None`: a configuration with no speech model
/// never enters preparation, so nothing is sent.
#[tokio::test]
async fn no_speech_model_narrates_nothing() {
    let fixture = Fixture::new(
        FakeClient {
            words: &[],
            error: None,
        },
        false,
    );
    let (answer, notices) = fixture
        .controller
        .narrate(
            fixture.session(Some("Unused.")),
            &fixture.notify,
            "hi".to_owned(),
            String::new(),
        )
        .await;
    assert_eq!(answer, json!({"ok": true}));
    assert!(notices.is_empty());
}
