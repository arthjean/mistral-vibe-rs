//! US-030: the session title, startup dialogs, resizes, a degraded provider,
//! and idle output in the shipped TUI.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Key, Outcome, Reply, Scenario, Setup, Tui, World, ensure, scaled};

use crate::cli::last_user;
use crate::tui::{STEP, exit, launch, prompt, textual, tui_world, turn_count};

/// The title the utility queue answers the title request with.
const TITLE: &str = "E2E session title";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (harness, flags) in [("unified", &[][..]), ("legacy", &["--legacy-harness"][..])] {
        scenarios.push(
            Scenario::new(
                format!("tui::title::{harness}::meta_and_window_carry_it"),
                move |ctx| title(ctx, flags),
            )
            .port_only(textual("vibe/cli/textual_ui/app.py"))
            .known_defect(
                "E2E-D51",
                "the generated session title never reaches the window title, and the default \
                 harness leaves it out of meta.json",
            ),
        );
    }
    scenarios.extend([
        Scenario::new(
            "tui::startup::an_untrusted_project_config_asks_for_trust",
            trust_dialog,
        )
        .port_only(textual("vibe/setup/trusted_folders/")),
        Scenario::new("tui::startup::whats_new_shows_once", whats_new)
            .port_only(textual("vibe/cli/update_notifier/whats_new.py")),
        Scenario::new("tui::startup::no_vs_code_promotion", vscode)
            .port_only(textual("vibe/cli/vscode_extension_promo/")),
        Scenario::new("tui::resize::during_a_stream", resize_stream)
            .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new("tui::resize::during_an_approval", resize_approval)
            .port_only(textual("vibe/cli/textual_ui/widgets/approval_app.py")),
        Scenario::new(
            "tui::resize::keys_typed_right_after_are_painted",
            typed_after_resize,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py"))
        .known_defect(
            "E2E-D52",
            "keys typed within 200 ms of a resize are painted only when another event arrives",
        ),
        Scenario::new("tui::degraded::a_failing_provider_shows_retrying", retrying)
            .port_only(textual("vibe/cli/textual_ui/widgets/loading.py"))
            .known_defect(
                "E2E-D53",
                "the TUI shows no `Retrying` status while a failing request is retried",
            ),
        Scenario::new(
            "tui::degraded::a_refused_connection_ends_in_an_error",
            refused,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new("tui::idle::at_most_4_kib_in_10_s", idle)
            .port_only(textual("vibe/cli/textual_ui/widgets/banner/petit_chat.py"))
            .known_defect(
                "E2E-D54",
                "an idle TUI writes about 24 KB in 10 s, a style reset and cursor move per frame",
            ),
    ]);
    scenarios
}

/// The `meta.json` files under the vibe home, whatever the harness's layout.
fn metas(world: &World) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![world.vibe_home().join("logs")];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| name == "meta.json") {
                found.push(path);
            }
        }
    }
    found
}

fn meta_title(path: &PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let meta: Value = serde_json::from_str(&text).ok()?;
    meta["title"].as_str().map(str::to_owned)
}

/// The first answer of a session asks the utility model for a title: before
/// the TUI exits, `meta.json` holds it and the window title carries it
/// (`_on_session_title_changed` in vibe/cli/textual_ui/app.py sets the
/// terminal's default title to the session title).
fn title(ctx: &mut Ctx, flags: &[&str]) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("the first answer"));
    let mut tui = launch(&world, flags)?;
    prompt(&mut tui, "name this session")?;
    tui.wait_text("the first answer", STEP)?;
    let until = Instant::now() + scaled(STEP);
    loop {
        let titles: Vec<Option<String>> = metas(&world).iter().map(meta_title).collect();
        if titles.iter().flatten().any(|title| title == TITLE) {
            break;
        }
        if Instant::now() >= until {
            return Err(format!("no meta.json holds the title: {titles:?}").into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let windows = tui.titles();
    ensure(windows.iter().any(|title| title.contains(TITLE)), || {
        format!("the window title never carried the session title: {windows:?}")
    })?;
    exit(&mut tui).map(drop)
}

/// A project `.vibe/` in an untrusted folder opens the trust dialog before
/// the composer; trusting it records the folder in `trusted_folders.toml`
/// (vibe/setup/trusted_folders/).
fn trust_dialog(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.write("work/.vibe/config.toml", "")?;
    let mut tui = world
        .command("vibe")?
        .spawn_tui(crate::tui::COLS, crate::tui::ROWS)?;
    let dialog = tui.wait_text("Trust this folder?", STEP)?;
    ensure(
        dialog.contains(".vibe/") && dialog.contains("Trust folder"),
        || {
            format!(
                "the dialog does not name the project config:\n{}",
                dialog.text()
            )
        },
    )?;
    tui.send(&[Key::Enter])?;
    tui.wait_text(crate::tui::READY, STEP)?;
    let trusted = std::fs::read_to_string(world.vibe_home().join("trusted_folders.toml"))?;
    let work = world.work().to_string_lossy().into_owned();
    ensure(trusted.contains(&format!("\"{work}\"")), || {
        format!("trusted_folders.toml does not trust the folder:\n{trusted}")
    })?;
    exit(&mut tui).map(drop)
}

/// A cache that saw an older version shows What's new once, marks the
/// running version seen, and the next start shows nothing
/// (`should_show_whats_new` in vibe/cli/update_notifier/whats_new.py).
fn whats_new(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    let version = env!("CARGO_PKG_VERSION");
    world.write(
        "vibe-home/cache.toml",
        format!(
            "[update_cache]\nlatest_version = \"{version}\"\nstored_at_timestamp = 1700000000\n\
             seen_whats_new_version = \"0.0.1\"\n"
        ),
    )?;
    let heading = format!("What's new in v{version}");
    let mut tui = launch(&world, &[])?;
    tui.wait_text(&heading, STEP)?;
    let cache = std::fs::read_to_string(world.vibe_home().join("cache.toml"))?;
    ensure(
        cache.contains(&format!("seen_whats_new_version = \"{version}\"")),
        || format!("the cache does not mark {version} seen:\n{cache}"),
    )?;
    exit(&mut tui)?;
    drop(tui);
    let tui = launch(&world, &[])?;
    std::thread::sleep(Duration::from_millis(500));
    let screen = tui.screen();
    ensure(!screen.contains(&heading), || {
        format!("What's new showed twice:\n{}", screen.text())
    })
}

/// `TERM_PROGRAM=vscode` shows no extension promotion: the port keeps that
/// divergence on purpose (the "VS Code extension promo" ledger row of
/// `docs/parity.md`, part 33).
fn vscode(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral().env("TERM_PROGRAM", "vscode"))?;
    let mut tui = launch(&world, &[])?;
    std::thread::sleep(Duration::from_millis(500));
    let screen = tui.screen();
    ensure(
        !screen.contains("VS Code") && !screen.contains("extension"),
        || format!("a promotion is on screen:\n{}", screen.text()),
    )?;
    exit(&mut tui).map(drop)
}

/// The sizes a resize goes through.
const SIZES: [(u16, u16); 3] = [(60, 20), (140, 40), (100, 30)];

/// Waits until the TUI repainted at `cols` columns: a rule spans the width.
fn repainted(tui: &mut Tui, cols: u16) -> Outcome {
    let width = usize::from(cols);
    tui.wait_screen(
        &format!("a rule {cols} columns wide"),
        |s| {
            s.size().0 == cols
                && s.rows()
                    .iter()
                    .any(|row| row.chars().count() == width && row.chars().all(|c| c == '─'))
        },
        STEP,
    )?;
    Ok(())
}

/// A paced answer resized through 60x20, 140x40, and 100x30 while it
/// streams: the answer completes whole, the prompt stays in the transcript,
/// and the next prompt reaches the platform.
fn resize_stream(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().replies([
        Reply::chunks((1..=12).map(|n| format!("s{n:02} "))).paced(200),
        Reply::text("after the resizes"),
    ]);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "stream while resizing")?;
    tui.wait_text("s02", STEP)?;
    for (cols, rows) in SIZES {
        tui.resize(cols, rows)?;
        repainted(&mut tui, cols)?;
    }
    let screen = tui.wait_text("s12", STEP)?;
    ensure(screen.contains("stream while resizing"), || {
        format!("the prompt left the transcript:\n{}", screen.text())
    })?;
    std::thread::sleep(Duration::from_millis(300));
    prompt(&mut tui, "next prompt")?;
    tui.wait_text("after the resizes", STEP)?;
    let sent = last_user(&turn_count(&world, 2)?[1]);
    ensure(sent == "next prompt", || {
        format!("the platform received {sent:?}")
    })?;
    exit(&mut tui).map(drop)
}

/// An approval dialog resized through the same sizes stays on screen and is
/// still answerable once its input grace is over.
fn resize_approval(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "touch resized.txt"})),
        Reply::text("approved after resizing"),
    ]);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "touch a file")?;
    tui.wait_text("Always allow", STEP)?;
    for (cols, rows) in SIZES {
        tui.resize(cols, rows)?;
        repainted(&mut tui, cols)?;
        tui.wait_text("touch resized.txt", STEP)?;
    }
    std::thread::sleep(Duration::from_millis(600));
    tui.send(&[Key::Enter])?;
    tui.wait_text("approved after resizing", STEP)?;
    ensure(world.work().join("resized.txt").exists(), || {
        "the approved command did not run".to_owned()
    })?;
    exit(&mut tui).map(drop)
}

/// Keys typed as soon as a resize repainted show in the composer without
/// another key to wake the TUI.
fn typed_after_resize(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    let mut tui = launch(&world, &[])?;
    tui.resize(60, 20)?;
    tui.type_text("abc")?;
    tui.wait_text("> abc", Duration::from_secs(3))?;
    exit(&mut tui).map(drop)
}

/// A provider answering 500 on every attempt: within 10 s the loading status
/// reads `Retrying` (`RETRYING_LOADING_STATUS` in
/// vibe/cli/textual_ui/widgets/loading.py, set while the turn is retried).
fn retrying(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world
        .platform()
        .replies((0..40).map(|_| Reply::status(500)));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "fail every time")?;
    tui.wait_text("Retrying", Duration::from_secs(10))?;
    tui.send(&[Key::Esc])?;
    exit(&mut tui).map(drop)
}

/// A provider whose port refuses connections: once the retry budget is spent
/// the error is on screen within 10 s, and `/exit` still ends the TUI. Both
/// implementations retry a refused connection on the same budget (US-011,
/// `shared_with_reference`).
fn refused(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(
        ctx,
        Setup::mistral()
            .api_base("http://127.0.0.1:9/v1")
            .config("api_retry_max_elapsed_time = 3"),
    )?;
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "nobody listens")?;
    tui.wait_screen(
        "an error",
        |s| s.contains("rror") || s.contains("refused"),
        Duration::from_secs(10),
    )?;
    exit(&mut tui).map(drop)
}

/// A started TUI left idle for 10 s writes at most 4 KiB.
fn idle(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    let mut tui = launch(&world, &[])?;
    std::thread::sleep(Duration::from_secs(1));
    let before = tui.bytes_written();
    std::thread::sleep(Duration::from_secs(10));
    let written = tui.bytes_written() - before;
    ensure(written <= 4096, || {
        format!("the idle TUI wrote {written} bytes in 10 s")
    })?;
    exit(&mut tui).map(drop)
}
