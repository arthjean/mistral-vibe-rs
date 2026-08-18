//! The speech transport, driven end to end with no device and no gateway.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::tui::narrator::{NarratorEffect, NarratorManager, NarratorState};
use crate::tui::runtime::interactive_test_runtime;
use crate::tui::state::TuiState;

use super::super::player::{DecodedAudio, Playback};
use super::*;

const FIXTURE_SUMMARY: &str = "wrote the parser";

/// The published view a document declaring one speech surface produces.
fn speech_view(model: Value, provider: Value) -> Value {
    json!({"speech": {"model": model, "provider": provider}})
}

fn speech_settings(api_base: &str) -> SpeechSettings {
    SpeechSettings {
        model: "fixture-speech-model".to_owned(),
        voice: "fixture-voice".to_owned(),
        response_format: "wav".to_owned(),
        api_base: api_base.to_owned(),
        api_key_env_var: String::new(),
    }
}

/// A one-second-silence WAV, which is what a speech response carries.
fn fixture_wav() -> Vec<u8> {
    let samples: Vec<i16> = (0..64).map(|index| index * 100).collect();
    let mut data = Vec::new();
    for sample in &samples {
        data.extend_from_slice(&sample.to_le_bytes());
    }
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(b"fmt ");
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
    let mut container = Vec::new();
    container.extend_from_slice(b"RIFF");
    container.extend_from_slice(&u32::try_from(body.len()).unwrap_or_default().to_le_bytes());
    container.extend_from_slice(&body);
    container
}

// --------------------------------------------------------------------------
// The scripted device and the scripted transport
// --------------------------------------------------------------------------

struct DropFlag {
    dropped: Arc<AtomicBool>,
    _finisher: Arc<watch::Sender<bool>>,
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

struct ScriptedOutput {
    error: Option<PlaybackError>,
    completes: bool,
    starts: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
    played: Mutex<Vec<DecodedAudio>>,
}

impl ScriptedOutput {
    fn playing() -> Arc<Self> {
        Arc::new(Self {
            error: None,
            completes: true,
            starts: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicBool::new(false)),
            played: Mutex::new(Vec::new()),
        })
    }

    /// A device that accepts the buffer and never reaches its end, so a test can
    /// cancel or overlap a playback that is still running.
    fn holding() -> Arc<Self> {
        Arc::new(Self {
            completes: false,
            ..Self::playing_inner()
        })
    }

    fn failing(error: PlaybackError) -> Arc<Self> {
        Arc::new(Self {
            error: Some(error),
            ..Self::playing_inner()
        })
    }

    fn playing_inner() -> Self {
        Self {
            error: None,
            completes: true,
            starts: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicBool::new(false)),
            played: Mutex::new(Vec::new()),
        }
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::Acquire)
    }
}

impl AudioOutput for ScriptedOutput {
    fn start(&self, audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        self.starts.fetch_add(1, Ordering::AcqRel);
        if let Some(error) = self.error.clone() {
            return Err(error);
        }
        self.played
            .lock()
            .expect("scripted playback log")
            .push(audio);
        let (sender, receiver) = watch::channel(self.completes);
        let sender = Arc::new(sender);
        Ok(Playback::new(
            receiver,
            Box::new(DropFlag {
                dropped: Arc::clone(&self.dropped),
                _finisher: sender,
            }),
        ))
    }
}

struct ScriptedClient {
    payload: Result<Vec<u8>, String>,
    spoken: Mutex<Vec<String>>,
    gate: Option<Arc<Notify>>,
}

impl ScriptedClient {
    fn answering(payload: Result<Vec<u8>, String>) -> Arc<Self> {
        Arc::new(Self {
            payload,
            spoken: Mutex::new(Vec::new()),
            gate: None,
        })
    }

    fn gated(gate: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            payload: Ok(fixture_wav()),
            spoken: Mutex::new(Vec::new()),
            gate: Some(gate),
        })
    }

    fn spoken(&self) -> Vec<String> {
        self.spoken.lock().expect("scripted speech log").clone()
    }
}

impl SpeechClient for ScriptedClient {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a> {
        self.spoken
            .lock()
            .expect("scripted speech log")
            .push(text.to_owned());
        Box::pin(async move {
            if let Some(gate) = self.gate.as_ref() {
                gate.notified().await;
            }
            self.payload.clone()
        })
    }
}

async fn next_event(manager: &mut SpeechManager) -> SpeechEvent {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(event) = manager.try_next_event() {
                return event;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a scripted speech event")
}

// --------------------------------------------------------------------------
// US-211: the request
// --------------------------------------------------------------------------

/// The provider's endpoint, the model, the voice and the output format all come
/// from the configured entry rather than from a constant.
#[test]
fn the_request_is_built_from_the_configured_entry() {
    let settings = SpeechSettings::from_config_view(&speech_view(
        json!({
            "name": "fixture-gateway-model",
            "voice": "fixture-gateway-voice",
            "responseFormat": "mp3",
        }),
        json!({"apiBase": "https://speech.fixture.invalid:9443", "apiKeyEnvVar": "FIXTURE_TOKEN"}),
    ))
    .expect("the configured surface resolves");
    assert_eq!(settings.api_key_env_var, "FIXTURE_TOKEN");
    let request = SpeechRequest::resolve(&settings, FIXTURE_SUMMARY).expect("a request");
    assert_eq!(
        request.endpoint.as_str(),
        "https://speech.fixture.invalid:9443/v1/audio/speech"
    );
    assert_eq!(
        request.body,
        json!({
            "model": "fixture-gateway-model",
            "input": FIXTURE_SUMMARY,
            "voice_id": "fixture-gateway-voice",
            "response_format": "mp3",
            "stream": false,
        })
    );
}

/// A gateway served below a path prefix keeps it.
#[test]
fn a_provider_path_prefix_is_kept_under_the_speech_path() {
    let request = SpeechRequest::resolve(
        &speech_settings("https://gateway.fixture.invalid/audio/"),
        FIXTURE_SUMMARY,
    )
    .expect("a request");
    assert_eq!(
        request.endpoint.as_str(),
        "https://gateway.fixture.invalid/audio/v1/audio/speech"
    );
}

#[test]
fn an_endpoint_that_is_not_a_url_is_reported_rather_than_posted_to() {
    let error = SpeechRequest::resolve(&speech_settings("not a url"), FIXTURE_SUMMARY)
        .expect_err("an unusable endpoint is reported");
    assert!(error.contains("invalid"), "{error}");
    let error = SpeechRequest::resolve(&speech_settings("wss://gateway.fixture.invalid"), "")
        .expect_err("an unsupported scheme is reported");
    assert!(error.contains("wss"), "{error}");
}

/// A document declaring no speech model is what leaves the reference with a null
/// client, so the reason names the two keys the operator has to write.
#[test]
fn a_configuration_declaring_no_speech_model_resolves_to_an_error() {
    let error = SpeechSettings::from_config_view(&speech_view(
        json!({"name": "", "voice": "", "responseFormat": "wav"}),
        json!({"apiBase": "https://api.mistral.ai", "apiKeyEnvVar": ""}),
    ))
    .expect_err("an empty model list resolves to nothing");
    assert!(error.contains("tts_models"), "{error}");
    assert!(error.contains("active_tts_model"), "{error}");
}

/// What one captured request carried.
struct CapturedRequest {
    line: String,
    authorization: Option<String>,
    body: Value,
}

/// A gateway that answers one speech request and reports what it received.
async fn one_shot_gateway(status: &str, response: String) -> (String, JoinHandle<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("the bound address")
    );
    let status = status.to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        let mut received = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = socket.read(&mut buffer).await.expect("a request");
            if read == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&received).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or_default();
                if body.len() >= length {
                    let captured = CapturedRequest {
                        line: head.lines().next().unwrap_or_default().to_owned(),
                        authorization: head
                            .lines()
                            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                            .map(|line| line["authorization:".len()..].trim().to_owned()),
                        body: serde_json::from_str(body).unwrap_or(Value::Null),
                    };
                    let payload = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: \
                         {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    );
                    socket
                        .write_all(payload.as_bytes())
                        .await
                        .expect("the response is written");
                    socket.flush().await.expect("the response is flushed");
                    return captured;
                }
            }
        }
        panic!("the gateway received no complete request");
    });
    (base, server)
}

/// The whole request path, from the resolved configuration to the bytes the
/// player is handed: the summary is posted to the configured endpoint with the
/// configured voice and format, and the response payload is base64-decoded.
#[tokio::test]
async fn a_summary_is_posted_to_the_configured_endpoint_and_decoded() {
    let audio = fixture_wav();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&audio);
    let (base, server) =
        one_shot_gateway("200 OK", json!({"audio_data": encoded}).to_string()).await;
    let mut settings = speech_settings(&base);
    settings.response_format = "wav".to_owned();
    let client = HttpSpeechClient::new(settings, "fixture-credential".to_owned())
        .expect("the transport starts");

    let spoken = client.speak(FIXTURE_SUMMARY).await.expect("audio bytes");
    assert_eq!(spoken, audio, "the payload is decoded before playback");

    let captured = server.await.expect("the gateway");
    assert_eq!(captured.line, "POST /v1/audio/speech HTTP/1.1");
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer fixture-credential")
    );
    assert_eq!(
        captured.body,
        json!({
            "model": "fixture-speech-model",
            "input": FIXTURE_SUMMARY,
            "voice_id": "fixture-voice",
            "response_format": "wav",
            "stream": false,
        })
    );
}

/// A refused request is reported without the endpoint or the credential.
#[tokio::test]
async fn a_refused_request_is_reported_by_status() {
    let (base, server) = one_shot_gateway("503 Service Unavailable", "{}".to_owned()).await;
    let client = HttpSpeechClient::new(speech_settings(&base), "fixture-credential".to_owned())
        .expect("the transport starts");
    let error = client
        .speak(FIXTURE_SUMMARY)
        .await
        .expect_err("a refused request fails");
    let _ = server.await;
    assert!(error.contains("503"), "{error}");
    assert!(!error.contains("fixture-credential"), "{error}");
}

/// A configuration that resolves to no client answers where the summary was
/// spoken, so the narrator settles instead of waiting, and nothing is posted.
#[tokio::test]
async fn an_unresolved_configuration_settles_the_generation_without_posting() {
    let mut manager = SpeechManager::production(
        &json!({}),
        "",
        std::path::Path::new("/nonexistent-vibe-home"),
    );
    assert!(!manager.available(), "no client is built");
    manager.speak(3, FIXTURE_SUMMARY.to_owned());
    let event = next_event(&mut manager).await;
    let SpeechEvent::Finished {
        generation: 3,
        error: Some(error),
    } = event
    else {
        panic!("the missing configuration is reported: {event:?}");
    };
    assert!(error.contains("tts_models"), "{error}");
}

// --------------------------------------------------------------------------
// US-212: the playback
// --------------------------------------------------------------------------

/// The decoded buffer reaches the device and the exhausted buffer settles the
/// generation that asked for it.
#[tokio::test]
async fn decoded_audio_is_played_and_its_completion_settles_the_turn() {
    let client = ScriptedClient::answering(Ok(fixture_wav()));
    let output = ScriptedOutput::playing();
    let mut manager = SpeechManager::scripted(client.clone(), output.clone());
    manager.speak(1, FIXTURE_SUMMARY.to_owned());

    assert_eq!(
        next_event(&mut manager).await,
        SpeechEvent::PlaybackStarted { generation: 1 }
    );
    assert_eq!(
        next_event(&mut manager).await,
        SpeechEvent::Finished {
            generation: 1,
            error: None,
        }
    );
    assert_eq!(client.spoken(), vec![FIXTURE_SUMMARY.to_owned()]);
    let played = output.played.lock().expect("the playback log");
    assert_eq!(played.len(), 1);
    assert_eq!(played[0].sample_rate, 16_000);
    assert_eq!(played[0].channels, 1);
    assert_eq!(played[0].samples.len(), 64);
    assert!(
        output.dropped.load(Ordering::Acquire),
        "the stream is closed once the buffer is exhausted"
    );
}

/// A payload that is not a supported container never reaches the device.
#[tokio::test]
async fn an_undecodable_payload_is_reported_and_opens_no_device() {
    let output = ScriptedOutput::playing();
    let mut manager = SpeechManager::scripted(
        ScriptedClient::answering(Ok(b"ID3\x04fixture-mp3".to_vec())),
        output.clone(),
    );
    manager.speak(2, FIXTURE_SUMMARY.to_owned());
    let event = next_event(&mut manager).await;
    let SpeechEvent::Finished {
        generation: 2,
        error: Some(error),
    } = event
    else {
        panic!("the undecodable payload is reported: {event:?}");
    };
    assert!(error.contains("decoded"), "{error}");
    assert_eq!(output.starts(), 0, "no device was opened");
}

/// A host with no output device says so and the turn stays successful.
#[tokio::test]
async fn an_absent_output_device_is_reported_once_and_settles_the_turn() {
    let output = ScriptedOutput::failing(PlaybackError::NoOutputDevice(
        "the host names none".to_owned(),
    ));
    let mut manager =
        SpeechManager::scripted(ScriptedClient::answering(Ok(fixture_wav())), output.clone());
    manager.speak(1, FIXTURE_SUMMARY.to_owned());
    let event = next_event(&mut manager).await;
    let SpeechEvent::Finished {
        generation: 1,
        error: Some(error),
    } = event
    else {
        panic!("the absent device is reported: {event:?}");
    };
    assert!(error.contains("No audio output device"), "{error}");

    // The same fact on the next turn reaches the operator only once, which is
    // what the TUI's own notice gate answers with, and the turn that asked for
    // the summary returns to idle rather than staying in the machine.
    let mut state = TuiState::new("speech-notice");
    state.narrator = NarratorManager::new(true, true);
    state.narrator.on_turn_start("write the parser");
    state.narrator.on_turn_end().expect("a summary");
    crate::tui::narration::apply_speech_event(
        SpeechEvent::Finished {
            generation: 1,
            error: Some(error.clone()),
        },
        &mut state,
    );
    crate::tui::narration::apply_speech_event(
        SpeechEvent::Finished {
            generation: 2,
            error: Some(error),
        },
        &mut state,
    );
    assert_eq!(
        state.diagnostics().count(),
        1,
        "the operator is told once per session, not once per turn"
    );
    assert_eq!(
        state.narrator.state(),
        NarratorState::Idle,
        "a failure settles the generation that asked for the summary"
    );
}

/// Reference `AlreadyPlayingError`: a second playback is refused and the running
/// one keeps its stream.
#[tokio::test]
async fn a_second_playback_is_rejected_while_one_runs() {
    let output = ScriptedOutput::holding();
    let mut manager =
        SpeechManager::scripted(ScriptedClient::answering(Ok(fixture_wav())), output.clone());
    manager.speak(1, FIXTURE_SUMMARY.to_owned());
    assert_eq!(
        next_event(&mut manager).await,
        SpeechEvent::PlaybackStarted { generation: 1 }
    );

    manager.speak(2, "a second summary".to_owned());
    let event = next_event(&mut manager).await;
    let SpeechEvent::Finished {
        generation: 2,
        error: Some(error),
    } = event
    else {
        panic!("the second request is refused: {event:?}");
    };
    assert!(error.contains("already speaking"), "{error}");
    assert_eq!(output.starts(), 1, "the running stream was left alone");
    assert!(!output.dropped.load(Ordering::Acquire));
}

/// Reference `cancel`: playback stops before the state machine returns to idle,
/// and the superseded generation answers nothing.
#[tokio::test]
async fn cancelling_stops_playback_and_answers_nothing() {
    let output = ScriptedOutput::holding();
    let mut manager =
        SpeechManager::scripted(ScriptedClient::answering(Ok(fixture_wav())), output.clone());
    manager.speak(1, FIXTURE_SUMMARY.to_owned());
    assert_eq!(
        next_event(&mut manager).await,
        SpeechEvent::PlaybackStarted { generation: 1 }
    );

    manager.stop();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !output.dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the stream is closed on cancellation");
    manager.shutdown().await;
    assert_eq!(
        manager.try_next_event(),
        None,
        "a cancelled generation settles nothing"
    );
}

/// A summary whose turn was superseded while the request was in flight plays
/// nothing at all.
#[tokio::test]
async fn a_superseded_request_never_reaches_the_device() {
    let gate = Arc::new(Notify::new());
    let output = ScriptedOutput::playing();
    let mut manager = SpeechManager::scripted(ScriptedClient::gated(gate.clone()), output.clone());
    manager.speak(1, FIXTURE_SUMMARY.to_owned());
    manager.stop();
    gate.notify_waiters();
    manager.shutdown().await;
    assert_eq!(output.starts(), 0, "the late answer opened no device");
    assert_eq!(manager.try_next_event(), None);
}

// --------------------------------------------------------------------------
// US-213: the narrator
// --------------------------------------------------------------------------

/// The effect the state machine emits reaches the transport, and the answers the
/// transport sends drive the same machine through speaking and back to idle.
#[tokio::test]
async fn the_speak_effect_reaches_the_transport_and_drives_the_state_machine() {
    let mut runtime = interactive_test_runtime("speech-wiring");
    let client = ScriptedClient::answering(Ok(fixture_wav()));
    runtime.speech = SpeechManager::scripted(client.clone(), ScriptedOutput::playing());
    let mut state = TuiState::new("speech-wiring");
    state.narrator = NarratorManager::new(true, true);
    state.narrator.on_turn_start("write the parser");
    state.narrator.on_assistant_text("done");
    let Some(NarratorEffect::Summarize { generation, .. }) = state.narrator.on_turn_end() else {
        panic!("an enabled narrator summarizes");
    };
    let Some(effect) = state
        .narrator
        .apply_summary(generation, Some(FIXTURE_SUMMARY.to_owned()))
    else {
        panic!("a summary speaks");
    };

    crate::tui::apply_narrator_effect(effect, &mut runtime, &mut state);
    assert_eq!(
        next_event(&mut runtime.speech).await,
        SpeechEvent::PlaybackStarted { generation }
    );
    crate::tui::narration::apply_speech_event(
        SpeechEvent::PlaybackStarted { generation },
        &mut state,
    );
    assert_eq!(state.narrator.state(), NarratorState::Speaking);
    assert_eq!(client.spoken(), vec![FIXTURE_SUMMARY.to_owned()]);

    let finished = next_event(&mut runtime.speech).await;
    assert_eq!(
        finished,
        SpeechEvent::Finished {
            generation,
            error: None,
        }
    );
    crate::tui::narration::apply_speech_event(finished, &mut state);
    assert_eq!(state.narrator.state(), NarratorState::Idle);
    assert_eq!(
        state.diagnostics().count(),
        0,
        "a spoken summary reports nothing"
    );
}

/// A late answer whose generation has been superseded settles nothing.
#[test]
fn a_late_answer_from_a_superseded_generation_is_discarded() {
    let mut state = TuiState::new("speech-late");
    state.narrator = NarratorManager::new(true, true);
    state.narrator.on_turn_start("first");
    state.narrator.on_turn_end().expect("a first summary");
    state.narrator.cancel();
    state.narrator.on_turn_start("second");
    state.narrator.on_turn_end().expect("a second summary");

    crate::tui::narration::apply_speech_event(
        SpeechEvent::PlaybackStarted { generation: 1 },
        &mut state,
    );
    assert_eq!(
        state.narrator.state(),
        NarratorState::Summarizing,
        "a stale playback never enters the speaking state"
    );
    crate::tui::narration::apply_speech_event(
        SpeechEvent::Finished {
            generation: 1,
            error: None,
        },
        &mut state,
    );
    assert_eq!(
        state.narrator.state(),
        NarratorState::Summarizing,
        "a stale completion settles nothing"
    );
}

/// Reference `NarratorManager.sync`: the client is rebuilt from the
/// configuration as it stands, so an edit reaches the next turn and a
/// configuration that stops resolving takes the speaking state with it.
#[test]
fn a_configuration_change_is_read_again_into_the_next_turn() {
    let mut manager = SpeechManager::production(
        &json!({}),
        "",
        std::path::Path::new("/nonexistent-vibe-home"),
    );
    assert!(!manager.available());
    manager.resync(&speech_view(
        json!({
            "name": "fixture-second-speech-model",
            "voice": "fixture-second-voice",
            "responseFormat": "wav",
        }),
        json!({"apiBase": "https://gateway.fixture.invalid", "apiKeyEnvVar": ""}),
    ));
    assert!(manager.available(), "the edited surface resolves");
    manager.resync(&json!({}));
    assert!(
        !manager.available(),
        "a configuration that stops resolving is read again too"
    );
}
