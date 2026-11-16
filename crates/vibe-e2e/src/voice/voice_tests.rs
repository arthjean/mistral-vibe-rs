use std::net::TcpStream;

use base64::Engine as _;
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

use super::{Capture, Transcription, alsa_config, speech_body};
use crate::Platform;

fn connect(platform: &Platform) -> WebSocket<TcpStream> {
    let stream = TcpStream::connect(platform.address()).unwrap();
    let url = format!(
        "ws://{}/v1/audio/transcriptions/realtime?model=e2e-transcribe-v1",
        platform.address()
    );
    tungstenite::client(url, stream).unwrap().0
}

fn next(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = socket.read().unwrap() {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

fn send(socket: &mut WebSocket<TcpStream>, frame: &Value) {
    socket.send(Message::text(frame.to_string())).unwrap();
}

#[test]
fn a_session_answers_the_audio_with_the_scripted_text() {
    let platform = Platform::start().unwrap();
    platform.transcription(Transcription::text(["hello ", "world"]));
    let mut socket = connect(&platform);
    let created = next(&mut socket);
    assert_eq!(created["type"], "session.created");
    assert_eq!(created["session"]["model"], "e2e-transcribe-v1");
    send(
        &mut socket,
        &json!({"type": "session.update", "session": {"audio_format":
            {"encoding": "pcm_s16le", "sample_rate": 16_000}}}),
    );
    for _ in 0..2 {
        send(
            &mut socket,
            &json!({"type": "input_audio.append", "audio": "AAABAgM="}),
        );
    }
    send(&mut socket, &json!({"type": "input_audio.flush"}));
    assert_eq!(next(&mut socket)["text"], "hello ");
    assert_eq!(next(&mut socket)["text"], "world");
    send(&mut socket, &json!({"type": "input_audio.end"}));
    let done = next(&mut socket);
    assert_eq!(done["type"], "transcription.done");
    assert_eq!(done["text"], "hello world");
    socket.close(None).unwrap();
    while socket.read().is_ok() {}
    let sessions = platform.transcriptions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].query, "model=e2e-transcribe-v1");
    assert_eq!(sessions[0].audio_bytes, 16);
    assert_eq!(sessions[0].first_sound, [1, 2, 3]);
    let kinds: Vec<&str> = sessions[0]
        .frames
        .iter()
        .filter_map(|frame| frame["type"].as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "session.update",
            "input_audio.append",
            "input_audio.flush",
            "input_audio.end"
        ]
    );
    let queues: Vec<String> = platform.journal().into_iter().map(|e| e.queue).collect();
    assert_eq!(queues, ["transcription"]);
    assert!(platform.failures().is_empty(), "{:?}", platform.failures());
}

#[test]
fn a_scripted_error_answers_the_session_update() {
    let platform = Platform::start().unwrap();
    platform.transcription(Transcription::error("e2e quota"));
    let mut socket = connect(&platform);
    next(&mut socket);
    send(
        &mut socket,
        &json!({"type": "session.update", "session": {}}),
    );
    let error = next(&mut socket);
    assert_eq!(
        error,
        json!({"type": "error", "error": {"message": "e2e quota", "code": 4000}})
    );
}

#[test]
fn the_alsa_configuration_declares_a_capture_device_only_when_asked() {
    let file = alsa_config(Capture::File, "/w/audio/capture.raw");
    assert!(file.contains("capture.pcm \"e2e_capture\""), "{file}");
    assert!(file.contains("infile \"/w/audio/capture.raw\""), "{file}");
    let absent = alsa_config(Capture::Absent, "/w/audio/capture.raw");
    assert!(!absent.contains("capture"), "{absent}");
    assert!(absent.contains("playback.pcm \"e2e_playback\""), "{absent}");
}

#[test]
fn the_speech_answer_is_a_base64_wav() {
    let body = speech_body();
    let wav = base64::engine::general_purpose::STANDARD
        .decode(body["audio_data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..16], b"WAVEfmt ");
    assert_eq!(wav.len(), 44 + 3_200);
}
