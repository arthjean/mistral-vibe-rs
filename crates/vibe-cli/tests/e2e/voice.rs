//! US-033: dictation, narration, and the voice settings of the TUI, on a
//! file-backed audio device and the platform's speech stand-ins
//! ([`vibe_e2e::Capture`], [`vibe_e2e::Transcription`]).
//!
//! Every trial is port-only: the reference's manylinux wheels, and the
//! attested environment the battery runs, link its terminal client without
//! the `voice` feature (`_rust_cli_build_flags` in
//! build_backend/maturin_backend.py), so that client refuses to record or to
//! play. The expectations were measured on 2026-11-16 against a build of the
//! pinned client with the feature (v3.1.0, `cargo build --release` of
//! vibe/cli-rust): Ctrl+T starts a recording only in voice mode, any key but
//! Esc stops it, and Esc or Ctrl+C cancels it (`handle_key_inner` and
//! `handle_priority_key` in vibe/cli-rust/src/input.rs); the transcript is
//! inserted where the cursor is (`apply_voice_event` in
//! vibe/cli-rust/src/app.rs); a start that fails is a notice carrying the
//! recorder's message (vibe/cli-rust/src/voice/audio_recorder.rs); the
//! narrator speaks the summary of an answer
//! (vibe/cli-rust/src/narrator_manager.rs); `/voice` toggles voice mode and
//! the narrator and saves them on close (vibe/cli-rust/src/voice_app.rs).
//!
//! This port still records on Ctrl+R, which the reference gave to its prompt
//! history search (E2E-D62). One trial holds that key; the others start a
//! recording with the key of the subject they run against ([`voice_key`]), so
//! each fails for its own cause. Esc ends a recording here instead of
//! canceling it (E2E-D63).

use std::time::{Duration, Instant};

use vibe_e2e::{
    Capture, Ctx, Key, Outcome, Reply, Scenario, Setup, Subject, Tui, World, ensure, jail,
};

use crate::tui::{STEP, launch, prompt, tui_world};

const DICTATED: &str = "e2e dictation";
const SUMMARY: &str = "e2e spoken summary";
const VOICE_ON: &str = "voice_mode_enabled = true\nnarrator_enabled = true";

/// Why no voice trial of the TUI runs against the reference subject.
const NO_VOICE_BUILD: &str = "the reference's manylinux wheels and its attested environment link \
     the terminal client without the `voice` feature (build_backend/maturin_backend.py); the \
     expectation was measured against a build of the pinned client with it";

/// What both implementations put before the audio library's own message when
/// the default device cannot capture.
const NO_CAPTURE: &str = "Audio backend is unavailable:";

pub fn scenarios() -> Vec<Scenario> {
    let trial = |name: &'static str, body: fn(&mut Ctx) -> Outcome| {
        Scenario::new(name, body)
            .port_only(NO_VOICE_BUILD)
            .needs_jail()
    };
    vec![
        trial("voice::tui::ctrl_t_starts_a_recording", reference_key).known_defect(
            "E2E-D62",
            "the TUI records on Ctrl+R, the key the reference gave to its history search, and \
             Ctrl+T starts nothing",
        ),
        trial(
            "voice::tui::a_key_ends_the_recording_and_the_transcript_lands_in_the_composer",
            dictation,
        ),
        trial(
            "voice::tui::ctrl_c_cancels_the_recording_without_text",
            |ctx| cancel(ctx, Key::Ctrl('c')),
        ),
        trial(
            "voice::tui::esc_cancels_the_recording_without_text",
            |ctx| cancel(ctx, Key::Esc),
        )
        .known_defect(
            "E2E-D63",
            "Esc ends the recording as any key does, and the transcript lands in the composer",
        ),
        trial(
            "voice::tui::the_voice_key_does_nothing_with_voice_mode_off",
            disabled,
        ),
        trial(
            "voice::tui::without_a_capture_device_the_voice_key_shows_the_notice",
            no_capture_notice,
        ),
        trial(
            "voice::tui::without_a_capture_device_alsa_lib_writes_to_the_terminal",
            no_capture_screen,
        ),
        trial(
            "voice::tui::the_narrator_speaks_the_summary_of_an_answer",
            narrator,
        ),
        trial(
            "voice::tui::voice_settings_persist_to_config_toml",
            settings,
        ),
    ]
}

/// The key that starts a recording in the subject of this run: Ctrl+T in the
/// reference, Ctrl+R in this port until E2E-D62 is fixed.
fn voice_key(world: &World) -> Key {
    match world.subject() {
        Subject::Reference => Key::Ctrl('t'),
        Subject::Port => Key::Ctrl('r'),
    }
}

/// A TUI on a Mistral world whose audio is `capture`, with `config` added.
fn voice_tui(ctx: &mut Ctx, capture: Capture, config: &str) -> Outcome<(World, Tui)> {
    let found = jail::sound_devices();
    ensure(found.is_empty(), || {
        format!(
            "{} holds {found:?}; the jail must hide it",
            jail::SOUND_DEVICES
        )
    })?;
    let world = tui_world(ctx, Setup::mistral().voice(capture).config(config))?;
    let tui = launch(&world, &[])?;
    if world.subject() == Subject::Reference {
        // The reference's client draws its first frame from a startup cache
        // and reads the voice settings afterward; a voice key typed before
        // that is dropped.
        std::thread::sleep(Duration::from_secs(3));
    }
    Ok((world, tui))
}

/// Waits until `condition` holds of the world, polling.
fn until(world: &World, what: &str, condition: impl Fn(&World) -> bool) -> Outcome {
    let deadline = Instant::now() + vibe_e2e::scaled(STEP);
    while !condition(world) {
        if Instant::now() >= deadline {
            return Err(format!("{what} did not happen within {STEP:?}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Waits until audio reaches the transcription stand-in.
fn recording(world: &World) -> Outcome {
    until(
        world,
        "audio reaching the transcription stand-in",
        |world| {
            world
                .platform()
                .transcriptions()
                .first()
                .is_some_and(|session| session.audio_bytes > 0)
        },
    )
}

/// Starts a recording with the subject's voice key and waits for its audio.
fn record(world: &World, tui: &mut Tui) -> Outcome {
    tui.send(&[voice_key(world)])?;
    recording(world)
}

/// The composer's line: the row that starts with the prompt marker.
fn composer(tui: &Tui) -> Option<String> {
    tui.screen()
        .rows()
        .into_iter()
        .rev()
        .find(|row| row.starts_with('>'))
        .map(|row| row.trim_end().to_owned())
}

/// The key the reference records on, whatever the subject.
fn reference_key(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, VOICE_ON)?;
    tui.send(&[Key::Ctrl('t')])?;
    recording(&world)
}

fn dictation(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, VOICE_ON)?;
    record(&world, &mut tui)?;
    tui.send(&[Key::Text("x".to_owned())])?;
    let expected = format!("> {DICTATED}");
    tui.wait_screen(
        &format!("{expected:?} in the composer"),
        |screen| screen.rows().iter().any(|row| row.trim_end() == expected),
        STEP,
    )?;
    Ok(())
}

fn cancel(ctx: &mut Ctx, key: Key) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, VOICE_ON)?;
    record(&world, &mut tui)?;
    tui.send(&[key])?;
    // Both implementations end the recorder before the transcription task, so
    // a cancel can still flush and draw text from the stand-in; that text
    // must not land.
    std::thread::sleep(Duration::from_millis(1500));
    let line = composer(&tui);
    ensure(line.as_deref() == Some(">"), || {
        format!(
            "the composer holds {line:?} after the cancel:\n{}",
            tui.screen().text()
        )
    })
}

fn disabled(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, "")?;
    tui.send(&[voice_key(&world)])?;
    std::thread::sleep(Duration::from_millis(1000));
    ensure(world.platform().transcriptions().is_empty(), || {
        "the voice key opened a transcription with voice mode off".to_owned()
    })?;
    // The composer still takes typing as if the key had not been pressed.
    tui.type_text("typed")?;
    tui.wait_screen(
        "\"> typed\" in the composer",
        |screen| screen.rows().iter().any(|row| row.trim_end() == "> typed"),
        STEP,
    )
    .map(drop)
}

/// The default device plays and cannot capture, so the recorder fails on its
/// input configuration and both implementations say so with the audio
/// library's own words after [`NO_CAPTURE`].
fn no_capture_notice(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::Absent, VOICE_ON)?;
    tui.send(&[voice_key(&world)])?;
    tui.wait_text(NO_CAPTURE, STEP)?;
    ensure(world.platform().transcriptions().is_empty(), || {
        "a transcription opened without a capture device".to_owned()
    })
}

/// shared_with_reference: alsa-lib reports the missing capture device on
/// stderr, which is the terminal, and neither implementation keeps it off
/// the screen (measured on 2026-11-16). The line overwrites the composer in
/// both, so a guard on either side shows up here.
fn no_capture_screen(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::Absent, VOICE_ON)?;
    tui.send(&[voice_key(&world)])?;
    tui.wait_text(NO_CAPTURE, STEP)?;
    let raw = tui.raw();
    let leaked = raw.windows(8).any(|window| window == b"ALSA lib");
    ensure(leaked, || {
        format!(
            "alsa-lib wrote nothing to the terminal:\n{}",
            tui.screen().text()
        )
    })
}

fn narrator(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, VOICE_ON)?;
    // The session title and the turn summary both ask the utility model.
    world.platform().utility(Reply::text(SUMMARY));
    world.platform().utility(Reply::text(SUMMARY));
    world.platform().reply(Reply::text("an answer to narrate"));
    prompt(&mut tui, "narrate this")?;
    tui.wait_text("an answer to narrate", STEP)?;
    until(&world, "a speech request", |world| {
        world
            .platform()
            .journal()
            .iter()
            .any(|entry| entry.queue == "speech")
    })?;
    let journal = world.platform().journal();
    let speech = journal
        .iter()
        .find(|entry| entry.queue == "speech")
        .ok_or("no speech request")?;
    ensure(speech.body["input"] == SUMMARY, || {
        format!("the speech request carried {}", speech.body)
    })?;
    ensure(speech.body["model"] == "e2e-tts-v1", || {
        format!("the speech request named {}", speech.body["model"])
    })
}

fn settings(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, "")?;
    prompt(&mut tui, "/voice")?;
    // The reference titles the panel "Voice Settings", this port "Voice
    // settings"; both list voice mode first, then the narrator.
    tui.wait_screen(
        "the voice settings panel",
        |screen| screen.text().to_lowercase().contains("voice settings"),
        STEP,
    )?;
    tui.send(&[Key::Enter])?;
    tui.send(&[Key::Down, Key::Enter])?;
    tui.send(&[Key::Esc])?;
    let path = world.vibe_home().join("config.toml");
    until(&world, "both voice settings saved to config.toml", |_| {
        std::fs::read_to_string(&path).is_ok_and(|text| {
            text.contains("voice_mode_enabled = true") && text.contains("narrator_enabled = true")
        })
    })?;
    // Voice mode applies at once.
    record(&world, &mut tui)
}
