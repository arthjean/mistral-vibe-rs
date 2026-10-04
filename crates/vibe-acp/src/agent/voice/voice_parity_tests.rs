//! Differential replay of the editor's voice extension.
//!
//! `scripts/parity/voice_behavior.py` drives the reference's `VoiceController`
//! (`vibe/acp/voice.py`) over the terminal client's managers, with the
//! recorder, the transcription client, the speech client, the player and the
//! turn summary scripted, and records what each call answered and which
//! notifications it raised. The `acpBridge` family of the voice corpus, which
//! `vibe-voice` owns, is replayed here against [`VoiceController`] over the
//! same scripts. A text the reference authors is held as its length and
//! SHA-256, so this port's is reduced the same way.
//!
//! The reference hands its notifications to `Client.ext_notification`, which
//! puts them on the wire with a leading underscore; this port's methods are
//! the wire names, so the reference's are compared with that underscore.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Map;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use vibe_core::parity::REFERENCE_COMMIT;
use vibe_voice::capture::{AudioStream, RecorderError};
use vibe_voice::playback::{AudioOutput, DecodedAudio, Playback, PlaybackError};
use vibe_voice::speech::{SpeechClient, SpeechFuture};
use vibe_voice::transcribe::{TranscribeEvent, TranscribeFuture};

use super::*;

/// The voice corpus, which the audio crate owns.
const CORPUS_RELATIVE: &str = "../vibe-voice/tests/voice/corpus.json";
/// The request identifier and the texts the oracle authored.
const REQUEST_ID: &str = "fixture-request";
const USER_MESSAGE: &str = "write the parser";
const ASSISTANT_TEXT: &str = "Parser written.";
/// How long a step is given to settle before it is observed, standing for the
/// oracle's loop turns.
const SETTLE: Duration = Duration::from_millis(40);
/// How long a scripted summary takes to answer. A summary is a request to the
/// app server, which answers after the call that asked for it has returned
/// and its own notifications have been sent.
const SUMMARY_LATENCY: Duration = Duration::from_millis(5);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    reference: Reference,
    acp_bridge: Vec<BridgeCase>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BridgeCase {
    case: String,
    session: bool,
    start_error: Option<String>,
    client: Vec<Value>,
    summaries: Vec<Value>,
    steps: Vec<Value>,
    summary_calls: Vec<Value>,
    spoken: Vec<String>,
}

fn prose(text: &str) -> Value {
    json!({
        "prose": text.chars().count(),
        "sha256": hex::encode(Sha256::digest(text.as_bytes())),
    })
}

/// A call's answer or a notification's parameters, with the error text
/// reduced to prose.
fn reduced(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        value["error"] = prose(error);
    }
    value
}

/// The oracle's scripted recorder: one queued block, or the refusal it names.
struct ScriptedRecorder {
    start_error: Option<RecorderError>,
    stream: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl AudioRecorder for ScriptedRecorder {
    fn start(&self, _sample_rate: u32) -> Result<AudioStream, RecorderError> {
        if let Some(error) = &self.start_error {
            return Err(error.clone());
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(vec![1, 0]).expect("the queued block");
        *self.stream.lock().expect("the stream slot") = Some(sender);
        Ok(receiver)
    }

    fn stop(&self) -> Duration {
        self.cancel();
        Duration::from_secs(1)
    }

    fn cancel(&self) {
        self.stream.lock().expect("the stream slot").take();
    }

    fn peak(&self) -> f32 {
        0.0
    }

    fn has_signal(&self) -> bool {
        true
    }
}

/// The oracle's scripted transcription client.
struct ScriptedTranscription(Vec<Value>);

impl TranscribeClient for ScriptedTranscription {
    fn transcribe(
        &self,
        mut audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture {
        let script = self.0.clone();
        Box::pin(async move {
            for step in script {
                let text = step
                    .get("text")
                    .or_else(|| step.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match step.get("step").and_then(Value::as_str) {
                    Some("created") => {
                        let _ = events.send(TranscribeEvent::SessionCreated {
                            request_id: REQUEST_ID.to_owned(),
                        });
                    }
                    Some("text") => {
                        let _ = events.send(TranscribeEvent::TextDelta(text));
                    }
                    Some("done") => {
                        let _ = events.send(TranscribeEvent::Done);
                    }
                    Some("error") => {
                        let _ = events.send(TranscribeEvent::Error(text));
                    }
                    Some("awaitEnd") => while audio.recv().await.is_some() {},
                    other => panic!("the replay scripts no `{other:?}` step"),
                }
            }
        })
    }
}

/// The oracle's scripted speech client, which records what it was asked to
/// say.
struct ScriptedSpeech {
    spoken: Mutex<Vec<String>>,
}

impl SpeechClient for ScriptedSpeech {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a> {
        self.spoken
            .lock()
            .expect("the spoken log")
            .push(text.to_owned());
        Box::pin(async { Ok(fixture_wav()) })
    }
}

/// The oracle's scripted player, which plays until told to finish.
struct ScriptedOutput {
    current: Mutex<Option<watch::Sender<bool>>>,
}

impl ScriptedOutput {
    fn finish(&self) {
        if let Some(current) = self.current.lock().expect("the playback slot").take() {
            let _ = current.send(true);
        }
    }
}

impl AudioOutput for ScriptedOutput {
    fn start(&self, _audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        let (finished_tx, finished_rx) = watch::channel(false);
        *self.current.lock().expect("the playback slot") = Some(finished_tx);
        Ok(Playback::new(finished_rx, Box::new(())))
    }
}

fn fixture_wav() -> Vec<u8> {
    let data: Vec<u8> = [0_i16, 1, 2, 3]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
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

fn start_error(name: Option<&str>) -> Option<RecorderError> {
    match name? {
        "NoAudioInputDeviceError" => Some(RecorderError::NoInputDevice),
        "AlreadyRecordingError" => Some(RecorderError::AlreadyRecording),
        other => panic!("the replay maps no `{other}` recorder error"),
    }
}

struct Observation {
    steps: Vec<Value>,
    summary_calls: Vec<Value>,
    spoken: Vec<String>,
}

async fn bridge_case(case: &BridgeCase) -> Observation {
    let script = case.client.clone();
    let speech = Arc::new(ScriptedSpeech {
        spoken: Mutex::new(Vec::new()),
    });
    let output = Arc::new(ScriptedOutput {
        current: Mutex::new(None),
    });
    let speech_client = Arc::clone(&speech);
    let speech_output = Arc::clone(&output);
    let backends = VoiceBackends {
        recorder: Arc::new(ScriptedRecorder {
            start_error: start_error(case.start_error.as_deref()),
            stream: Mutex::new(None),
        }),
        transcribe: Arc::new(move |_: &Value| {
            Some(Arc::new(ScriptedTranscription(script.clone())) as Arc<dyn TranscribeClient>)
        }),
        speech: Arc::new(move |_: &Value| {
            SpeechManager::scripted(
                Arc::clone(&speech_client) as Arc<dyn SpeechClient>,
                Arc::clone(&speech_output) as Arc<dyn AudioOutput>,
            )
        }),
        credentials: Arc::new(|_: &str| None),
    };
    let controller = VoiceController::new(backends);
    let sent = Arc::new(Mutex::new(Vec::<Notice>::new()));
    let sink = Arc::clone(&sent);
    let notify: Notify = Arc::new(move |method: &str, params: Value| {
        let method = match method {
            TRANSCRIPTION_DELTA_METHOD => TRANSCRIPTION_DELTA_METHOD,
            NARRATION_PREP_METHOD => NARRATION_PREP_METHOD,
            NARRATION_DONE_METHOD => NARRATION_DONE_METHOD,
            NARRATION_ERROR_METHOD => NARRATION_ERROR_METHOD,
            other => panic!("the bridge raised the unknown notification `{other}`"),
        };
        sink.lock().expect("the sent log").push((method, params));
    });
    let answers = Arc::new(Mutex::new(
        case.summaries.iter().cloned().collect::<VecDeque<_>>(),
    ));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let session = || {
        case.session.then(|| {
            let answers = Arc::clone(&answers);
            let calls = Arc::clone(&calls);
            let summarizer: Summarizer = Arc::new(move |user_message, assistant_text| {
                calls.lock().expect("the call log").push(json!({
                    "assistantText": assistant_text,
                    "error": null,
                    "messageId": null,
                    "userMessage": user_message,
                }));
                let answer = answers.lock().expect("the answers").pop_front();
                Box::pin(async move {
                    match answer {
                        Some(Value::Object(answer)) if answer.contains_key("hang") => {
                            std::future::pending::<()>().await;
                            None
                        }
                        Some(Value::String(summary)) => {
                            tokio::time::sleep(SUMMARY_LATENCY).await;
                            Some(summary)
                        }
                        _ => {
                            tokio::time::sleep(SUMMARY_LATENCY).await;
                            None
                        }
                    }
                })
            });
            VoiceSession {
                config: json!({"transcription": {
                    "model": {"sampleRate": 16_000},
                    "provider": {"apiKeyEnvVar": ""},
                }}),
                summarizer,
                record: Arc::new(|_: &TelemetryRecord| {}),
            }
        })
    };
    let mut steps = Vec::new();
    for step in &case.steps {
        let mut observed = Map::new();
        let mut notices = Vec::new();
        if step.get("finish").is_some() {
            observed.insert("finish".to_owned(), json!(true));
            output.finish();
        } else {
            let call = step["call"].as_str().unwrap_or_default();
            observed.insert("call".to_owned(), json!(call));
            let (result, raised) = match call {
                "transcribeStart" => (
                    controller.transcribe_start(session(), &notify).await,
                    Vec::new(),
                ),
                "transcribeStop" => (controller.transcribe_stop().await, Vec::new()),
                "transcribeCancel" => (controller.transcribe_cancel().await, Vec::new()),
                "narrate" => {
                    controller
                        .narrate(
                            session(),
                            &notify,
                            USER_MESSAGE.to_owned(),
                            ASSISTANT_TEXT.to_owned(),
                        )
                        .await
                }
                "narrateCancel" => controller.narrate_cancel().await,
                other => panic!("the replay calls no `{other}`"),
            };
            observed.insert("result".to_owned(), reduced(&result));
            // The call's own notifications follow its answer.
            notices.extend(raised);
        }
        tokio::time::sleep(SETTLE).await;
        notices.extend(sent.lock().expect("the sent log").drain(..));
        observed.insert(
            "notices".to_owned(),
            Value::Array(
                notices
                    .iter()
                    .map(|(method, params)| json!({"method": method, "params": reduced(params)}))
                    .collect(),
            ),
        );
        steps.push(Value::Object(observed));
    }
    controller.close().await;
    let summary_calls = calls.lock().expect("the call log").clone();
    let spoken = speech.spoken.lock().expect("the spoken log").clone();
    Observation {
        steps,
        summary_calls,
        spoken,
    }
}

/// The reference's step as this replay records its own: the call or the
/// playback ending, its answer, and the notifications under their wire names.
fn wire_step(step: &Value) -> Value {
    let mut step = step.clone();
    if let Some(notices) = step.get_mut("notices").and_then(Value::as_array_mut) {
        for notice in notices {
            if let Some(method) = notice["method"].as_str() {
                notice["method"] = json!(format!("_{method}"));
            }
        }
    }
    step
}

#[test]
fn the_voice_corpus_bridge_replays_against_this_port() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw).expect("the voice corpus parses");
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    assert!(
        corpus.acp_bridge.len() >= 10,
        "the bridge family holds {} cases",
        corpus.acp_bridge.len()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a test runtime");
    let mut divergences = Vec::new();
    let mut total = 0_usize;
    for case in &corpus.acp_bridge {
        let observed = runtime.block_on(bridge_case(case));
        let expected = case.steps.iter().map(wire_step).collect::<Vec<_>>();
        for (field, reference, port) in [
            ("steps", json!(expected), json!(observed.steps)),
            (
                "summaryCalls",
                json!(case.summary_calls),
                json!(observed.summary_calls),
            ),
            ("spoken", json!(case.spoken), json!(observed.spoken)),
        ] {
            total += 1;
            if reference != port {
                divergences.push(format!(
                    "acpBridge/{field}/{}: reference {reference}, port {port}",
                    case.case
                ));
            }
        }
    }
    println!(
        "voice: acpBridge {}/{total} conform (0 ledgered)",
        total - divergences.len()
    );
    assert!(
        divergences.is_empty(),
        "acpBridge diverges from the reference:\n{}",
        divergences.join("\n")
    );
}
