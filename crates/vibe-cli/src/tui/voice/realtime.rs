//! Realtime transcription websocket protocol.

use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::{WebSocketStream, connect_async};
use url::Url;

use crate::tui::chat_input::InputEvent;

use super::recorder::AudioMessage;

const DEFAULT_MODEL: &str = "voxtral-mini-transcribe-realtime-2602";
const DEFAULT_SAMPLE_RATE: u32 = 16_000;
const TARGET_STREAMING_DELAY_MS: u32 = 500;
pub(super) const START_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT_VALUE: &str = concat!("mistral-vibe-rs/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub(super) struct VoiceConfig {
    pub(super) endpoint: Url,
    pub(super) requested_sample_rate: u32,
    pub(super) target_streaming_delay_ms: u32,
}

impl VoiceConfig {
    pub(super) fn from_api_base(api_base: &str) -> Result<Self, String> {
        let mut endpoint =
            Url::parse(api_base).map_err(|error| format!("Voice API URL is invalid: {error}"))?;
        let websocket_scheme = match endpoint.scheme() {
            "https" | "wss" => "wss",
            "http" | "ws" => "ws",
            scheme => return Err(format!("Voice API URL scheme `{scheme}` is unsupported")),
        };
        endpoint
            .set_scheme(websocket_scheme)
            .map_err(|()| "Voice API URL scheme could not be set".to_owned())?;
        endpoint.set_path("/v1/audio/transcriptions/realtime");
        endpoint.set_query(None);
        endpoint
            .query_pairs_mut()
            .append_pair("model", DEFAULT_MODEL);
        Ok(Self {
            endpoint,
            requested_sample_rate: DEFAULT_SAMPLE_RATE,
            target_streaming_delay_ms: TARGET_STREAMING_DELAY_MS,
        })
    }
}

pub(super) async fn run_transcription(
    mut audio: mpsc::Receiver<AudioMessage>,
    credential: &SecretString,
    config: &VoiceConfig,
    generation: u64,
    updates: &mpsc::Sender<InputEvent>,
) -> Result<(), String> {
    let mut request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|error| format!("Transcription request is invalid: {error}"))?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", credential.expose_secret()))
        .map_err(|_| "Transcription credential is invalid".to_owned())?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    request
        .headers_mut()
        .insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    let (mut websocket, _) = tokio::time::timeout(START_TIMEOUT, connect_async(request))
        .await
        .map_err(|_| "Transcription connection timed out".to_owned())?
        .map_err(|error| format!("Transcription connection failed: {error}"))?;
    wait_for_session(&mut websocket).await?;
    websocket
        .send(Message::Text(
            session_update(
                config.requested_sample_rate,
                config.target_streaming_delay_ms,
            )
            .into(),
        ))
        .await
        .map_err(|error| format!("Transcription session setup failed: {error}"))?;
    let (mut sink, mut source) = websocket.split();
    let mut ended = false;
    let mut drain = Box::pin(tokio::time::sleep(DRAIN_TIMEOUT));

    loop {
        tokio::select! {
            frame = audio.recv(), if !ended => match frame {
                Some(AudioMessage::Chunk(chunk)) => {
                    let payload = json!({
                        "type": "input_audio.append",
                        "audio": base64::engine::general_purpose::STANDARD.encode(chunk),
                    });
                    sink.send(Message::Text(payload.to_string().into()))
                        .await
                        .map_err(|error| format!("Transcription audio send failed: {error}"))?;
                }
                Some(AudioMessage::Failure(error)) => return Err(error),
                None => {
                    sink.send(Message::Text(json!({"type": "input_audio.flush"}).to_string().into()))
                        .await
                        .map_err(|error| format!("Transcription flush failed: {error}"))?;
                    sink.send(Message::Text(json!({"type": "input_audio.end"}).to_string().into()))
                        .await
                        .map_err(|error| format!("Transcription end failed: {error}"))?;
                    ended = true;
                    drain.as_mut().reset(tokio::time::Instant::now() + DRAIN_TIMEOUT);
                }
            },
            message = source.next() => {
                let Some(message) = message else {
                    return Err("Transcription connection closed before completion".to_owned());
                };
                let message = message.map_err(|error| format!("Transcription stream failed: {error}"))?;
                match realtime_event(&message)? {
                    RealtimeEvent::TextDelta(text) => {
                        let _ = updates.send(InputEvent::VoiceTranscriptDelta { text, generation }).await;
                    }
                    RealtimeEvent::Done => return Ok(()),
                    RealtimeEvent::Error(error) => return Err(error),
                    RealtimeEvent::Ping(payload) => {
                        sink.send(Message::Pong(payload))
                            .await
                            .map_err(|error| format!("Transcription keepalive failed: {error}"))?;
                    }
                    RealtimeEvent::Other => {}
                }
            }
            _ = &mut drain, if ended => return Err("Transcription timed out".to_owned()),
        }
    }
}

type RealtimeSocket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn wait_for_session(websocket: &mut RealtimeSocket) -> Result<(), String> {
    tokio::time::timeout(START_TIMEOUT, async {
        loop {
            let message = websocket
                .next()
                .await
                .ok_or_else(|| "Transcription connection closed during setup".to_owned())?
                .map_err(|error| format!("Transcription setup failed: {error}"))?;
            let value = message_json(&message)?;
            match value.get("type").and_then(Value::as_str) {
                Some("session.created") => return Ok(()),
                Some("error") => return Err(realtime_error(&value)),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "Transcription session setup timed out".to_owned())?
}

pub(super) fn session_update(sample_rate: u32, target_streaming_delay_ms: u32) -> String {
    json!({
        "type": "session.update",
        "session": {
            "audio_format": {
                "encoding": "pcm_s16le",
                "sample_rate": sample_rate,
            },
            "target_streaming_delay_ms": target_streaming_delay_ms,
        },
    })
    .to_string()
}

enum RealtimeEvent {
    TextDelta(String),
    Done,
    Error(String),
    Ping(tokio_tungstenite::tungstenite::Bytes),
    Other,
}

fn realtime_event(message: &Message) -> Result<RealtimeEvent, String> {
    if let Message::Ping(payload) = message {
        return Ok(RealtimeEvent::Ping(payload.clone()));
    }
    let value = message_json(message)?;
    Ok(match value.get("type").and_then(Value::as_str) {
        Some("transcription.text.delta") => RealtimeEvent::TextDelta(
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        Some("transcription.done") => RealtimeEvent::Done,
        Some("error") => RealtimeEvent::Error(realtime_error(&value)),
        _ => RealtimeEvent::Other,
    })
}

pub(super) fn message_json(message: &Message) -> Result<Value, String> {
    match message {
        Message::Text(text) => serde_json::from_str(text.as_ref()),
        Message::Binary(bytes) => serde_json::from_slice(bytes),
        Message::Close(_) => return Err("Transcription connection closed".to_owned()),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(Value::Null),
    }
    .map_err(|error| format!("Transcription event is invalid: {error}"))
}

fn realtime_error(value: &Value) -> String {
    value
        .pointer("/error/message/detail")
        .or_else(|| value.pointer("/error/message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Realtime transcription failed")
        .to_owned()
}
