//! US-026 and US-027: the shipped TUI on a pseudo-terminal, from a configured
//! provider through a golden turn, the exit summary, and resume.
//!
//! The reference's Textual TUI is not driven: its screens differ from the
//! port's by design, so every TUI scenario runs against the port alone and
//! cites the reference module whose behavior it reproduces.

use std::time::Duration;

use serde_json::json;
use vibe_e2e::{
    Ctx, Entry, Key, Outcome, Reply, RouteReply, Scenario, Screen, Setup, Tui, World, ensure,
};

use crate::cli::{chats, content_text};

/// The line the composer's welcome shows once the TUI is ready for input.
pub const READY: &str = "Type /help for more information";

/// How long one TUI step may take before the trial fails, before scaling.
pub const STEP: Duration = Duration::from_secs(20);

/// The terminal every TUI scenario starts on unless it says otherwise.
pub const COLS: u16 = 100;
pub const ROWS: u16 = 30;

/// Why a TUI scenario runs against the port alone, naming the reference
/// module its expectation is read from.
pub fn textual(module: &str) -> String {
    format!("the reference's Textual TUI is not driven; the expectation is read from {module}")
}

/// A world for a TUI scenario: the session title the first answer asks for is
/// answered from the utility queue, so it never takes a turn response.
pub fn tui_world(ctx: &mut Ctx, setup: Setup) -> Outcome<World> {
    let world = ctx.world_with(setup)?;
    world.platform().utility(Reply::text("E2E session title"));
    Ok(world)
}

/// Starts `vibe` with `arguments` on the default terminal and waits until the
/// composer is ready.
pub fn launch(world: &World, arguments: &[&str]) -> Outcome<Tui> {
    let mut tui = world
        .command("vibe")?
        .args(arguments)
        .spawn_tui(COLS, ROWS)?;
    tui.wait_text(READY, STEP)?;
    Ok(tui)
}

/// How the composer shows `text` once typed: a command after the `/ ` mode
/// marker, a shell command after `! `, anything else after the `> ` prompt.
fn composed(text: &str) -> String {
    match (text.strip_prefix('/'), text.strip_prefix('!')) {
        (Some(command), _) => format!("/ {command}"),
        (_, Some(command)) => format!("! {command}"),
        _ => format!("> {text}"),
    }
}

/// Types `text`, waits until the composer shows it, then presses Enter, so the
/// Enter never races the completion popup a `/` opens.
pub fn prompt(tui: &mut Tui, text: &str) -> Outcome {
    tui.type_text(text)?;
    let shown = composed(text);
    tui.wait_screen(
        &format!("{shown:?} in the composer"),
        |s| s.rows().iter().any(|row| row.trim_end() == shown),
        STEP,
    )?;
    tui.send(&[Key::Enter])
}

/// Submits `/exit`, waits for a clean exit, and returns the final screen.
pub fn exit(tui: &mut Tui) -> Outcome<Screen> {
    prompt(tui, "/exit")?;
    let status = tui.wait_exit(STEP)?;
    let screen = tui.screen();
    ensure(status.success, || {
        format!("/exit ended the TUI with {status}:\n{}", screen.text())
    })?;
    Ok(screen)
}

/// The short id the exit summary's `vibe --resume <id>` line names.
pub fn short_id(screen: &Screen) -> Outcome<String> {
    screen
        .rows()
        .iter()
        .find_map(|row| {
            row.trim()
                .strip_prefix("Or: vibe --resume ")
                .map(str::to_owned)
        })
        .ok_or_else(|| format!("the exit summary names no session:\n{}", screen.text()).into())
}

/// The turn requests so far, without the title and other utility requests.
pub fn turns(world: &World) -> Vec<Entry> {
    chats(world)
        .into_iter()
        .filter(|entry| entry.queue == "turn")
        .collect()
}

/// Fails unless exactly `count` turn requests arrived, and returns them.
pub fn turn_count(world: &World, count: usize) -> Outcome<Vec<Entry>> {
    let turns = turns(world);
    ensure(turns.len() == count, || {
        format!(
            "expected {count} turn request(s), the platform received {}",
            turns.len()
        )
    })?;
    Ok(turns)
}

/// The user and assistant texts the `request`-th turn request carries, in
/// order.
pub fn history(world: &World, request: usize) -> Outcome<Vec<String>> {
    let entries = turns(world);
    let entry = entries
        .get(request.saturating_sub(1))
        .ok_or_else(|| format!("no chat request #{request}"))?;
    Ok(entry
        .messages()
        .iter()
        .filter(|m| matches!(m["role"].as_str(), Some("user" | "assistant")))
        .map(content_text)
        .collect())
}

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        Scenario::new(
            "tui::driver::the_composer_receives_every_key",
            composer_keys,
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/chat_input/")),
        Scenario::new("tui::driver::a_resize_repaints_at_the_new_size", resize)
            .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new(
            "tui::routing::a_generic_provider_needs_no_mistral_key",
            generic_provider,
        )
        .port_only(textual("vibe/cli/textual_ui/provider_auth_status.py"))
        .known_defect(
            "E2E-D01",
            "the TUI forces Mistral onboarding for a configured generic provider",
        ),
        Scenario::new(
            "tui::routing::a_mistral_provider_on_loopback",
            mistral_provider,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new(
            "tui::golden::a_paced_answer_renders_before_it_ends",
            paced_answer,
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/messages.py")),
        Scenario::new(
            "tui::exit::the_summary_names_the_short_session_id",
            exit_summary,
        )
        .port_only(textual("vibe/cli/session_exit.py")),
        Scenario::new("tui::resume::by_short_id", |ctx| resume(ctx, Resume::Short))
            .port_only(textual("vibe/cli/session_exit.py"))
            .known_defect(
                "E2E-D24",
                "`vibe --resume <short id>`, the form the exit summary prints, fails with \
                 \"Session not found\"",
            ),
        Scenario::new("tui::resume::continue_opens_the_last_session", |ctx| {
            resume(ctx, Resume::Continue)
        })
        .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new("tui::resume::the_picker_lists_and_opens_a_session", |ctx| {
            resume(ctx, Resume::Picker)
        })
        .port_only(textual("vibe/cli/textual_ui/widgets/session_picker.py"))
        .known_defect(
            "E2E-D24",
            "a bare `--resume` does not offer Unified Harness sessions",
        ),
        Scenario::new(
            "tui::faults::a_provider_failing_every_attempt",
            failing_provider,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py")),
    ];
    for command in ["--check-upgrade", "update"] {
        for choice in [
            Choice::Cancel,
            Choice::Quit,
            Choice::Updated,
            Choice::Failed,
        ] {
            let name = format!(
                "tui::update::{}::{}",
                command.trim_start_matches('-').replace('-', "_"),
                choice.name()
            );
            scenarios.push(
                Scenario::new(name, move |ctx| update_prompt(ctx, command, choice))
                    .port_only(
                        "the reference's update host, pypi.org, is hardcoded; the prompt is read \
                         from `_show_update_prompt` in vibe/cli/cli.py",
                    )
                    .tests_seam("VIBE_UPDATE_BASE_URL"),
            );
        }
    }
    scenarios
}

/// Every key the encoder sends reaches the composer: text, Left, Ctrl+A and
/// Ctrl+E moving the cursor, Backspace, a bracketed paste that keeps its line
/// break and submits nothing, Up moving to the first line, Enter submitting
/// what the composer shows, Tab accepting a command completion, Shift+Tab
/// cycling the mode, and Ctrl+C clearing the composer, then arming the quit.
fn composer_keys(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("received"));
    let mut tui = launch(&world, &[])?;
    ensure(tui.screen().alternate(), || {
        "the TUI did not take the alternate screen".to_owned()
    })?;
    tui.type_text("helo")?;
    tui.send(&[Key::Left, Key::Text("l".to_owned())])?;
    tui.wait_text("> hello", STEP)?;
    tui.send(&[
        Key::Ctrl('a'),
        Key::Text("[".to_owned()),
        Key::Ctrl('e'),
        Key::Text("]".to_owned()),
    ])?;
    tui.wait_text("> [hello]", STEP)?;
    tui.send(&[Key::Backspace, Key::Text("!".to_owned())])?;
    tui.wait_text("> [hello!", STEP)?;
    tui.send(&[Key::Paste("first\nsecond".to_owned())])?;
    let pasted = tui.wait_text("second", STEP)?;
    ensure(pasted.contains("> [hello!first"), || {
        format!("the paste did not land at the cursor:\n{}", pasted.text())
    })?;
    ensure(chats(&world).is_empty(), || {
        "the paste submitted the prompt".to_owned()
    })?;
    tui.send(&[Key::Up, Key::Text("^".to_owned())])?;
    tui.wait_screen(
        "the caret on the first composer line",
        |s| {
            s.rows()
                .iter()
                .any(|row| row.starts_with("> [") && row.contains('^'))
        },
        STEP,
    )?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("received", STEP)?;
    let sent = crate::cli::last_user(&turn_count(&world, 1)?[0]);
    ensure(
        sent.starts_with("[hello") && sent.contains('^') && sent.ends_with("first\nsecond"),
        || format!("the platform received {sent:?}"),
    )?;
    tui.type_text("/hel")?;
    tui.send(&[Key::Tab])?;
    tui.wait_text("/ help", STEP)?;
    let mode = tui.screen();
    tui.send(&[Key::BackTab])?;
    tui.wait_screen(
        "another mode in the footer",
        |s| s.contains("accept-edits") != mode.contains("accept-edits"),
        STEP,
    )?;
    tui.send(&[Key::Ctrl('c')])?;
    tui.wait_screen("an empty composer", |s| !s.contains("/ help"), STEP)?;
    tui.send(&[Key::Ctrl('c')])?;
    tui.wait_text("Press Ctrl+C again to quit", STEP)?;
    tui.send(&[Key::Esc])?;
    exit(&mut tui)?;
    Ok(())
}

/// A resize reaches the TUI, which repaints at the new size: widening, the
/// footer rule spans the new width, which the screen model alone leaves
/// blank; narrowing, the harness notice wraps onto a second row, which
/// truncation alone never produces.
fn resize(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    let mut tui = launch(&world, &[])?;
    let rule = |screen: &Screen, width: usize| {
        screen
            .rows()
            .iter()
            .any(|row| row.chars().count() == width && row.chars().all(|c| c == '─'))
    };
    ensure(rule(&tui.screen(), usize::from(COLS)), || {
        format!(
            "no full-width rule at {COLS} columns:\n{}",
            tui.screen().text()
        )
    })?;
    tui.resize(140, 40)?;
    let screen = tui.wait_screen("a rule 140 columns wide", |s| rule(s, 140), STEP)?;
    ensure(screen.size() == (140, 40), || {
        format!("{:?}", screen.size())
    })?;
    tui.resize(60, 20)?;
    tui.wait_screen(
        "the harness notice wrapped at 60 columns",
        |s| {
            s.rows()
                .iter()
                .any(|row| row.trim_start().starts_with("⎣ issues, restart"))
        },
        STEP,
    )?;
    // Keys typed right after a resize are painted late (E2E-D52, US-030), so
    // the session ends with the world rather than through the composer.
    Ok(())
}

/// A generic-backend provider keyed by its own variable starts the TUI with no
/// `MISTRAL_API_KEY`: no onboarding, one turn naming the configured model, the
/// answer on screen, and no egress (judged by the world).
fn generic_provider(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::default())?;
    world.platform().reply(Reply::text("generic answer"));
    let mut tui = world.command("vibe")?.spawn_tui(COLS, ROWS)?;
    let screen = tui.wait_screen(
        "the composer or onboarding",
        |s| s.contains(READY) || s.contains("Welcome to Mistral Vibe"),
        STEP,
    )?;
    ensure(!screen.contains("Welcome to Mistral Vibe"), || {
        format!(
            "onboarding opened for a configured provider:\n{}",
            screen.text()
        )
    })?;
    prompt(&mut tui, "route me")?;
    tui.wait_text("generic answer", STEP)?;
    let turns = turn_count(&world, 1)?;
    ensure(
        turns[0].body["model"] == json!(world.provider().model_name()),
        || format!("the turn named {}", turns[0].body["model"]),
    )?;
    ensure(
        turns[0].header("authorization") == Some("Bearer e2e-key"),
        || format!("the turn carried {:?}", turns[0].header("authorization")),
    )?;
    exit(&mut tui)?;
    Ok(())
}

/// A Mistral-backend provider on loopback answers on screen with the
/// configured key.
fn mistral_provider(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("mistral answer"));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "route me")?;
    tui.wait_text("mistral answer", STEP)?;
    let turns = turn_count(&world, 1)?;
    ensure(
        turns[0].header("authorization") == Some("Bearer e2e-key"),
        || format!("the turn carried {:?}", turns[0].header("authorization")),
    )?;
    ensure(
        turns[0].body["model"] == json!(world.provider().model_name()),
        || format!("the turn named {}", turns[0].body["model"]),
    )?;
    exit(&mut tui)?;
    Ok(())
}

/// The token footer, as `<used>/<window> tokens`.
fn footer(screen: &Screen) -> Option<String> {
    screen.rows().iter().find_map(|row| {
        let end = row.find(" tokens (")?;
        let start = row[..end].rfind(' ')? + 1;
        Some(row[start..end].to_owned())
    })
}

/// An answer paced in 5 chunks with a gate before the last: the screen shows
/// the partial answer while the gate holds, and the token footer counts the
/// turn once the stream ends.
fn paced_answer(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(
        Reply::chunks(["alpha ", "bravo ", "charlie ", "delta ", "echo-end"])
            .usage(40, 2)
            .paced(100),
    );
    // Events: the role chunk, then one per piece; the fifth piece is event 5.
    let gate = world.platform().gate_before(1, 5);
    let mut tui = launch(&world, &[])?;
    let before = footer(&tui.screen());
    prompt(&mut tui, "stream please")?;
    world.platform().await_events(1, 5, STEP)?;
    let partial = tui.wait_text("alpha bravo charlie delta", STEP)?;
    std::thread::sleep(Duration::from_millis(300));
    let held = tui.screen();
    gate.release();
    ensure(!held.contains("echo-end"), || {
        format!("the held piece was on screen:\n{}", held.text())
    })?;
    ensure(footer(&partial) == before, || {
        format!(
            "the footer moved before the stream ended: {before:?} then {:?}",
            footer(&partial)
        )
    })?;
    let done = tui.wait_text("delta echo-end", STEP)?;
    let after = tui.wait_screen(
        "a token footer counting the turn",
        |s| footer(s).is_some_and(|f| f.starts_with("42/")),
        STEP,
    )?;
    ensure(
        done.contains("alpha bravo") && after.contains("delta echo-end"),
        || done.text(),
    )?;
    exit(&mut tui)?;
    Ok(())
}

/// `/exit` prints the token total and the two continuation lines, the second
/// naming the first 8 characters of the session id.
fn exit_summary(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("noted"));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "remember this")?;
    tui.wait_text("noted", STEP)?;
    let screen = exit(&mut tui)?;
    ensure(
        screen.contains("To continue this session, run: vibe --continue"),
        || format!("no continuation line:\n{}", screen.text()),
    )?;
    let id = short_id(&screen)?;
    ensure(
        id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()),
        || format!("the summary names {id:?}"),
    )?;
    let sessions = session_ids(&world);
    ensure(sessions.iter().any(|full| full.starts_with(&id)), || {
        format!("no saved session starts with {id}: {sessions:?}")
    })
}

/// The ids of the sessions saved under the vibe home, from their `meta.json`.
pub fn session_ids(world: &World) -> Vec<String> {
    let mut ids = Vec::new();
    let mut pending = vec![world.vibe_home().join("logs")];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|n| n == "meta.json")
                && let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(id) = meta["session_id"].as_str().or(meta["id"].as_str())
            {
                ids.push(id.to_owned());
            }
        }
    }
    ids
}

#[derive(Clone, Copy)]
enum Resume {
    Short,
    Continue,
    Picker,
}

/// A first session, then a second one opened by the short id the exit summary
/// printed, by `-c`, or from the bare `--resume` picker: the earlier history
/// is on screen and the next turn request carries it.
fn resume(ctx: &mut Ctx, how: Resume) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("first answer"));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "first marker")?;
    tui.wait_text("first answer", STEP)?;
    let id = short_id(&exit(&mut tui)?)?;
    drop(tui);
    let mut request = 2;
    if matches!(how, Resume::Picker) {
        // A newer session, listed first, so the picker must move to reach
        // the older one.
        world.platform().utility(Reply::text("E2E second title"));
        world.platform().reply(Reply::text("second answer"));
        let mut tui = launch(&world, &[])?;
        prompt(&mut tui, "second marker")?;
        tui.wait_text("second answer", STEP)?;
        exit(&mut tui)?;
        request = 3;
    }
    world.platform().reply(Reply::text("resumed answer"));
    let arguments: Vec<&str> = match how {
        Resume::Short => vec!["--resume", &id],
        Resume::Continue => vec!["-c"],
        Resume::Picker => vec!["--resume"],
    };
    let mut tui = world
        .command("vibe")?
        .args(&arguments)
        .spawn_tui(COLS, ROWS)?;
    if matches!(how, Resume::Picker) {
        let picker = tui.wait_text(&id, STEP)?;
        ensure(
            picker.contains("second marker") && picker.contains("first marker"),
            || format!("the picker does not list both sessions:\n{}", picker.text()),
        )?;
        let newer = picker.find("second marker").map(|(row, _)| row);
        let older = picker.find("first marker").map(|(row, _)| row);
        ensure(newer < older, || {
            format!("the newer session is not listed first:\n{}", picker.text())
        })?;
        tui.send(&[Key::Down, Key::Enter])?;
    }
    let screen = tui.wait_text("first answer", STEP)?;
    ensure(screen.contains("first marker"), || {
        format!("the resumed history is not on screen:\n{}", screen.text())
    })?;
    prompt(&mut tui, "after resume")?;
    tui.wait_text("resumed answer", STEP)?;
    let carried = history(&world, request)?;
    ensure(
        carried.first().map(String::as_str) == Some("first marker")
            && carried.iter().any(|text| text == "first answer")
            && carried.last().map(String::as_str) == Some("after resume"),
        || format!("the resumed turn carried {carried:?}"),
    )?;
    exit(&mut tui)?;
    Ok(())
}

/// A provider answering 500 on every attempt: an error is on screen within
/// the retry budget, and `/exit` still ends the TUI.
fn failing_provider(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(
        ctx,
        Setup::mistral().config("api_retry_max_elapsed_time = 3"),
    )?;
    world
        .platform()
        .replies((0..12).map(|_| Reply::status(500).body(r#"{"message":"boom"}"#)));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "fail please")?;
    let screen = tui.wait_text("Status 500", Duration::from_secs(30))?;
    ensure(!chats(&world).is_empty(), || {
        format!("no turn request was sent:\n{}", screen.text())
    })?;
    exit(&mut tui)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Choice {
    Cancel,
    Quit,
    Updated,
    Failed,
}

impl Choice {
    fn name(self) -> &'static str {
        match self {
            Self::Cancel => "cancel_exits_zero",
            Self::Quit => "ctrl_c_exits_zero",
            Self::Updated => "a_successful_update_exits_zero",
            Self::Failed => "a_failed_update_exits_one",
        }
    }
}

/// A newer release on the release route opens the update prompt in the
/// terminal; each choice ends as `_show_update_prompt` (vibe/cli/cli.py) ends
/// its result: `Cancel upgrade` and Ctrl+C exit 0, a successful update prints
/// both versions and exits 0, and a failed one names the manual path and
/// exits 1. The installer runs through `sh -c`, so a `sh` placed first on
/// `PATH` decides its outcome without running it.
fn update_prompt(ctx: &mut Ctx, command: &'static str, choice: Choice) -> Outcome {
    let world = ctx.world()?;
    world.platform().route(
        "release_github",
        RouteReply::json(
            200,
            &json!([{"tag_name": "v99.0.0", "published_at": "2026-01-01T00:00:00Z",
                     "prerelease": false, "draft": false}]),
        ),
    );
    let shell = world.tmp().join("bin");
    let code = if matches!(choice, Choice::Failed) {
        1
    } else {
        0
    };
    world.write(shell.join("sh"), format!("#!/bin/sh\nexit {code}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(shell.join("sh"), std::fs::Permissions::from_mode(0o755))?;
    }
    let path = format!(
        "{}:{}",
        shell.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut tui = world
        .command("vibe")?
        .arg(command)
        .env("VIBE_UPDATE_BASE_URL", world.platform().base())
        .env("PATH", path)
        .spawn_tui(COLS, ROWS)?;
    let prompt = tui.wait_text("A new Vibe release is available", STEP)?;
    let current = env!("CARGO_PKG_VERSION");
    ensure(
        prompt.contains(&format!("{current} \u{2192} 99.0.0")) && prompt.contains("Cancel upgrade"),
        || format!("the prompt does not offer the release:\n{}", prompt.text()),
    )?;
    match choice {
        Choice::Cancel => tui.send(&[Key::Right, Key::Enter])?,
        Choice::Quit => tui.send(&[Key::Ctrl('c')])?,
        Choice::Updated | Choice::Failed => tui.send(&[Key::Enter])?,
    }
    let status = tui.wait_exit(STEP)?;
    let screen = tui.screen();
    let expected = if matches!(choice, Choice::Failed) {
        1
    } else {
        0
    };
    ensure(status.code == expected && status.signal.is_none(), || {
        format!(
            "expected exit code {expected}, got {status}:\n{}",
            screen.text()
        )
    })?;
    match choice {
        Choice::Updated => ensure(
            screen.contains(&format!("Vibe was updated from {current} to 99.0.0.")),
            || format!("no success line:\n{}", screen.text()),
        )?,
        Choice::Failed => ensure(
            screen.contains("Vibe could not update automatically."),
            || format!("no failure line:\n{}", screen.text()),
        )?,
        Choice::Cancel | Choice::Quit => ensure(!screen.contains("Updating Vibe"), || {
            format!("the update ran:\n{}", screen.text())
        })?,
    }
    ensure(chats(&world).is_empty(), || {
        "the prompt started a session".to_owned()
    })
}
