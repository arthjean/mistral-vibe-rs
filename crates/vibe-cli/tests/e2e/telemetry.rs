//! US-031: the telemetry events `vibe -p` and the TUI send to the datalake
//! route, compared as the multiset of event names the reference sends for the
//! same sequence.
//!
//! Every multiset below was measured against the pinned reference on
//! 2026-10-29: `vibe -p` through `VIBE_E2E_SUBJECT=reference`, the TUI once
//! through the PTY driver, since the suite does not drive the reference's
//! Textual UI. The events are those `vibe/core/telemetry/send.py` sends.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Key, Outcome, Reply, RouteReply, Scenario, Setup, World, ensure};

use crate::cli::{chats, exits, session_id, vibe};
use crate::tui::{STEP, exit, launch, prompt, textual, tui_world};

/// The plan `/api/vibe/whoami` reports in these worlds, and the label the
/// reference derives from it (`vibe/setup/auth/whoami.py`).
const PLAN: (&str, &str) = ("chat", "INDIVIDUAL");
const PLAN_LABEL: &str = "Pro";

/// A run whose turn calls `bash` and answers, on either harness.
const TOOL_TURN: &[(&str, usize)] = &[
    ("vibe.new_session", 1),
    ("vibe.ready", 1),
    ("vibe.request_sent", 2),
    ("vibe.tool_call_finished", 1),
    ("vibe.session_closed", 1),
];

/// A continued or resumed run on the default harness: the session is not new
/// and the client is not announced again.
const UNIFIED_CONTINUED: &[(&str, usize)] = &[("vibe.request_sent", 1), ("vibe.session_closed", 1)];

/// The legacy harness announces every run, continued or not.
const LEGACY_CONTINUED: &[(&str, usize)] = &[
    ("vibe.new_session", 1),
    ("vibe.ready", 1),
    ("vibe.request_sent", 1),
    ("vibe.session_closed", 1),
];

/// A run whose managed-configuration route fails: the outcome is reported
/// (`vibe/app_server/_admin_config.py`) and the turn runs.
const MANAGED_CONFIG_FAILED: &[(&str, usize)] = &[
    ("vibe.new_session", 1),
    ("vibe.ready", 1),
    ("vibe.admin_config_applied", 1),
    ("vibe.request_sent", 1),
    ("vibe.session_closed", 1),
];

/// The reference answered in 3.1 s with the managed-configuration route
/// failing (1.6 s with it answering), so an answer must come within twice
/// that.
const FIRST_ANSWER: Duration = Duration::from_secs(6);

/// The TUI started, a prompt answered (its title request counts), a second
/// prompt interrupted with Ctrl+C, `/clear`, then `/exit`.
const TUI_SEQUENCE: &[(&str, usize)] = &[
    ("vibe.new_session", 2),
    ("vibe.ready", 1),
    ("vibe.startup", 1),
    ("vibe.request_sent", 3),
    ("vibe.user_cancelled_action", 1),
    ("vibe.slash_command_used", 2),
    ("vibe.session_closed", 2),
];

const D29: &str = "E2E-D29";

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new(
            "telemetry::programmatic::unified::a_tool_turn_sends_the_reference_event_set",
            |ctx| tool_turn(ctx, false),
        )
        .known_defect(
            D29,
            "the default harness sends no `vibe.request_sent` and no `vibe.tool_call_finished`",
        ),
        Scenario::new(
            "telemetry::programmatic::unified::continue_announces_no_new_session",
            |ctx| continued(ctx, false, Continuation::Latest),
        )
        .known_defect(
            D29,
            "the default harness sends `vibe.new_session` and `vibe.ready` again on `-c`, and no \
             `vibe.request_sent`",
        ),
        Scenario::new(
            "telemetry::programmatic::unified::resume_announces_no_new_session",
            |ctx| continued(ctx, false, Continuation::Resume),
        )
        .known_defect(
            D29,
            "the default harness sends `vibe.new_session` and `vibe.ready` again on `--resume`, \
             and no `vibe.request_sent`",
        ),
        Scenario::new(
            "telemetry::programmatic::legacy::a_tool_turn_sends_the_reference_event_set",
            |ctx| tool_turn(ctx, true),
        ),
        Scenario::new(
            "telemetry::programmatic::legacy::continue_announces_the_session_again",
            |ctx| continued(ctx, true, Continuation::Latest),
        ),
        Scenario::new(
            "telemetry::programmatic::legacy::resume_announces_the_session_again",
            |ctx| continued(ctx, true, Continuation::Resume),
        ),
        Scenario::new("telemetry::silent::telemetry_disabled", |ctx| {
            silent(ctx, Silent::Disabled)
        }),
        Scenario::new("telemetry::silent::no_mistral_key", |ctx| {
            silent(ctx, Silent::NoKey)
        }),
        Scenario::new("telemetry::silent::a_third_party_provider_only", |ctx| {
            silent(ctx, Silent::ThirdParty)
        }),
        Scenario::new(
            "telemetry::managed_config::a_failing_route_keeps_the_event_set_and_the_first_answer",
            managed_config_failing,
        )
        .known_defect(
            D29,
            "the default harness sends no `vibe.admin_config_applied` and no `vibe.request_sent`",
        ),
        Scenario::new(
            "telemetry::tui::a_prompt_an_interrupt_and_clear_send_the_reference_event_set",
            tui_sequence,
        )
        .port_only(textual("vibe/core/telemetry/send.py"))
        .known_defect(
            D29,
            "the TUI on the default harness sends no `vibe.request_sent`, and refuses `/clear` \
             after an interrupt (E2E-D55)",
        ),
        Scenario::new(
            "tui::interrupt::clear_runs_after_an_interrupt",
            clear_after_interrupt,
        )
        .port_only(textual("vibe/cli/textual_ui/app.py"))
        .known_defect(
            "E2E-D55",
            "after Ctrl+C interrupts a turn, the TUI refuses `/clear` as a queued slash \
                 command",
        ),
    ]
}

/// A Mistral world whose console reports a plan.
fn planned(ctx: &mut Ctx, setup: Setup) -> Outcome<World> {
    let world = ctx.world_with(setup)?;
    world.platform().route(
        "whoami",
        RouteReply::json(200, &json!({"plan_type": PLAN.0, "plan_name": PLAN.1})),
    );
    Ok(world)
}

/// The event names of `events`, counted.
pub fn multiset(events: &[Value]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        let name = event["event"].as_str().unwrap_or("<unnamed>").to_owned();
        *counts.entry(name).or_insert(0) += 1;
    }
    counts
}

/// Fails unless `events` name exactly `expected`, quoting both.
pub fn same_events(what: &str, events: &[Value], expected: &[(&str, usize)]) -> Outcome {
    let got = multiset(events);
    let want: BTreeMap<String, usize> = expected
        .iter()
        .map(|&(name, count)| (name.to_owned(), count))
        .collect();
    ensure(got == want, || {
        format!("{what} sent the events {got:?}, the reference sends {want:?}")
    })
}

/// Fails unless every event of `events` carries the label of the console's
/// plan.
fn every_event_carries_the_plan(events: &[Value]) -> Outcome {
    let unplanned: Vec<String> = events
        .iter()
        .filter(|event| event["properties"]["user_plan"] != PLAN_LABEL)
        .map(|event| {
            format!(
                "{} (user_plan {})",
                event["event"], event["properties"]["user_plan"]
            )
        })
        .collect();
    ensure(unplanned.is_empty(), || {
        format!("events without user_plan {PLAN_LABEL:?}: {unplanned:?}")
    })
}

fn harness_flags(legacy: bool) -> &'static [&'static str] {
    if legacy { &["--legacy-harness"] } else { &[] }
}

fn tool_turn(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = planned(ctx, Setup::mistral())?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "true"})),
        Reply::text("measured"),
    ]);
    world.platform().exact();
    let mut arguments = vec!["-p", "hello", "--output", "text", "--auto-approve"];
    arguments.extend(harness_flags(legacy));
    exits(&vibe(&world, &arguments)?, 0)?;
    let events = world.platform().telemetry();
    same_events("a tool turn", &events, TOOL_TURN)?;
    let requests = chats(&world).len();
    let sent = multiset(&events)
        .get("vibe.request_sent")
        .copied()
        .unwrap_or(0);
    ensure(sent == requests, || {
        format!("{sent} vibe.request_sent event(s) for {requests} chat request(s)")
    })?;
    let finished = events
        .iter()
        .find(|event| event["event"] == "vibe.tool_call_finished")
        .ok_or("no vibe.tool_call_finished")?;
    let properties = &finished["properties"];
    ensure(
        properties["tool_name"] == "bash"
            && properties["status"] == "success"
            && properties["decision"] == "execute",
        || format!("the approved bash call finished as {properties}"),
    )?;
    // The legacy harness announces the client before the plan resolves, so
    // its `vibe.ready` carries none in the reference either.
    let planned: Vec<Value> = events
        .into_iter()
        .filter(|event| !legacy || event["event"] != "vibe.ready")
        .collect();
    every_event_carries_the_plan(&planned)
}

#[derive(Clone, Copy)]
enum Continuation {
    /// `-c`
    Latest,
    /// `--resume <full id>`
    Resume,
}

fn continued(ctx: &mut Ctx, legacy: bool, continuation: Continuation) -> Outcome {
    let world = planned(ctx, Setup::mistral())?;
    world
        .platform()
        .replies([Reply::text("first answer"), Reply::text("second answer")]);
    world.platform().exact();
    let mut first = vec!["-p", "first question", "--output", "json"];
    first.extend(harness_flags(legacy));
    let output = vibe(&world, &first)?;
    exits(&output, 0)?;
    let id = session_id(&output.stdout)?;
    let from = world.platform().telemetry().len();
    let mut second = vec!["-p", "second question", "--output", "text"];
    match continuation {
        Continuation::Latest => second.push("-c"),
        Continuation::Resume => second.extend(["--resume", id.as_str()]),
    }
    second.extend(harness_flags(legacy));
    exits(&vibe(&world, &second)?, 0)?;
    let events = world.platform().telemetry().split_off(from);
    let expected = if legacy {
        LEGACY_CONTINUED
    } else {
        UNIFIED_CONTINUED
    };
    same_events("the continued run", &events, expected)?;
    every_event_carries_the_plan(&events)
}

#[derive(Clone, Copy)]
enum Silent {
    Disabled,
    NoKey,
    ThirdParty,
}

fn silent(ctx: &mut Ctx, case: Silent) -> Outcome {
    let world = ctx.world_with(match case {
        Silent::Disabled => Setup::mistral().config("enable_telemetry = false"),
        Silent::NoKey => Setup::mistral(),
        Silent::ThirdParty => Setup::default(),
    })?;
    world.platform().reply(Reply::text("silent"));
    let mut command = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"]);
    if let Silent::NoKey = case {
        command = command.env_remove("MISTRAL_API_KEY");
    }
    // Without its key the Mistral provider runs no turn in either
    // implementation.
    let code = if let Silent::NoKey = case { 1 } else { 0 };
    exits(&command.run()?, code)?;
    let events = world.platform().telemetry();
    ensure(events.is_empty(), || {
        format!("the datalake route received {:?}", multiset(&events))
    })
}

fn managed_config_failing(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::mistral())?;
    world.platform().route(
        "managed_config",
        RouteReply::json(500, &json!({"detail": "e2e failure"})),
    );
    world.platform().reply(Reply::text("answered"));
    let started = Instant::now();
    let output = vibe(&world, &["-p", "hello", "--output", "text"])?;
    let elapsed = started.elapsed();
    exits(&output, 0)?;
    ensure(output.stdout == "answered\n", || output.stdout.clone())?;
    ensure(elapsed <= vibe_e2e::scaled(FIRST_ANSWER), || {
        format!("the answer took {elapsed:?}; the reference answers within {FIRST_ANSWER:?}")
    })?;
    same_events(
        "a run with the managed configuration failing",
        &world.platform().telemetry(),
        MANAGED_CONFIG_FAILED,
    )
}

/// A TUI on a planned Mistral world that answered `hello` and was interrupted
/// with Ctrl+C three pieces into its second answer.
fn interrupted_tui(ctx: &mut Ctx) -> Outcome<(World, vibe_e2e::Tui)> {
    let world = tui_world(ctx, Setup::mistral())?;
    world.platform().route(
        "whoami",
        RouteReply::json(200, &json!({"plan_type": PLAN.0, "plan_name": PLAN.1})),
    );
    world.platform().replies([
        Reply::text("first answer"),
        Reply::chunks((1..=60).map(|n| format!("piece{n} "))).paced(200),
    ]);
    let mut tui = launch(&world, &[])?;
    prompt(&mut tui, "hello")?;
    tui.wait_text("first answer", STEP)?;
    prompt(&mut tui, "stream")?;
    tui.wait_text("piece3 ", STEP)?;
    tui.send(&[Key::Ctrl('c')])?;
    tui.wait_text("Interrupted", STEP)?;
    Ok((world, tui))
}

/// Submits `/clear` and waits until the conversation is gone from the screen.
fn clear(tui: &mut vibe_e2e::Tui) -> Outcome {
    prompt(tui, "/clear")?;
    tui.wait_screen(
        "the conversation cleared",
        |screen| !screen.contains("first answer"),
        STEP,
    )
    .map(drop)
}

fn tui_sequence(ctx: &mut Ctx) -> Outcome {
    let (world, mut tui) = interrupted_tui(ctx)?;
    clear(&mut tui)?;
    exit(&mut tui)?;
    same_events(
        "the TUI sequence",
        &world.platform().telemetry(),
        TUI_SEQUENCE,
    )
}

/// The reference runs `/clear` once the interrupted turn stopped.
fn clear_after_interrupt(ctx: &mut Ctx) -> Outcome {
    let (_, mut tui) = interrupted_tui(ctx)?;
    clear(&mut tui)?;
    exit(&mut tui).map(drop)
}
