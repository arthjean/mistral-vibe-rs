//! US-033: dictation, narration, and the voice settings of the TUI, on a
//! file-backed audio device and the platform's speech stand-ins
//! ([`vibe_e2e::Capture`], [`vibe_e2e::Transcription`]).
//!
//! The reference's Textual UI is not driven, so every trial is port-only and
//! reads its expectation from the reference: Ctrl+R starts a recording only in
//! voice mode, any key stops it, Ctrl+C cancels it, and a start that fails is
//! a notice (`_handle_voice_key` in
//! vibe/cli/textual_ui/widgets/chat_input/text_area.py); text is inserted as
//! it arrives (`on_transcribe_text` in
//! vibe/cli/textual_ui/widgets/chat_input/body.py), which the stand-in holds
//! until the recording stops; alsa-lib's stderr is kept off the screen
//! (`stderr_guard` in vibe/cli/textual_ui/app.py); `/voice` toggles voice mode
//! and the narrator and saves them on close
//! (vibe/cli/textual_ui/widgets/voice_app.py).

use std::time::{Duration, Instant};

use vibe_e2e::{Capture, Ctx, Key, Outcome, Reply, Scenario, Setup, Tui, World, ensure, jail};

use crate::tui::{STEP, launch, prompt, textual, tui_world};

const DICTATED: &str = "e2e dictation";
const SUMMARY: &str = "e2e spoken summary";
const VOICE_ON: &str = "voice_mode_enabled = true\nnarrator_enabled = true";

pub fn scenarios() -> Vec<Scenario> {
    let text_area = || textual("vibe/cli/textual_ui/widgets/chat_input/text_area.py");
    vec![
        Scenario::new(
            "voice::tui::ctrl_r_then_a_key_puts_the_transcript_in_the_composer",
            dictation,
        )
        .port_only(text_area())
        .needs_jail(),
        Scenario::new(
            "voice::tui::ctrl_c_cancels_the_recording_without_text",
            cancel,
        )
        .port_only(text_area())
        .needs_jail(),
        Scenario::new(
            "voice::tui::ctrl_r_does_nothing_with_voice_mode_off",
            disabled,
        )
        .port_only(text_area())
        .needs_jail(),
        Scenario::new(
            "voice::tui::without_a_capture_device_ctrl_r_shows_the_notice",
            no_capture_notice,
        )
        .port_only(text_area())
        .needs_jail()
        .known_defect(
            "E2E-D57",
            "without a capture device the notice reads \"Audio backend is unavailable: Device \
             does not support input\"",
        ),
        Scenario::new(
            "voice::tui::without_a_capture_device_no_alsa_line_reaches_the_screen",
            no_capture_screen,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py"))
        .needs_jail()
        .known_defect(
            "E2E-D58",
            "alsa-lib's error line about the missing capture device is painted over the TUI",
        ),
        Scenario::new(
            "voice::tui::the_narrator_speaks_the_summary_of_an_answer",
            narrator,
        )
        .port_only(textual("vibe/cli/narrator_manager/narrator_manager.py"))
        .needs_jail(),
        Scenario::new(
            "voice::tui::voice_settings_persist_to_config_toml",
            settings,
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/voice_app.py"))
        .needs_jail(),
    ]
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

/// Starts a recording with Ctrl+R and waits until audio reaches the stand-in.
fn record(world: &World, tui: &mut Tui) -> Outcome {
    tui.send(&[Key::Ctrl('r')])?;
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

/// The composer's line: the row that starts with the prompt marker.
fn composer(tui: &Tui) -> Option<String> {
    tui.screen()
        .rows()
        .into_iter()
        .rev()
        .find(|row| row.starts_with('>'))
        .map(|row| row.trim_end().to_owned())
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

fn cancel(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::File, VOICE_ON)?;
    record(&world, &mut tui)?;
    tui.send(&[Key::Ctrl('c')])?;
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
    tui.send(&[Key::Ctrl('r')])?;
    std::thread::sleep(Duration::from_millis(1000));
    ensure(world.platform().transcriptions().is_empty(), || {
        "Ctrl+R opened a transcription with voice mode off".to_owned()
    })?;
    // The composer still takes typing as if Ctrl+R had not been pressed.
    tui.type_text("typed")?;
    tui.wait_screen(
        "\"> typed\" in the composer",
        |screen| screen.rows().iter().any(|row| row.trim_end() == "> typed"),
        STEP,
    )
    .map(drop)
}

fn no_capture_notice(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = voice_tui(ctx, Capture::Absent, VOICE_ON)?;
    tui.send(&[Key::Ctrl('r')])?;
    tui.wait_text("No audio input device found.", STEP)?;
    ensure(world.platform().transcriptions().is_empty(), || {
        "a transcription opened without a capture device".to_owned()
    })
}

fn no_capture_screen(ctx: &mut Ctx) -> Outcome {
    let (_, mut tui) = voice_tui(ctx, Capture::Absent, VOICE_ON)?;
    tui.send(&[Key::Ctrl('r')])?;
    // Whatever the notice says, it is painted after the start failed.
    tui.wait_screen(
        "a notice that the recording cannot start",
        |screen| screen.rows().iter().any(|row| row.starts_with("! ")),
        STEP,
    )?;
    let raw = tui.raw();
    let leaked = raw.windows(8).any(|window| window == b"ALSA lib");
    ensure(!leaked, || {
        format!("alsa-lib wrote to the terminal:\n{}", tui.screen().text())
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
    tui.wait_text("Voice settings", STEP)?;
    // Voice mode, then the narrator.
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
