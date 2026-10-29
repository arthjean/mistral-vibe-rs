//! The voice stack without audio hardware: an ALSA configuration of the
//! world's own whose default device captures a file and plays into a null
//! sink, a realtime transcription stand-in on the platform, and the speech
//! route's audio.
//!
//! The transcription stand-in speaks the protocol of the Mistral SDK's
//! realtime transcription (`mistralai/extra/realtime/transcription.py` in the
//! reference's environment): it opens the session, reads the client's
//! `session.update` and the audio, answers `input_audio.flush`, which a client
//! sends once its recording stopped, with the scripted text deltas, and
//! `input_audio.end` with `transcription.done`. Holding the text until the
//! recording stops lets a scenario cancel one before any text arrives. A
//! scripted error is sent instead, right after the session update.
//!
//! Both implementations reach ALSA (the port through CPAL, the reference
//! through miniaudio, which takes ALSA once no PulseAudio server answers), so
//! `ALSA_CONFIG_PATH` naming a self-contained file replaces every system
//! device, the PipeWire plugin included. The jail hides `/dev/snd`
//! ([`crate::jail`]), so nothing reaches a sound card even by name.

use std::io::Write as _;
use std::net::TcpStream;

use base64::Engine as _;
use serde_json::{Value, json};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

use crate::http::Request;
use crate::{DEFAULT_DEADLINE, scaled};

/// Which capture device a voice world's ALSA configuration declares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Capture {
    /// The default device captures [`CAPTURE_FILE`] and plays into a null
    /// sink.
    #[default]
    File,
    /// No device captures; the default device only plays into a null sink.
    Absent,
}

/// The raw 16-bit PCM the default device captures.
pub(crate) const CAPTURE_FILE: &str = "audio/capture.raw";
/// The ALSA configuration `ALSA_CONFIG_PATH` names.
pub(crate) const ALSA_CONFIG: &str = "audio/asound.conf";

/// The path both implementations open the realtime transcription on.
pub(crate) const REALTIME_PATH: &str = "/v1/audio/transcriptions/realtime";

/// The audio providers, models, and aliases a voice world's `config.toml`
/// declares, with `base` the platform's address.
pub(crate) fn config_tables(base: &str, key_variable: &str) -> String {
    let socket = base.replacen("http://", "ws://", 1);
    format!(
        "\n[[transcribe_providers]]\nname = \"e2e-audio\"\napi_base = \"{socket}\"\n\
         api_key_env_var = \"{key_variable}\"\n\n[[transcribe_models]]\n\
         name = \"e2e-transcribe-v1\"\nprovider = \"e2e-audio\"\nalias = \"e2e-transcribe\"\n\n\
         [[tts_providers]]\nname = \"e2e-audio\"\napi_base = \"{base}\"\n\
         api_key_env_var = \"{key_variable}\"\n\n[[tts_models]]\nname = \"e2e-tts-v1\"\n\
         provider = \"e2e-audio\"\nalias = \"e2e-tts\"\n"
    )
}

/// The top-level keys a voice world's `config.toml` sets.
pub(crate) const CONFIG_KEYS: &str =
    "active_transcribe_model = \"e2e-transcribe\"\nactive_tts_model = \"e2e-tts\"\n";

/// The ALSA configuration of a voice world, capturing `capture_file`.
pub(crate) fn alsa_config(capture: Capture, capture_file: &str) -> String {
    let playback = "pcm.e2e_playback {\n    type null\n}\n";
    match capture {
        Capture::File => format!(
            "pcm.!default {{\n    type asym\n    capture.pcm \"e2e_capture\"\n    \
             playback.pcm \"e2e_playback\"\n    hint {{\n        show on\n        \
             description \"e2e default\"\n    }}\n}}\npcm.e2e_capture {{\n    type file\n    \
             slave.pcm \"e2e_silence\"\n    file \"/dev/null\"\n    infile \"{capture_file}\"\n    \
             format \"raw\"\n}}\npcm.e2e_silence {{\n    type null\n}}\n{playback}"
        ),
        Capture::Absent => format!(
            "pcm.!default {{\n    type asym\n    playback.pcm \"e2e_playback\"\n    hint {{\n        \
             show on\n        description \"e2e playback\"\n    }}\n}}\n{playback}"
        ),
    }
}

/// One second of a 440 Hz tone at 16 kHz, 16-bit little-endian mono.
pub(crate) fn capture_pcm() -> Vec<u8> {
    (0..16_000u32)
        .flat_map(|n| {
            let phase = f64::from(n) * 440.0 * std::f64::consts::TAU / 16_000.0;
            #[allow(clippy::cast_possible_truncation, reason = "bounded by 8,000")]
            let sample = (phase.sin() * 8_000.0) as i16;
            sample.to_le_bytes()
        })
        .collect()
}

/// What the speech route answers by default: a tenth of a second of silence
/// as a WAV file, base64-encoded the way the speech API returns it.
pub(crate) fn speech_body() -> Value {
    let samples = 1_600u32;
    let mut wav = Vec::new();
    let data = samples * 2;
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    wav.resize(wav.len() + data as usize, 0);
    json!({"audio_data": base64::engine::general_purpose::STANDARD.encode(wav)})
}

/// How the transcription stand-in answers a session.
#[derive(Debug, Clone)]
pub struct Transcription {
    deltas: Vec<String>,
    error: Option<String>,
}

impl Default for Transcription {
    fn default() -> Self {
        Self::text(["e2e dictation"])
    }
}

impl Transcription {
    /// Answers the audio with one text delta per piece.
    pub fn text<I, S>(pieces: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            deltas: pieces.into_iter().map(Into::into).collect(),
            error: None,
        }
    }

    /// Answers the session update with an error frame carrying `message`.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            deltas: Vec::new(),
            error: Some(message.into()),
        }
    }

    /// The text the deltas add up to.
    #[must_use]
    pub fn joined(&self) -> String {
        self.deltas.concat()
    }
}

/// What one transcription session received: its query, the frames the
/// client sent with each audio payload replaced by its length (the first
/// append standing for all), how many base64 audio bytes arrived, and the
/// first audio that is not silence, from its first non-zero sample.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub query: String,
    pub frames: Vec<Value>,
    pub audio_bytes: usize,
    pub first_sound: Vec<u8>,
}

/// How many decoded audio bytes [`Session::first_sound`] keeps.
const FIRST_SOUND: usize = 32;

/// Applies an update to the session being served, wherever the platform
/// keeps it.
pub(crate) type Recorder<'a> = &'a dyn Fn(&dyn Fn(&mut Session));

/// Whether `request` asks to open a realtime transcription.
pub(crate) fn is_transcription(request: &Request) -> bool {
    request.path() == REALTIME_PATH
        && request
            .header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// Serves one transcription session on `stream`, after the handshake
/// `request` opened, recording into `record` as frames arrive.
pub(crate) fn serve(
    mut stream: TcpStream,
    request: &Request,
    script: &Transcription,
    record: Recorder<'_>,
) {
    let Some(key) = request.header("sec-websocket-key") else {
        return;
    };
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\
         sec-websocket-accept: {accept}\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(scaled(DEFAULT_DEADLINE)));
    let query = request
        .target
        .split_once('?')
        .map(|(_, query)| query.to_owned())
        .unwrap_or_default();
    let model = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("model="))
        .unwrap_or("e2e-transcribe-v1")
        .to_owned();
    record(&|session| session.query.clone_from(&query));
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let send = |socket: &mut WebSocket<TcpStream>, frame: Value| {
        socket.send(Message::text(frame.to_string())).is_ok()
    };
    if !send(
        &mut socket,
        json!({"type": "session.created", "session": {
            "request_id": "e2e-transcription",
            "model": model,
            "audio_format": {"encoding": "pcm_s16le", "sample_rate": 16_000},
            "target_streaming_delay_ms": 500,
        }}),
    ) {
        return;
    }
    loop {
        let frame = match socket.read() {
            Ok(Message::Text(text)) => serde_json::from_str::<Value>(text.as_str()).ok(),
            Ok(Message::Binary(bytes)) => serde_json::from_slice::<Value>(&bytes).ok(),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Some(mut frame) = frame else { continue };
        let kind = frame["type"].as_str().unwrap_or_default().to_owned();
        let mut audio = 0;
        let mut sound = Vec::new();
        if let Some(payload) = frame.get_mut("audio") {
            let encoded = payload.as_str().unwrap_or_default();
            audio = encoded.len();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_default();
            // A capture may open on silence before the device's first frames.
            if let Some(start) = decoded.iter().position(|&byte| byte != 0) {
                let start = start - start % 2;
                sound = decoded[start..].iter().copied().take(FIRST_SOUND).collect();
            }
            *payload = json!(audio);
        }
        let is_append = kind == "input_audio.append";
        record(&|session| {
            session.audio_bytes += audio;
            if session.first_sound.is_empty() {
                session.first_sound.clone_from(&sound);
            }
            // Every append differs only in its length, so one stands for all.
            let seen = session
                .frames
                .iter()
                .any(|recorded| recorded["type"] == "input_audio.append");
            if !(is_append && seen) {
                session.frames.push(frame.clone());
            }
        });
        let replies: Vec<Value> = match (kind.as_str(), &script.error) {
            ("session.update", Some(message)) => {
                vec![json!({"type": "error", "error": {"message": message, "code": 4000}})]
            }
            ("input_audio.flush", None) => script
                .deltas
                .iter()
                .map(|text| json!({"type": "transcription.text.delta", "text": text}))
                .collect(),
            ("input_audio.end", None) => vec![json!({
                "type": "transcription.done",
                "model": model,
                "text": script.joined(),
                "usage": {},
                "language": null,
            })],
            _ => Vec::new(),
        };
        for reply in replies {
            if !send(&mut socket, reply) {
                return;
            }
        }
    }
    let _ = socket.close(None);
    let _ = socket.flush();
}

#[cfg(test)]
mod voice_tests;
