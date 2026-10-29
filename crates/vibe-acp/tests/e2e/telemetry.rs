//! US-031: the telemetry events `vibe-acp` sends to the datalake route,
//! compared as the multiset of event names the pinned reference sent for the
//! same exchange on 2026-10-29 (`vibe/core/telemetry/send.py`).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use vibe_e2e::{Answer, Ctx, Outcome, Reply, RouteReply, Rpc, Scenario, Setup, World, ensure};

use crate::editor::{editor, new_session, prompt_ends};

/// The plan `/api/vibe/whoami` reports, and the label the reference derives
/// from it (`vibe/setup/auth/whoami.py`).
const PLAN: (&str, &str) = ("chat", "INDIVIDUAL");
const PLAN_LABEL: &str = "Pro";

/// A session that ran one prompt calling `bash`, then the editor hung up.
const TOOL_PROMPT: &[(&str, usize)] = &[
    ("vibe.new_session", 1),
    ("vibe.ready", 1),
    ("vibe.request_sent", 2),
    ("vibe.tool_call_finished", 1),
    ("vibe.session_closed", 1),
];

/// A fresh `vibe-acp` that loaded that session, then the editor hung up: the
/// default harness announces nothing, the legacy one announces the session.
const UNIFIED_LOAD: &[(&str, usize)] = &[("vibe.session_closed", 1)];
const LEGACY_LOAD: &[(&str, usize)] = &[
    ("vibe.new_session", 1),
    ("vibe.ready", 1),
    ("vibe.session_closed", 1),
];

/// The event `_telemetry/send` forwards for a live session
/// (`vibe/acp/agent.py`, `ext_notification`).
const FORWARDED: &str = "vibe.at_mention_inserted";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (harness, flags) in HARNESSES {
        let tool = Scenario::new(
            format!(
                "acp::telemetry::{harness}::a_tool_prompt_and_a_load_send_the_reference_event_sets"
            ),
            move |ctx| tool_prompt_then_load(ctx, flags),
        );
        scenarios.push(if flags.is_empty() {
            tool.known_defect(
                "E2E-D29",
                "the default harness sends no `vibe.request_sent` and no \
                 `vibe.tool_call_finished`; the prompt also stops on E2E-D02",
            )
        } else {
            crate::routing::prompting(tool)
        });
        scenarios.push(Scenario::new(
            format!("acp::telemetry::{harness}::only_a_known_event_with_a_session_is_forwarded"),
            move |ctx| forwarded(ctx, flags),
        ));
    }
    scenarios
}

const HARNESSES: [(&str, &[&str]); 2] = [("unified", &[]), ("legacy", &["--legacy-harness"])];

/// A Mistral world whose console reports a plan.
fn planned(ctx: &mut Ctx) -> Outcome<World> {
    let world = ctx.world_with(Setup::mistral())?;
    world.platform().route(
        "whoami",
        RouteReply::json(200, &json!({"plan_type": PLAN.0, "plan_name": PLAN.1})),
    );
    Ok(world)
}

fn multiset(events: &[Value]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        let name = event["event"].as_str().unwrap_or("<unnamed>").to_owned();
        *counts.entry(name).or_insert(0) += 1;
    }
    counts
}

fn same_events(what: &str, events: &[Value], expected: &[(&str, usize)]) -> Outcome {
    let got = multiset(events);
    let want: BTreeMap<String, usize> = expected
        .iter()
        .map(|&(name, count)| (name.to_owned(), count))
        .collect();
    ensure(got == want, || {
        format!("{what} sent the events {got:?}, the reference sends {want:?}")
    })
}

/// Answers every permission request with "allow once".
fn allow(rpc: &mut Rpc) {
    rpc.respond("session/request_permission", |_| {
        Answer::result(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}))
    });
}

fn tool_prompt_then_load(ctx: &mut Ctx, flags: &[&str]) -> Outcome {
    let world = planned(ctx)?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "true"})),
        Reply::text("measured"),
    ]);
    world.platform().exact();
    let mut rpc = editor(&world, flags)?;
    allow(&mut rpc);
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "hello", "end_turn")?;
    // The reference flushes its pending events before it exits.
    rpc.hang_up()?;
    let events = world.platform().telemetry();
    same_events("a tool prompt", &events, TOOL_PROMPT)?;
    let unplanned: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] != "vibe.ready" || flags.is_empty())
        .filter(|event| event["properties"]["user_plan"] != PLAN_LABEL)
        .collect();
    ensure(unplanned.is_empty(), || {
        format!("events without user_plan {PLAN_LABEL:?}: {unplanned:?}")
    })?;
    let from = events.len();
    let mut rpc = editor(&world, flags)?;
    rpc.call(
        "session/load",
        json!({"cwd": world.work(), "mcpServers": [], "sessionId": session}),
    )?;
    rpc.hang_up()?;
    let loaded = world.platform().telemetry().split_off(from);
    let expected = if flags.is_empty() {
        UNIFIED_LOAD
    } else {
        LEGACY_LOAD
    };
    same_events("session/load", &loaded, expected)
}

/// A known event with a live session is forwarded; an unknown event and an
/// event without a session are not.
fn forwarded(ctx: &mut Ctx, flags: &[&str]) -> Outcome {
    let world = planned(ctx)?;
    let mut rpc = editor(&world, flags)?;
    let session = new_session(&mut rpc, &world)?;
    for params in [
        json!({"event": FORWARDED, "sessionId": session, "properties": {"nb_mentions": 1}}),
        json!({"event": "vibe.e2e_unknown", "sessionId": session, "properties": {}}),
        json!({"event": FORWARDED, "properties": {"nb_mentions": 2}}),
    ] {
        rpc.notify("_telemetry/send", params)?;
    }
    // A request answered after the notifications proves they were read.
    let listed = rpc.request("session/list", json!({"cwd": world.work()}))?;
    ensure(listed.get("result").is_some(), || {
        format!("session/list answered {listed}")
    })?;
    rpc.hang_up()?;
    let events = world.platform().telemetry();
    let sent: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == FORWARDED || event["event"] == "vibe.e2e_unknown")
        .collect();
    ensure(
        sent.len() == 1 && sent[0]["properties"]["nb_mentions"] == 1,
        || format!("the datalake route received {sent:?}, expected the one known event"),
    )?;
    ensure(
        sent[0]["properties"]["session_id"] == session.as_str(),
        || {
            format!(
                "the forwarded event names the session {}",
                sent[0]["properties"]["session_id"]
            )
        },
    )?;
    Ok(())
}
