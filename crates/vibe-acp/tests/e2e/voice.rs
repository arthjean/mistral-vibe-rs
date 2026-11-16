//! US-033: dictation and narration through ACP's `_voice/*` extension, on a
//! file-backed audio device and the platform's speech stand-ins
//! ([`vibe_e2e::Capture`], [`vibe_e2e::Transcription`]).
//!
//! The expectations were measured against the pinned reference on 2026-10-29
//! (`vibe/acp/voice.py`, `vibe/cli/voice_manager/`, `vibe/cli/narrator_manager/`).
//! Narration summarizes through the provider named `mistral`
//! (`vibe/app_server/_narration.py`), so these worlds declare a Mistral
//! backend.

use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{
    Capture, Ctx, Outcome, Reply, RouteReply, Rpc, Scenario, Setup, Transcription, World, ensure,
    jail,
};

use crate::editor::{editor, new_session};

/// What the stand-in transcribes and the utility model summarizes.
const DICTATED: &str = "e2e dictation";
const SUMMARY: &str = "e2e summary of the answer";

/// How long a dictation records before it is stopped.
const RECORDING: Duration = Duration::from_millis(500);
const NOTIFIED: Duration = Duration::from_secs(10);

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new(
            "acp::voice::device::the_sandbox_captures_the_file_device_only",
            file_device,
        )
        .needs_jail(),
        Scenario::new(
            "acp::voice::dictation::the_transcript_arrives_as_a_delta",
            dictation,
        )
        .needs_jail(),
        Scenario::new(
            "acp::voice::narration::the_summary_is_spoken_after_its_preparation",
            narration,
        )
        .needs_jail(),
        Scenario::new(
            "acp::voice::device::without_a_capture_device_dictation_fails",
            no_capture,
        )
        .needs_jail()
        .known_defect(
            "E2E-D57",
            "without a capture device dictation fails with \"Audio backend is unavailable: \
             Device does not support input\"",
        ),
        Scenario::new(
            "acp::voice::errors::a_transcription_error_frame_reaches_the_editor",
            transcription_error,
        )
        .needs_jail(),
        Scenario::new(
            "acp::voice::errors::a_failing_speech_route_fails_the_narration",
            speech_error,
        )
        .needs_jail(),
    ]
}

/// An editor past `session/new` in a Mistral world whose audio is `capture`.
fn voice_editor(ctx: &mut Ctx, capture: Capture) -> Outcome<(World, Rpc)> {
    let found = jail::sound_devices();
    ensure(found.is_empty(), || {
        format!(
            "{} holds {found:?}; the jail must hide it",
            jail::SOUND_DEVICES
        )
    })?;
    let world = ctx.world_with(Setup::mistral().voice(capture))?;
    let mut rpc = editor(&world, &[])?;
    new_session(&mut rpc, &world)?;
    Ok((world, rpc))
}

/// Records for [`RECORDING`] and stops, returning both results.
fn dictate(rpc: &mut Rpc) -> Outcome<(Value, Value)> {
    let started = rpc.request("_voice/transcribeStart", json!({}))?;
    std::thread::sleep(RECORDING);
    let stopped = rpc.request("_voice/transcribeStop", json!({}))?;
    Ok((started["result"].clone(), stopped["result"].clone()))
}

/// The first `_voice/transcriptionDelta` the editor received.
fn first_delta(rpc: &mut Rpc) -> Outcome<Value> {
    rpc.await_method("_voice/transcriptionDelta", NOTIFIED)
        .map(|message| message["params"].clone())
}

/// The capture file's audio reached the stand-in: the device read the file,
/// so it is the device the implementation opened.
fn file_device(ctx: &mut Ctx) -> Outcome {
    let (world, mut rpc) = voice_editor(ctx, Capture::File)?;
    let (started, _) = dictate(&mut rpc)?;
    ensure(started == json!({"ok": true}), || {
        format!("transcribeStart answered {started}")
    })?;
    first_delta(&mut rpc)?;
    let sessions = world.platform().transcriptions();
    let session = sessions.first().ok_or("no transcription session opened")?;
    let captured = std::fs::read(world.root().join("audio/capture.raw"))?;
    let sent = &session.first_sound;
    ensure(
        !sent.is_empty() && captured.windows(sent.len()).any(|window| window == sent),
        || format!("the first sound sent ({sent:?}) is not in the capture file"),
    )
}

fn dictation(ctx: &mut Ctx) -> Outcome {
    let (world, mut rpc) = voice_editor(ctx, Capture::File)?;
    let (started, stopped) = dictate(&mut rpc)?;
    ensure(
        started == json!({"ok": true}) && stopped == json!({"ok": true, "text": ""}),
        || format!("transcribeStart answered {started}, transcribeStop {stopped}"),
    )?;
    let delta = first_delta(&mut rpc)?;
    ensure(delta == json!({"text": DICTATED}), || {
        format!("the first delta carried {delta}")
    })?;
    rpc.hang_up()?;
    let sessions = world.platform().transcriptions();
    ensure(sessions.len() == 1, || {
        format!("{} transcription sessions opened", sessions.len())
    })?;
    let session = &sessions[0];
    ensure(session.query == "model=e2e-transcribe-v1", || {
        format!("the session opened with the query {:?}", session.query)
    })?;
    let update = session.frames.first().cloned().unwrap_or_default();
    let expected = json!({
        "type": "session.update",
        "session": {
            "audio_format": {"encoding": "pcm_s16le", "sample_rate": 16_000},
            "target_streaming_delay_ms": 500,
        },
    });
    ensure(update == expected, || {
        format!("the first frame was {update}, the reference sends {expected}")
    })?;
    let kinds: Vec<&str> = session
        .frames
        .iter()
        .filter_map(|frame| frame["type"].as_str())
        .collect();
    ensure(
        kinds
            == [
                "session.update",
                "input_audio.append",
                "input_audio.flush",
                "input_audio.end",
            ],
        || format!("the client sent the frames {kinds:?}"),
    )
}

/// Asks for a narration and returns the `_voice/narration*` notifications up
/// to the one that ends it.
fn narrate(rpc: &mut Rpc) -> Outcome<Vec<Value>> {
    let answered = rpc.request(
        "_voice/narrate",
        json!({"userMessage": "what changed", "assistantText": "The answer is long and detailed."}),
    )?;
    ensure(answered["result"] == json!({"ok": true}), || {
        format!("narrate answered {answered}")
    })?;
    rpc.await_notification(
        |message| {
            matches!(
                message["method"].as_str(),
                Some("_voice/narrationDone" | "_voice/narrationError")
            )
        },
        NOTIFIED,
    )?;
    Ok(rpc
        .notifications()
        .iter()
        .filter(|message| {
            message["method"]
                .as_str()
                .is_some_and(|method| method.starts_with("_voice/narration"))
        })
        .cloned()
        .collect())
}

fn methods(notifications: &[Value]) -> Vec<&str> {
    notifications
        .iter()
        .filter_map(|message| message["method"].as_str())
        .collect()
}

fn narration(ctx: &mut Ctx) -> Outcome {
    let (world, mut rpc) = voice_editor(ctx, Capture::File)?;
    world.platform().utility(Reply::text(SUMMARY));
    let notified = narrate(&mut rpc)?;
    ensure(
        methods(&notified) == ["_voice/narrationPrep", "_voice/narrationDone"],
        || format!("the narration notified {notified:?}"),
    )?;
    // The speech request precedes the notification that ends the narration.
    let journal = world.platform().journal();
    let summary = journal
        .iter()
        .find(|entry| entry.queue == "utility")
        .ok_or("no summary request reached the platform")?;
    ensure(summary.body["model"] == "mistral-vibe-cli-fast", || {
        format!("the summary was asked of {}", summary.body["model"])
    })?;
    let speech = journal
        .iter()
        .find(|entry| entry.queue == "speech")
        .ok_or("no speech request reached the platform")?;
    let expected = json!({
        "model": "e2e-tts-v1",
        "input": SUMMARY,
        "voice_id": "gb_jane_neutral",
        "response_format": "wav",
        "stream": false,
        "metadata": {},
    });
    ensure(speech.body == expected, || {
        format!(
            "the speech request carried {}, the reference sends {expected}",
            speech.body
        )
    })
}

fn no_capture(ctx: &mut Ctx) -> Outcome {
    let (_, mut rpc) = voice_editor(ctx, Capture::Absent)?;
    let started = rpc.request("_voice/transcribeStart", json!({}))?;
    let expected = json!({"ok": false, "error": "No audio input device found."});
    ensure(started["result"] == expected, || {
        format!("transcribeStart answered {started}, the reference answers {expected}")
    })
}

fn transcription_error(ctx: &mut Ctx) -> Outcome {
    let (world, mut rpc) = voice_editor(ctx, Capture::File)?;
    world
        .platform()
        .transcription(Transcription::error("e2e transcription refused"));
    let (started, _) = dictate(&mut rpc)?;
    ensure(started == json!({"ok": true}), || {
        format!("transcribeStart answered {started}")
    })?;
    let delta = first_delta(&mut rpc)?;
    ensure(
        delta == json!({"error": "e2e transcription refused"}),
        || format!("the first delta carried {delta}"),
    )
}

fn speech_error(ctx: &mut Ctx) -> Outcome {
    let (world, mut rpc) = voice_editor(ctx, Capture::File)?;
    world.platform().utility(Reply::text(SUMMARY));
    world.platform().route(
        "speech",
        RouteReply::json(500, &json!({"detail": "e2e speech failure"})),
    );
    let notified = narrate(&mut rpc)?;
    ensure(
        methods(&notified) == ["_voice/narrationPrep", "_voice/narrationError"],
        || format!("the narration notified {notified:?}"),
    )?;
    let error = &notified[1]["params"];
    ensure(
        *error == json!({"error": "Narration failed during preparation"}),
        || format!("the narration failed with {error}"),
    )
}
