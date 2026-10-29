//! US-029: every slash command of the TUI registry after a model turn, the
//! configuration editor, the model picker, the composer, and onboarding.

use std::time::Duration;

use vibe_cli::tui::commands::{COMMANDS, CommandDefinition};
use vibe_e2e::{Ctx, Key, Outcome, Reply, Scenario, Setup, Tui, World, ensure};

use crate::cli::last_user;
use crate::tui::{READY, STEP, exit, launch, prompt, textual, tui_world, turn_count, turns};

/// What a failed app-server call prints in the transcript.
const SERVER_ERROR: &str = "app-server error";

/// How long a command gets to fail asynchronously once its echo is on screen.
const SETTLE: Duration = Duration::from_millis(1500);

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios: Vec<Scenario> = COMMANDS
        .iter()
        .map(|definition| {
            let scenario =
                Scenario::new(format!("tui::commands::{}", definition.name), move |ctx| {
                    command(ctx, definition)
                })
                .port_only(textual("vibe/cli/commands.py"));
            match definition.name {
                "copy" => scenario.windows_ignore(
                    "windows: E2E-D59 `/copy` waits on its PowerShell clipboard command without \
                     a bound, on the event loop, and the TUI stops repainting (US-034)",
                ),
                "debug" => scenario.known_defect(
                    "E2E-D25",
                    "`/debug` sends `diagnostics/logs/read` without `sessionId`",
                ),
                "config" => scenario.known_defect(
                    "E2E-D25",
                    "`/config` answers with an app-server `InvalidParams` error",
                ),
                _ => scenario,
            }
        })
        .collect();
    scenarios.extend([
        Scenario::new("tui::debug::lists_log_entries", debug_lists_entries)
            .port_only(textual("vibe/cli/textual_ui/app.py"))
            .known_defect(
                "E2E-D25",
                "`/debug` sends `diagnostics/logs/read` without `sessionId`",
            ),
        Scenario::new("tui::log::shows_an_existing_path", log_shows_a_path)
            .port_only(textual("vibe/cli/textual_ui/app.py"))
            .known_defect("E2E-D25", "`/log` reports an unpersisted log"),
        Scenario::new("tui::config::an_edit_reaches_config_toml", config_edit)
            .port_only(textual("vibe/cli/textual_ui/screens/config/"))
            .known_defect("E2E-D25", "`/config` is read-only"),
        Scenario::new(
            "tui::model::the_picker_preselects_the_active_model",
            model_picker,
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/model_picker.py")),
        Scenario::new(
            "tui::composer::multiline_input_reaches_the_platform_as_shown",
            multiline,
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/chat_input/")),
        Scenario::new("tui::composer::a_paste_submits_nothing_by_itself", paste)
            .port_only(textual("vibe/cli/textual_ui/widgets/chat_input/")),
        Scenario::new(
            "tui::composer::history_carries_across_sessions",
            history_across_sessions,
        )
        .port_only(textual("vibe/cli/history_manager.py")),
        Scenario::new(
            "tui::composer::a_path_completion_reaches_the_platform",
            path_completion,
        )
        .port_only(textual("vibe/cli/autocompletion/")),
        Scenario::new(
            "tui::composer::a_command_completion_runs_the_command",
            command_completion,
        )
        .port_only(textual("vibe/cli/autocompletion/")),
        Scenario::new("tui::onboarding::a_typed_key_is_saved", |ctx| {
            onboarding(ctx, false)
        })
        .port_only(textual("vibe/setup/auth/api_key_persistence.py")),
        Scenario::new("tui::onboarding::a_pasted_key_is_saved", |ctx| {
            onboarding(ctx, true)
        })
        .port_only(textual("vibe/setup/auth/api_key_persistence.py"))
        .known_defect(
            "E2E-D50",
            "onboarding turns bracketed paste on and drops the pasted key",
        ),
        Scenario::new(
            "tui::commands::an_unknown_command_goes_to_the_model",
            unknown_command,
        )
        .port_only(textual(
            "vibe/cli/textual_ui/widgets/chat_input/input_kinds.py",
        )),
    ]);
    scenarios
}

/// A Mistral world whose TUI has completed one turn.
fn after_turn(ctx: &mut Ctx, setup: Setup) -> Outcome<(World, Tui)> {
    let world = tui_world(ctx, setup)?;
    world.platform().reply(Reply::text("warm-up answer"));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "warm up")?;
    tui.wait_text("warm-up answer", STEP)?;
    Ok((world, tui))
}

/// Fails when an app-server error is on screen.
fn no_server_error(tui: &Tui, after: &str) -> Outcome {
    let screen = tui.screen();
    ensure(!screen.contains(SERVER_ERROR), || {
        format!(
            "{after} left an app-server error on screen:\n{}",
            screen.text()
        )
    })
}

/// Commands that refuse in a world with no GitHub remote and no key the
/// console verifies, as the reference refuses them
/// (`ServiceTeleportNotSupportedError` in vibe/core/teleport/git.py): their
/// refusal is the expected outcome, not a failure.
const REFUSED_HERE: [&str; 2] = ["teleport", "remote-project"];

/// One registry command after a completed turn: it opens, Esc (`q` for the
/// rewind panel, `quit_rewind` in rewind_app.py) closes what it opened, no
/// app-server error reaches the screen, and the TUI still takes `/exit`; a
/// command that ends the session ends it cleanly.
fn command(ctx: &mut Ctx, definition: &'static CommandDefinition) -> Outcome {
    let (world, mut tui) = after_turn(ctx, Setup::mistral())?;
    // `/compact` asks for a summary and `/retry` for another answer.
    world
        .platform()
        .utility(Reply::text("Summary of the conversation."));
    world.platform().reply(Reply::text("retried answer"));
    let alias = definition.aliases[0];
    let text = if alias.starts_with('/') {
        alias.to_owned()
    } else {
        format!("/{}", definition.name)
    };
    prompt(&mut tui, &text)?;
    if definition.exits {
        let status = tui.wait_exit(STEP)?;
        return ensure(status.success, || {
            format!(
                "{text} ended the TUI with {status}:\n{}",
                tui.screen().text()
            )
        });
    }
    std::thread::sleep(SETTLE);
    let refused = REFUSED_HERE.contains(&definition.name);
    if !refused {
        no_server_error(&tui, &text)?;
    }
    let close = if definition.name == "rewind" {
        Key::Text("q".to_owned())
    } else {
        Key::Esc
    };
    tui.send(&[close])?;
    std::thread::sleep(Duration::from_millis(300));
    if !refused {
        no_server_error(&tui, &text)?;
    }
    exit(&mut tui).map(drop)
}

/// The log entries `/debug` lists, with `LOG_LEVEL=DEBUG`.
fn debug_lists_entries(ctx: &mut Ctx) -> Outcome {
    let (_, mut tui) = after_turn(ctx, Setup::mistral().env("LOG_LEVEL", "DEBUG"))?;
    prompt(&mut tui, "/debug")?;
    std::thread::sleep(SETTLE);
    no_server_error(&tui, "/debug")?;
    let screen = tui.wait_text("DEBUG", STEP)?;
    ensure(screen.contains("INFO") || screen.contains("DEBUG"), || {
        format!("/debug lists no log entry:\n{}", screen.text())
    })?;
    tui.send(&[Key::Esc])?;
    exit(&mut tui).map(drop)
}

/// `/log` names the session log, a file that exists.
fn log_shows_a_path(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = after_turn(ctx, Setup::mistral().env("LOG_LEVEL", "DEBUG"))?;
    prompt(&mut tui, "/log")?;
    std::thread::sleep(SETTLE);
    no_server_error(&tui, "/log")?;
    let home = world.vibe_home().to_string_lossy().into_owned();
    let screen = tui.wait_text(&home, STEP)?;
    let path = screen
        .rows()
        .iter()
        .find_map(|row| {
            let start = row.find(&home)?;
            Some(row[start..].split_whitespace().next()?.to_owned())
        })
        .ok_or_else(|| format!("/log names no path:\n{}", screen.text()))?;
    ensure(std::path::Path::new(&path).exists(), || {
        format!(
            "/log names {path}, which does not exist:\n{}",
            screen.text()
        )
    })?;
    exit(&mut tui).map(drop)
}

/// `/config`: filtering to `disable_welcome_banner_animation`, Enter opens the
/// choice screen on the current `False`, Up and Enter pick `True`, and Esc
/// closes the browser; `config.toml` then holds the new value
/// (`ConfigScreen` and `prompt_field_value` in
/// vibe/cli/textual_ui/screens/config/).
fn config_edit(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = after_turn(ctx, Setup::mistral())?;
    prompt(&mut tui, "/config")?;
    tui.type_text("disable_welcome_banner")?;
    tui.wait_text("disable_welcome_banner_animation", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("True", STEP)?;
    tui.send(&[Key::Up, Key::Enter])?;
    std::thread::sleep(SETTLE);
    tui.send(&[Key::Esc])?;
    let path = world.vibe_home().join("config.toml");
    let until = std::time::Instant::now() + vibe_e2e::scaled(STEP);
    loop {
        let text = std::fs::read_to_string(&path)?;
        if text.contains("disable_welcome_banner_animation = true") {
            break;
        }
        if std::time::Instant::now() >= until {
            return Err(format!("config.toml never held the edit:\n{text}").into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tui.wait_screen(
        "the composer after the browser closed",
        |s| s.contains(READY) || s.contains("accept-edits"),
        STEP,
    )?;
    exit(&mut tui).map(drop)
}

/// A second model on the world's Mistral provider.
fn second_model() -> &'static str {
    "[[models]]\nname = \"mistral-small-latest\"\nprovider = \"mistral\"\nalias = \"e2e-other\"\n\
     input_price = 0.0\noutput_price = 0.0\n"
}

/// `/model` with two models: the active one is preselected, and Esc leaves it
/// active for the next turn.
fn model_picker(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    crate::cli::edit_config(&world, |text| format!("{text}\n{}", second_model()))?;
    world
        .platform()
        .replies([Reply::text("warm-up answer"), Reply::text("still here")]);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "warm up")?;
    tui.wait_text("warm-up answer", STEP)?;
    prompt(&mut tui, "/model")?;
    let picker = tui.wait_screen(
        "both models in the picker",
        |s| s.contains("e2e-other") && s.contains("e2e-model"),
        STEP,
    )?;
    let selected = picker
        .rows()
        .into_iter()
        .find(|row| row.contains('›') || row.contains('▸'))
        .ok_or_else(|| format!("the picker marks no row:\n{}", picker.text()))?;
    ensure(selected.contains("e2e-model"), || {
        format!("the picker preselects {selected:?}:\n{}", picker.text())
    })?;
    tui.send(&[Key::Esc])?;
    tui.wait_screen("the picker closed", |s| !s.contains("e2e-other"), STEP)?;
    prompt(&mut tui, "which model")?;
    tui.wait_text("still here", STEP)?;
    let second = &turn_count(&world, 2)?[1];
    ensure(second.body["model"] == "mistral-medium-latest", || {
        format!("the turn after Esc named {}", second.body["model"])
    })?;
    exit(&mut tui).map(drop)
}

/// Two lines entered with Ctrl+J reach the platform as the composer showed
/// them.
fn multiline(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("got both lines"));
    let mut tui = launch(&world, &[])?;
    tui.type_text("first line")?;
    tui.send(&[Key::Ctrl('j')])?;
    tui.type_text("second line")?;
    let shown = tui.wait_text("second line", STEP)?;
    ensure(shown.contains("> first line") && chat_free(&world), || {
        format!("the composer does not hold both lines:\n{}", shown.text())
    })?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("got both lines", STEP)?;
    let sent = last_user(&turn_count(&world, 1)?[0]);
    ensure(sent == "first line\nsecond line", || {
        format!("the platform received {sent:?}")
    })?;
    exit(&mut tui).map(drop)
}

fn chat_free(world: &World) -> bool {
    turns(world).is_empty()
}

/// A bracketed paste of three lines lands whole in the composer and submits
/// nothing; Enter then sends it as shown.
fn paste(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("pasted text received"));
    let mut tui = launch(&world, &[])?;
    tui.send(&[Key::Paste("alpha\nbeta\ngamma".to_owned())])?;
    let shown = tui.wait_text("gamma", STEP)?;
    std::thread::sleep(Duration::from_millis(500));
    ensure(chat_free(&world), || {
        "the paste submitted itself".to_owned()
    })?;
    ensure(shown.contains("> alpha") && shown.contains("beta"), || {
        format!("the paste is not whole in the composer:\n{}", shown.text())
    })?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("pasted text received", STEP)?;
    let sent = last_user(&turn_count(&world, 1)?[0]);
    ensure(sent == "alpha\nbeta\ngamma", || {
        format!("the platform received {sent:?}")
    })?;
    exit(&mut tui).map(drop)
}

/// A prompt submitted in one session comes back with Up in the next, and
/// Enter sends it again.
fn history_across_sessions(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world
        .platform()
        .replies([Reply::text("first session"), Reply::text("second session")]);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "remembered prompt")?;
    tui.wait_text("first session", STEP)?;
    exit(&mut tui)?;
    drop(tui);
    world.platform().utility(Reply::text("E2E second title"));
    let mut tui = launch(&world, &[])?;
    // Every submitted line is history, `/exit` included (`HistoryManager.add`
    // in vibe/cli/history_manager.py), so the prompt is two entries back.
    tui.send(&[Key::Up])?;
    tui.wait_text("/ exit", STEP)?;
    tui.send(&[Key::Up])?;
    tui.wait_text("> remembered prompt", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("second session", STEP)?;
    let sent = last_user(&turn_count(&world, 2)?[1]);
    ensure(sent == "remembered prompt", || {
        format!("the platform received {sent:?}")
    })?;
    exit(&mut tui).map(drop)
}

/// `@not` completes to the work tree's `notes-e2e.md` with Tab, and the
/// mention reaches the platform as the composer showed it.
fn path_completion(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.write("work/notes-e2e.md", "mentioned file\n")?;
    world.platform().reply(Reply::text("saw the mention"));
    let mut tui = launch(&world, &[])?;
    tui.type_text("read @notes-e")?;
    tui.wait_text("notes-e2e.md", STEP)?;
    tui.send(&[Key::Tab])?;
    tui.wait_text("> read @notes-e2e.md", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("saw the mention", STEP)?;
    let sent = last_user(&turn_count(&world, 1)?[0]);
    ensure(sent.starts_with("read @notes-e2e.md"), || {
        format!("the platform received {sent:?}")
    })?;
    exit(&mut tui).map(drop)
}

/// `/hel` completes to `/help` with Tab, and Enter runs it: the help lists
/// the commands and nothing reaches the platform.
fn command_completion(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    let mut tui = launch(&world, &[])?;
    tui.type_text("/hel")?;
    // Tab completes what the popup offers, so it waits for the command mode.
    tui.wait_screen(
        "\"/ hel\" in the composer",
        |screen| screen.rows().iter().any(|row| row.trim_end() == "/ hel"),
        STEP,
    )?;
    tui.send(&[Key::Tab])?;
    tui.wait_text("/ help", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("/model: Select active model", STEP)?;
    ensure(chat_free(&world), || {
        "/help reached the platform".to_owned()
    })?;
    tui.send(&[Key::Esc])?;
    exit(&mut tui).map(drop)
}

/// Onboarding with no key: the key, pasted or typed, lands in
/// `VIBE_HOME/.env`, owner only (`_save_api_key_to_env_file` in
/// vibe/setup/auth/api_key_persistence.py creates it 0600), and the next turn
/// carries it.
fn onboarding(ctx: &mut Ctx, paste: bool) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("onboarded answer"));
    let mut tui = world
        .command("vibe")?
        .env_remove("MISTRAL_API_KEY")
        .spawn_tui(crate::tui::COLS, crate::tui::ROWS)?;
    // The welcome takes Enter once its greeting has finished typing.
    tui.wait_text("Press Enter to start", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("Pick the colors you want to work in", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("Paste an API key", STEP)?;
    tui.send(&[Key::Down, Key::Enter])?;
    tui.wait_text("Paste API key", STEP)?;
    let key = "pasted-e2e-key".to_owned();
    tui.send(&[if paste {
        Key::Paste(key)
    } else {
        Key::Text(key)
    }])?;
    tui.send(&[Key::Enter])?;
    tui.wait_text(READY, STEP)?;
    let env = world.vibe_home().join(".env");
    let text =
        std::fs::read_to_string(&env).map_err(|error| format!("no {}: {error}", env.display()))?;
    ensure(
        text.lines()
            .any(|line| line.starts_with("MISTRAL_API_KEY=") && line.contains("pasted-e2e-key")),
        || format!(".env holds {text:?}"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env)?.permissions().mode() & 0o777;
        ensure(mode == 0o600, || format!(".env has mode {mode:o}"))?;
    }
    prompt(&mut tui, "after onboarding")?;
    tui.wait_text("onboarded answer", STEP)?;
    let turn = &turn_count(&world, 1)?[0];
    ensure(
        turn.header("authorization") == Some("Bearer pasted-e2e-key"),
        || format!("the turn carried {:?}", turn.header("authorization")),
    )?;
    exit(&mut tui).map(drop)
}

/// An unknown slash command is neither a command nor a skill, so it goes to
/// the model as a prompt and leaves the composer empty (`classify` in
/// vibe/cli/textual_ui/widgets/chat_input/input_kinds.py).
fn unknown_command(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("answered the slash"));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "/nosuchcommand please")?;
    tui.wait_text("answered the slash", STEP)?;
    // A key typed now shows alone only in an empty composer.
    tui.type_text("x")?;
    tui.wait_text("> x", STEP)?;
    tui.send(&[Key::Backspace])?;
    let sent = last_user(&turn_count(&world, 1)?[0]);
    ensure(sent == "/nosuchcommand please", || {
        format!("the platform received {sent:?}")
    })?;
    exit(&mut tui).map(drop)
}
