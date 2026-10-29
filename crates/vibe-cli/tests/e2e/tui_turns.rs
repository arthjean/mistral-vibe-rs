//! US-028: approvals, `!` commands, interruption, rewind, and questions in
//! the shipped TUI, each after a model turn.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Key, Outcome, Reply, Scenario, Setup, Tui, ensure};

use crate::cli::content_text;
use crate::tui::{STEP, exit, launch, prompt, textual, tui_world, turn_count, turns};

/// The word that opens the banner under an interrupted answer: the reference's
/// `InterruptMessage` (vibe/cli/textual_ui/widgets/messages.py) reads
/// "Interrupted · What should Vibe do instead?".
const INTERRUPTED: &str = "Interrupted";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios: Vec<Scenario> = [
        Approval::Once,
        Approval::Session,
        Approval::Always,
        Approval::Deny,
        Approval::Esc,
    ]
    .into_iter()
    .map(|choice| {
        let scenario = Scenario::new(format!("tui::approval::{}", choice.name()), move |ctx| {
            approval(ctx, choice)
        })
        .port_only(textual("vibe/cli/textual_ui/widgets/approval_app.py"));
        if matches!(choice, Approval::Always) {
            scenario.known_defect(
                "E2E-D49",
                "\"Always allow\" for `touch marker.txt` persists `touch *` where the reference \
                 persists `touch`",
            )
        } else {
            scenario
        }
    })
    .collect();
    scenarios.extend([
        Scenario::new(
            "tui::shell::a_bang_command_after_a_turn_shows_its_output",
            bang_after_turn,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py"))
        .known_defect(
            "E2E-D23",
            "the output of a `!` command is invisible after the first model turn",
        ),
        Scenario::new("tui::interrupt::esc_stops_the_stream", |ctx| {
            interrupt(ctx, Key::Esc)
        })
        .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new("tui::interrupt::ctrl_c_stops_the_stream", |ctx| {
            interrupt(ctx, Key::Ctrl('c'))
        })
        .port_only(textual("vibe/cli/textual_ui/app.py")),
        Scenario::new("tui::rewind::legacy::in_the_live_session", |ctx| {
            rewind(ctx, Rewind::Live, &["--legacy-harness"])
        })
        .port_only(textual("vibe/cli/textual_ui/widgets/rewind_app.py")),
        Scenario::new("tui::rewind::in_the_live_session", |ctx| {
            rewind(ctx, Rewind::Live, &[])
        })
        .port_only(textual("vibe/cli/textual_ui/widgets/rewind_app.py"))
        .known_defect(
            "E2E-D22",
            "`/rewind` fails with \"Cannot find user entry\" and restores no file on the default \
             harness",
        ),
        Scenario::new("tui::rewind::after_continue", |ctx| {
            rewind(ctx, Rewind::Continued, &[])
        })
        .port_only(textual("vibe/cli/textual_ui/widgets/rewind_app.py"))
        .known_defect(
            "E2E-D22",
            "after `vibe -c`, `/rewind` offers no file to restore on the default harness",
        ),
        Scenario::new(
            "tui::question::down_then_enter_picks_the_second_option",
            |ctx| question_answer(ctx, false),
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/question_app.py")),
        // E2E-D26 did not reproduce on this sequence or three other timings
        // (US-028, 2026-10-29), so the trial guards the behavior undeclared.
        Scenario::new(
            "tui::question::an_arrow_typed_as_the_question_opens",
            |ctx| question_answer(ctx, true),
        )
        .port_only(textual("vibe/cli/textual_ui/widgets/question_app.py")),
    ]);
    scenarios
}

#[derive(Clone, Copy)]
enum Approval {
    Once,
    Session,
    Always,
    Deny,
    Esc,
}

impl Approval {
    fn name(self) -> &'static str {
        match self {
            Self::Once => "allow_once",
            Self::Session => "allow_for_the_session",
            Self::Always => "always_allow",
            Self::Deny => "deny_reaches_the_model",
            Self::Esc => "esc_denies",
        }
    }

    /// The keys that pick the choice in the dialog, whose options are listed
    /// in the reference's order: once, session, always, deny.
    fn keys(self) -> Vec<Key> {
        let down = |n: usize| std::iter::repeat_n(Key::Down, n);
        match self {
            Self::Once => vec![Key::Enter],
            Self::Session => down(1).chain([Key::Enter]).collect(),
            Self::Always => down(2).chain([Key::Enter]).collect(),
            Self::Deny => down(3).chain([Key::Enter]).collect(),
            Self::Esc => vec![Key::Esc],
        }
    }

    fn allows(self) -> bool {
        matches!(self, Self::Once | Self::Session | Self::Always)
    }
}

/// The tool messages a request carries.
fn tool_messages(entry: &vibe_e2e::Entry) -> Vec<String> {
    entry
        .messages()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(content_text)
        .collect()
}

/// How long a dialog ignores a selection once shown: `_INPUT_GRACE_PERIOD_S`
/// in vibe/cli/textual_ui/widgets/approval_app.py and question_app.py, so a
/// key meant for the composer never answers it.
const DIALOG_GRACE: Duration = Duration::from_millis(500);

/// Waits until the approval dialog for the `touch` call is on screen, then
/// out its input grace.
fn await_dialog(tui: &mut Tui, file: &str) -> Outcome {
    tui.wait_screen(
        "the approval dialog",
        |s| s.contains(&format!("touch {file}")) && s.contains("Always allow"),
        STEP,
    )?;
    std::thread::sleep(DIALOG_GRACE + Duration::from_millis(100));
    Ok(())
}

/// A `bash` call that needs approval, answered with each choice of the
/// dialog (vibe/cli/textual_ui/widgets/approval_app.py): the marker file
/// appears only after an allow; a denial, by the dialog or by Esc
/// (`_handle_approval_app_escape` in vibe/cli/textual_ui/app.py), reaches the
/// model as the call's result and the turn reaches its final answer. A second
/// identical call
/// asks again after "Allow once" and runs unasked after "Allow for session";
/// "Always allow" persists the rule the reference persists, `touch`, in the
/// bash allowlist of `config.toml`.
fn approval(ctx: &mut Ctx, choice: Approval) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "touch marker.txt"})),
        Reply::text("first turn done"),
    ]);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "make a marker")?;
    await_dialog(&mut tui, "marker.txt")?;
    ensure(!world.work().join("marker.txt").exists(), || {
        "the command ran before its approval".to_owned()
    })?;
    tui.send(&choice.keys())?;
    let marker = world.work().join("marker.txt");
    tui.wait_text("first turn done", STEP)?;
    ensure(marker.exists() == choice.allows(), || {
        format!(
            "after {}, the marker file exists: {}",
            choice.name(),
            marker.exists()
        )
    })?;
    let second = &turn_count(&world, 2)?[1];
    let results = tool_messages(second);
    ensure(results.len() == 1, || {
        format!("the call was answered by {results:?}")
    })?;
    if matches!(choice, Approval::Deny | Approval::Esc) {
        return exit(&mut tui).map(drop);
    }
    if matches!(choice, Approval::Always) {
        let config: toml::Table = std::fs::read_to_string(world.vibe_home().join("config.toml"))?
            .parse()
            .map_err(|error| format!("config.toml does not parse: {error}"))?;
        let rules: Vec<String> = config
            .get("tools")
            .and_then(|tools| tools.get("bash"))
            .and_then(|bash| bash.get("allowlist"))
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .filter(|rule| rule.starts_with("touch"))
            .map(str::to_owned)
            .collect();
        ensure(rules == ["touch"], || {
            format!("the persisted touch rules are {rules:?}")
        })?;
        return exit(&mut tui).map(drop);
    }
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "touch second.txt"})),
        Reply::text("second turn done"),
    ]);
    prompt(&mut tui, "make another")?;
    if matches!(choice, Approval::Once) {
        await_dialog(&mut tui, "second.txt")?;
        tui.send(&[Key::Enter])?;
    }
    tui.wait_text("second turn done", STEP)?;
    ensure(world.work().join("second.txt").exists(), || {
        "the second command did not run".to_owned()
    })?;
    exit(&mut tui)?;
    Ok(())
}

/// `!echo <marker>` after a completed model turn shows its output within 5 s,
/// with no continuity-loss notice (`_handle_bash_command` in
/// vibe/cli/textual_ui/app.py).
fn bang_after_turn(ctx: &mut Ctx) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().reply(Reply::text("model said hi"));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "hello")?;
    tui.wait_text("model said hi", STEP)?;
    prompt(&mut tui, "!echo e2e-bang-$((40 + 2))")?;
    let screen = tui.wait_text("e2e-bang-42", Duration::from_secs(5))?;
    ensure(!screen.contains("continuity was lost"), || {
        format!("a continuity-loss notice is on screen:\n{}", screen.text())
    })?;
    turn_count(&world, 1)?;
    exit(&mut tui)?;
    Ok(())
}

/// The pieces of a paced answer: `w01 ` to `w30 `.
fn piece(n: usize) -> String {
    format!("w{n:02} ")
}

/// A paced stream interrupted with `key` once three pieces are on screen: the
/// turn stops with the interruption banner, no piece served after it appears,
/// and a second press behaves as in the reference: Esc does nothing more,
/// while Ctrl+C on an empty composer asks to press it again to quit
/// (`action_interrupt` and `action_interrupt_or_quit` in
/// vibe/cli/textual_ui/app.py).
fn interrupt(ctx: &mut Ctx, key: Key) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world
        .platform()
        .reply(Reply::chunks((1..=30).map(piece)).paced(150));
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "stream a long answer")?;
    tui.wait_text(&piece(3), STEP)?;
    tui.send(std::slice::from_ref(&key))?;
    tui.wait_text(INTERRUPTED, STEP)?;
    // Events: the role chunk, then one per piece.
    let served = world.platform().events_served(1).saturating_sub(1);
    std::thread::sleep(Duration::from_millis(1500));
    let screen = tui.screen();
    for late in (served + 1)..=30 {
        ensure(!screen.contains(piece(late).trim_end()), || {
            format!(
                "{} was on screen though only {served} pieces were served before the \
                 interruption:\n{}",
                piece(late),
                screen.text()
            )
        })?;
    }
    ensure(world.platform().events_served(1) < 31, || {
        "the whole stream was read after the interruption".to_owned()
    })?;
    tui.send(std::slice::from_ref(&key))?;
    if key == Key::Esc {
        std::thread::sleep(Duration::from_millis(500));
        let after = tui.screen();
        ensure(!after.contains("again to quit"), || {
            format!("a second Esc armed the quit:\n{}", after.text())
        })?;
    } else {
        tui.wait_text("Press Ctrl+C again to quit", STEP)?;
    }
    turn_count(&world, 1)?;
    exit(&mut tui)?;
    Ok(())
}

/// A turn that wrote two files, rewound with "Edit & restore files to this
/// point" and then keeping the current session (vibe/cli/textual_ui/widgets/
/// rewind_app.py), in the live session or after `vibe -c`: the files return
/// to their state before the turn and the rewound prompt is in the composer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rewind {
    Live,
    Continued,
}

fn rewind(ctx: &mut Ctx, when: Rewind, harness: &[&str]) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.write("work/kept.txt", "before\n")?;
    // The unified `write_file` takes a `path` and overwrites; the legacy one
    // takes an absolute `file_path` and refuses an existing file, which its
    // `edit` changes instead (vibe/core/tools/builtins/write_file.py, edit.py).
    let turn = if harness.is_empty() {
        Reply::tool_call(
            "write_file",
            json!({"path": "kept.txt", "content": "after\n"}),
        )
        .and_tool_call(
            "write_file",
            json!({"path": "created.txt", "content": "new\n"}),
        )
    } else {
        Reply::tool_call(
            "edit",
            json!({"file_path": world.work().join("kept.txt"), "old_string": "before",
                   "new_string": "after"}),
        )
        .and_tool_call(
            "write_file",
            json!({"file_path": world.work().join("created.txt"), "content": "new\n"}),
        )
    };
    world
        .platform()
        .replies([turn, Reply::text("files written")]);
    let mut tui = launch(&world, harness)?;
    prompt(&mut tui, "rewrite the files")?;
    tui.wait_text("files written", STEP)?;
    ensure(
        std::fs::read_to_string(world.work().join("kept.txt"))? == "after\n",
        || "the turn did not rewrite kept.txt".to_owned(),
    )?;
    if when == Rewind::Continued {
        exit(&mut tui)?;
        drop(tui);
        tui = launch(&world, &[harness, &["-c"]].concat())?;
        tui.wait_text("files written", STEP)?;
    }
    prompt(&mut tui, "/rewind")?;
    tui.wait_text("Edit & restore files to this point", STEP)?;
    tui.send(&[Key::Enter])?;
    // The persistence step lists keeping the session first and forking second
    // (`_build_persistence_options`); the port words them differently.
    tui.wait_text("Fork", STEP)?;
    tui.send(&[Key::Enter])?;
    let screen = tui.wait_screen(
        "the rewound prompt in the composer",
        |s| {
            s.rows()
                .iter()
                .any(|row| row.trim_end() == "> rewrite the files")
        },
        STEP,
    )?;
    ensure(!screen.contains("files written"), || {
        format!("the rewound answer is still on screen:\n{}", screen.text())
    })?;
    let kept = std::fs::read_to_string(world.work().join("kept.txt"))?;
    ensure(kept == "before\n", || {
        format!("kept.txt was not restored: {kept:?}")
    })?;
    ensure(!world.work().join("created.txt").exists(), || {
        "created.txt was not removed".to_owned()
    })?;
    tui.send(&[Key::Ctrl('c')])?;
    tui.wait_screen(
        "an empty composer",
        |s| {
            !s.rows()
                .iter()
                .any(|row| row.trim_end() == "> rewrite the files")
        },
        STEP,
    )?;
    exit(&mut tui)?;
    Ok(())
}

/// The arguments of an `ask_user_question` call with two options
/// (vibe/questions.py).
fn question() -> Value {
    json!({"questions": [{
        "question": "Which color?",
        "header": "Color",
        "options": [
            {"label": "Red", "description": "the first option"},
            {"label": "Blue", "description": "the second option"},
        ],
    }]})
}

/// A question, arriving while the user types or not: Down pressed as the
/// dialog opens, then Enter within 500 ms once its input grace is over,
/// answer with the second option (vibe/cli/textual_ui/widgets/question_app.py;
/// a question that arrives during typing waits for a pause,
/// `_wait_for_typing_pause` in vibe/cli/textual_ui/app.py). Text typed
/// meanwhile stays in the composer.
fn question_answer(ctx: &mut Ctx, typing: bool) -> Outcome {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().replies([
        Reply::tool_call("ask_user_question", question()),
        Reply::text("thanks for answering"),
    ]);
    let gate = world.platform().gate(1);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "ask me something")?;
    world.platform().await_request(1, STEP)?;
    if typing {
        tui.type_text("typing meanwhile")?;
        tui.wait_text("typing meanwhile", STEP)?;
    }
    gate.release();
    // The transcript lists the question as soon as the call arrives; the
    // dialog opens after the typing pause, with its first option selected.
    tui.wait_text("› 1. Red", STEP)?;
    let opened = Instant::now();
    // Down lands inside the dialog's input grace, which holds back only a
    // selection (`action_select` in question_app.py), and Enter just after it.
    std::thread::sleep(Duration::from_millis(100));
    tui.send(&[Key::Down])?;
    let down = Instant::now();
    std::thread::sleep(
        (opened + DIALOG_GRACE + Duration::from_millis(20))
            .saturating_duration_since(Instant::now()),
    );
    tui.send(&[Key::Enter])?;
    ensure(down.elapsed() < Duration::from_millis(500), || {
        format!("Down and Enter were {:?} apart", down.elapsed())
    })?;
    tui.wait_text("thanks for answering", STEP)?;
    let second = &turns(&world)[1];
    let answers = tool_messages(second);
    ensure(
        answers.len() == 1 && answers[0].contains("Blue") && !answers[0].contains("Red"),
        || format!("the question was answered with {answers:?}"),
    )?;
    if typing {
        tui.wait_text("> typing meanwhile", STEP)?;
        tui.send(&[Key::Ctrl('c')])?;
        tui.wait_screen(
            "an empty composer",
            |s| !s.contains("> typing meanwhile"),
            STEP,
        )?;
    }
    exit(&mut tui)?;
    Ok(())
}
