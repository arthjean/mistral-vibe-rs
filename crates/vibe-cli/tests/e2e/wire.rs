//! US-025: app-server framing, turn control, approvals through
//! `callback/call`, the state notifications project, leases and marks, and
//! the response class of every routed method.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_e2e::{Answer, Ctx, Outcome, Reply, Rpc, Scenario, World, ensure};

use crate::app_server::{
    TURN, history, listed, server, start, start_turn, state_id, stop, turn, turn_end,
};
use crate::cli::{content_text, with_role};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for &(name, frame, answer) in FRAMES {
        let scenario = Scenario::new(format!("app_server::wire::{name}"), move |ctx| {
            framing(ctx, frame(), answer)
        });
        scenarios.push(if FRAME_DEFECTS.contains(&name) {
            framing_defect(scenario)
        } else {
            scenario
        });
    }
    scenarios.push(framing_defect(Scenario::new(
        "app_server::wire::a_12_mb_image_attachment",
        large_image,
    )));
    scenarios.push(Scenario::new(
        "app_server::projection::notifications_rebuild_the_session_read",
        projection,
    ));
    for &(decision, written, asks_again) in DECISIONS {
        let scenario = Scenario::new(format!("app_server::approval::{decision}"), move |ctx| {
            approval(ctx, decision, written, asks_again)
        });
        scenarios.push(if decision == "approve_permanently" {
            scenario.known_defect(
                "E2E-D49",
                "approving `touch first.txt` permanently persists `touch *` where the \
                 reference persists `touch`",
            )
        } else {
            scenario
        });
    }
    scenarios.extend([
        Scenario::new("app_server::control::interrupt_ends_the_turn", interrupt),
        Scenario::new(
            "app_server::control::steer_and_queue_reach_the_model",
            steer_and_queue,
        ),
        Scenario::new(
            "app_server::control::an_interrupted_shell_command_stops",
            shell_interrupt,
        ),
        Scenario::new(
            "app_server::lease::a_second_server_meets_an_open_session",
            second_server,
        ),
    ]);
    scenarios.push(Scenario::new(
        "app_server::sweep::covers_the_routed_inventory",
        covers_the_routed_inventory,
    ));
    for &(method, bare, rooted) in SWEEP {
        let scenario = Scenario::new(format!("app_server::sweep::{method}"), move |ctx| {
            sweep(ctx, method, bare, rooted)
        });
        scenarios.push(if method == "session/compact" {
            scenario.known_defect(
                "E2E-D48",
                "`session/compact` on a session with no turn never answers",
            )
        } else if SWEEP_DEFECTS.contains(&method) {
            scenario.known_defect(
                "E2E-D47",
                "the response class of a routed method differs from the reference's",
            )
        } else {
            scenario
        });
    }
    scenarios
}

/// Declares what ends the connection on the port while E2E-D46 is open.
fn framing_defect(scenario: Scenario) -> Scenario {
    scenario.known_defect(
        "E2E-D46",
        "a frame above 8 MiB, a message without `jsonrpc`, or a byte-order mark ends the \
         app-server connection",
    )
}

const FRAME_DEFECTS: [&str; 3] = [
    "a_9_mb_frame",
    "a_message_without_jsonrpc",
    "a_byte_order_mark",
];

/// A `session/list` request with id 5, padded to `bytes` with a cursor.
fn padded(bytes: usize) -> Vec<u8> {
    let cursor = "x".repeat(bytes);
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"session/list\",\"params\":{{\"cursor\":\"{cursor}\"}}}}\n"
    )
    .into_bytes()
}

/// What the reference does with one frame, measured on 2026-10-29.
#[derive(Clone, Copy)]
enum Answered {
    /// Answers id 5 with a result and keeps the connection.
    Result,
    /// Ends the connection.
    Closes,
}

/// `(case, frame, answer)`.
#[allow(clippy::type_complexity)]
const FRAMES: &[(&str, fn() -> Vec<u8>, Answered)] = &[
    ("a_9_mb_frame", || padded(9 * 1024 * 1024), Answered::Result),
    (
        "a_message_without_jsonrpc",
        || {
            br#"{"id":5,"method":"session/list","params":{}}"#.iter().chain(b"\n").copied().collect()
        },
        Answered::Result,
    ),
    (
        "a_byte_order_mark",
        || {
            "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"session/list\",\"params\":{}}\n"
                .into()
        },
        Answered::Result,
    ),
    (
        "a_crlf_line_ending",
        || b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"session/list\",\"params\":{}}\r\n".to_vec(),
        Answered::Result,
    ),
    (
        "a_batch_array",
        || b"[{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"session/list\",\"params\":{}}]\n".to_vec(),
        Answered::Closes,
    ),
    (
        "a_null_id",
        || {
            b"{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"session/list\",\"params\":{}}\n"
                .to_vec()
        },
        Answered::Closes,
    ),
];

/// The next response, skipping notifications, or `None` once the server
/// ended the connection.
fn next_response(rpc: &mut Rpc) -> Outcome<Option<Value>> {
    loop {
        let line = match rpc.raw_line(TURN) {
            Ok(line) => line,
            Err(failure) if failure.0.contains("exited") => return Ok(None),
            Err(failure) => return Err(failure),
        };
        let message: Value = serde_json::from_str(&line)?;
        if message.get("method").is_none() {
            return Ok(Some(message));
        }
    }
}

fn framing(ctx: &mut Ctx, frame: Vec<u8>, answer: Answered) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = server(&world, &[])?;
    // A server that hangs up mid-frame breaks the pipe; what it then wrote
    // tells the outcome.
    let written = rpc.write_raw(&frame);
    match answer {
        Answered::Result => {
            written?;
            let response = next_response(&mut rpc)?
                .ok_or("the server ended the connection instead of answering")?;
            ensure(
                response["id"] == 5 && response.get("result").is_some(),
                || format!("the frame was answered with {response}"),
            )?;
            rpc.call("session/list", json!({}))?;
            stop(rpc)
        }
        Answered::Closes => {
            let response = next_response(&mut rpc)?;
            ensure(response.is_none(), || {
                format!("the server kept the connection and answered {response:?}")
            })
        }
    }
}

fn large_image(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("seen"));
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    let data = "A".repeat(12 * 1024 * 1024);
    let started = rpc.call(
        "turn/start",
        json!({"sessionId": session, "message": [
            {"type": "text", "text": "look at this"},
            {"type": "image", "attachment": {
                "source": {"kind": "inline", "data": data},
                "alias": "large.png", "mimeType": "image/png"}},
        ]}),
    )?;
    ensure(started["turn"]["id"].is_string(), || {
        format!("turn/start with a 12 MB image answered {started}")
    })?;
    rpc.call("session/list", json!({}))?;
    Ok(())
}

/// Applies one RFC 6902 operation (`add`, `replace`, `remove`) to `target`.
fn apply(target: &mut Value, operation: &Value) -> Outcome {
    let path = operation["path"].as_str().ok_or("a patch names no path")?;
    let mut tokens: Vec<String> = path
        .split('/')
        .skip(1)
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect();
    let last = tokens.pop().ok_or("a patch targets the whole document")?;
    let mut parent = &mut *target;
    for token in &tokens {
        parent = match parent {
            Value::Object(map) => map.entry(token.clone()).or_insert(json!({})),
            Value::Array(items) => token
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index))
                .ok_or_else(|| format!("no item {token} in {path}"))?,
            _ => return Err(format!("{path} crosses a scalar").into()),
        };
    }
    let op = operation["op"].as_str().unwrap_or_default();
    match (parent, op) {
        (Value::Object(map), "remove") => {
            map.remove(&last);
        }
        (Value::Object(map), _) => {
            map.insert(last, operation["value"].clone());
        }
        (Value::Array(items), "remove") => {
            let index: usize = last.parse().map_err(|_| format!("bad index in {path}"))?;
            if index < items.len() {
                items.remove(index);
            }
        }
        (Value::Array(items), "add") if last == "-" => items.push(operation["value"].clone()),
        (Value::Array(items), op) => {
            let index: usize = last.parse().map_err(|_| format!("bad index in {path}"))?;
            if op == "add" {
                items.insert(index.min(items.len()), operation["value"].clone());
            } else if let Some(item) = items.get_mut(index) {
                *item = operation["value"].clone();
            }
        }
        _ => return Err(format!("{path} has no container").into()),
    }
    Ok(())
}

/// The session and history a client rebuilds from `initial` and the
/// notifications that followed it.
fn rebuild(
    initial: &Value,
    notifications: &[Value],
    session: &str,
) -> Outcome<(Value, Vec<Value>)> {
    let mut state = initial["session"].clone();
    let mut entries: Vec<Value> = initial["history"].as_array().cloned().unwrap_or_default();
    for message in notifications {
        let params = &message["params"];
        if params["sessionId"] != session {
            continue;
        }
        match message["method"].as_str() {
            Some("session/updated") => {
                for operation in params["patch"].as_array().into_iter().flatten() {
                    apply(&mut state, operation)?;
                }
            }
            Some("history/entryAdded") => entries.push(params["entry"].clone()),
            Some("history/entryUpdated") => {
                let entry = entries
                    .iter_mut()
                    .find(|entry| entry["id"] == params["entryId"])
                    .ok_or_else(|| format!("an update names an unknown entry: {params}"))?;
                for operation in params["patch"].as_array().into_iter().flatten() {
                    apply(entry, operation)?;
                }
            }
            _ => {}
        }
    }
    Ok((state, entries))
}

fn projection(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "echo projected"})),
        Reply::text("projected answer"),
    ]);
    let mut rpc = server(&world, &[])?;
    let started = rpc.call(
        "session/start",
        json!({"agentConfig": {"cwd": world.work(), "agent": "auto-approve"}}),
    )?;
    let session = state_id(&started)?;
    let from = rpc.notifications().len();
    turn(&mut rpc, &session, "project this")?;
    rpc.call(
        "session/rename",
        json!({"sessionId": session, "title": "Projected"}),
    )?;
    rpc.call("session/pin", json!({"sessionId": session, "pinned": true}))?;
    rpc.call("session/markAsSeen", json!({"sessionId": session}))?;
    let read = rpc.call("session/read", json!({"sessionId": session}))?;
    // Every notification the operations raised precedes the read's answer.
    let (state, entries) = rebuild(&started["state"], &rpc.notifications()[from..], &session)?;
    // Neither implementation patches the model or the reasoning effort into
    // the session (measured 2026-10-29); the read resolves both.
    let mut read_session = read["state"]["session"].clone();
    for derived in ["model", "reasoningEffort"] {
        read_session[derived] = state[derived].clone();
    }
    ensure(state == read_session, || {
        format!(
            "the rebuilt session differs from session/read:\nrebuilt {state}\nread    {}",
            read["state"]["session"]
        )
    })?;
    ensure(
        Value::Array(entries.clone()) == read["state"]["history"],
        || {
            format!(
                "the rebuilt history differs from session/read:\nrebuilt {}\nread    {}",
                Value::Array(entries),
                read["state"]["history"]
            )
        },
    )?;
    stop(rpc)
}

/// `(decision, file written, asked again on the next call)`, measured on
/// 2026-10-29.
const DECISIONS: &[(&str, bool, bool)] = &[
    ("deny", false, true),
    ("approve", true, true),
    ("approve_for_session", true, false),
    ("approve_permanently", true, false),
];

/// A server whose client takes approval callbacks and answers each with
/// `decision`.
fn approving_server(world: &World, decision: &'static str) -> Outcome<Rpc> {
    let child = world.command("vibe-app-server")?.stdin_piped().spawn()?;
    let mut rpc = Rpc::new(child);
    rpc.call(
        "initialize",
        json!({"clientInfo": {"name": "vibe-e2e", "version": "0"},
               "capabilities": {"callbackKinds": ["approval", "user_input"]}}),
    )?;
    rpc.notify("initialized", json!({}))?;
    rpc.respond("callback/call", move |params| {
        let callback = &params["callback"];
        Answer::result(json!({"callbackId": callback["callbackId"], "accepted": true})).then(
            "callback/result",
            json!({"sessionId": callback["sessionId"], "result": {
                "callbackId": callback["callbackId"],
                "output": {"type": "approval", "decision": {"type": decision}}}}),
        )
    });
    Ok(rpc)
}

fn approval(ctx: &mut Ctx, decision: &'static str, written: bool, asks_again: bool) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "touch first.txt"})),
        Reply::text("one"),
        Reply::tool_call("bash", json!({"command": "touch second.txt"})),
        Reply::text("two"),
    ]);
    world.platform().exact();
    let mut rpc = approving_server(&world, decision)?;
    let started = rpc.call(
        "session/start",
        json!({"agentConfig": {"cwd": world.work()}}),
    )?;
    let session = state_id(&started)?;
    turn(&mut rpc, &session, "first")?;
    ensure(rpc.exchanges().len() == 1, || {
        format!(
            "the first command raised {} callbacks",
            rpc.exchanges().len()
        )
    })?;
    turn(&mut rpc, &session, "second")?;
    let again = rpc.exchanges().len() == 2;
    ensure(again == asks_again, || {
        format!("after {decision} the next command asked again: {again}")
    })?;
    for name in ["first.txt", "second.txt"] {
        let exists = world.work().join(name).exists();
        ensure(exists == written, || {
            format!("after {decision} {name} exists: {exists}")
        })?;
    }
    let accepted = rpc.exchanges().iter().all(|e| {
        e.follow_ups
            .first()
            .is_some_and(|f| f["result"]["accepted"] == true)
    });
    ensure(accepted, || "a callback result was not accepted".to_owned())?;
    if decision == "approve_permanently" {
        let config: toml::Table = std::fs::read_to_string(world.vibe_home().join("config.toml"))?
            .parse()
            .map_err(|error| format!("config.toml does not parse: {error}"))?;
        let rules: Vec<&str> = config["tools"]["bash"]["allowlist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .filter(|rule| rule.starts_with("touch"))
            .collect();
        ensure(rules == ["touch"], || {
            format!("the permanent approval persisted the rules {rules:?}")
        })?;
    }
    stop(rpc)
}

fn interrupt(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("never shown"));
    let gate = world.platform().gate(1);
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    let id = start_turn(&mut rpc, &session, "interrupt me")?;
    world.platform().await_request(1, TURN)?;
    rpc.call(
        "turn/interrupt",
        json!({"sessionId": session, "expectedTurnId": id}),
    )?;
    gate.release();
    let end = turn_end(&mut rpc, &id)?;
    ensure(
        end["method"] == "turn/completed" && end["params"]["turn"]["status"] == "interrupted",
        || format!("the interrupted turn ended with {end}"),
    )?;
    stop(rpc)
}

/// The user texts of every chat request, in order.
fn user_texts(world: &World) -> Vec<Vec<String>> {
    world
        .platform()
        .chat_requests()
        .iter()
        .map(|entry| {
            with_role(entry, "user")
                .into_iter()
                .map(content_text)
                .collect()
        })
        .collect()
}

fn steer_and_queue(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "echo hi"})),
        Reply::text("after the steer"),
        Reply::text("queued answer"),
    ]);
    world.platform().exact();
    let gate = world.platform().gate(1);
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    let id = start_turn(&mut rpc, &session, "steer me")?;
    world.platform().await_request(1, TURN)?;
    rpc.call(
        "turn/steer",
        json!({"sessionId": session, "expectedTurnId": id,
               "message": [{"type": "text", "text": "steer text"}]}),
    )?;
    let queued = rpc.call(
        "session/turn/enqueue",
        json!({"sessionId": session, "entries": [
            {"role": "user", "content": [{"type": "text", "text": "queued text"}]}]}),
    )?;
    ensure(queued["queueItemId"].is_string(), || {
        format!("session/turn/enqueue answered {queued}")
    })?;
    gate.release();
    let end = turn_end(&mut rpc, &id)?;
    ensure(end["method"] == "turn/completed", || {
        format!("the steered turn ended with {end}")
    })?;
    rpc.await_notification(
        |m| m["method"] == "turn/completed" && m["params"]["turn"]["id"] != id.as_str(),
        TURN,
    )?;
    let texts = user_texts(&world);
    let expected = [
        vec!["steer me"],
        vec!["steer me", "steer text"],
        vec!["steer me", "steer text", "queued text"],
    ];
    ensure(texts == expected, || {
        format!("the requests carried the user texts {texts:?}")
    })?;
    stop(rpc)
}

/// Waits until `path` exists.
fn await_file(path: &std::path::Path) -> Outcome {
    let until = Instant::now() + vibe_e2e::scaled(TURN);
    while !path.exists() {
        if Instant::now() >= until {
            return Err(format!("{} did not appear", path.display()).into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn shell_interrupt(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    // The run is answered once the command ends.
    let run = rpc.send(
        "session/shellCommand",
        json!({"sessionId": session, "operationId": "op-1",
               "command": "touch started.txt; sleep 3; touch late.txt"}),
    )?;
    await_file(&world.work().join("started.txt"))?;
    let interrupted = rpc.call(
        "session/shellCommand",
        json!({"sessionId": session, "operationId": "op-1", "action": "interrupt"}),
    )?;
    ensure(interrupted["accepted"] == true, || {
        format!("the interrupt answered {interrupted}")
    })?;
    let ran = rpc.response(run)?;
    ensure(ran["result"]["accepted"] == true, || {
        format!("the interrupted run answered {ran}")
    })?;
    // The command would touch its file 3 s after it started.
    let late = world.work().join("late.txt");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        ensure(!late.exists(), || {
            "the interrupted command finished".to_owned()
        })?;
        std::thread::sleep(Duration::from_millis(100));
    }
    stop(rpc)
}

fn second_server(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("from the first server"));
    let mut first = server(&world, &[])?;
    let session = start(&mut first, &world)?;
    turn(&mut first, &session, "hold this session")?;
    let mut second = server(&world, &[])?;
    ensure(listed(&mut second)? == [session.clone()], || {
        "the second server does not list the open session".to_owned()
    })?;
    let resumed = second.request("session/resume", json!({"sessionId": session}))?;
    ensure(resumed["error"]["code"] == "conflict", || {
        format!("resuming a session another server holds answered {resumed}")
    })?;
    ensure(history(&mut second, &session)?.len() == 2, || {
        "the second server cannot read the open session".to_owned()
    })?;
    // The holder marks it.
    let renamed = first.call(
        "session/rename",
        json!({"sessionId": session, "title": "Marked"}),
    )?;
    let pinned = first.call("session/pin", json!({"sessionId": session, "pinned": true}))?;
    let seen = first.call("session/markAsSeen", json!({"sessionId": session}))?;
    let archived = first.call(
        "session/archive",
        json!({"sessionId": session, "archived": true}),
    )?;
    ensure(
        renamed["title"] == "Marked"
            && pinned["pinnedAt"].is_number()
            && seen == json!({})
            && archived["archivedAt"].is_number(),
        || format!("the marks answered {renamed} {pinned} {seen} {archived}"),
    )?;
    ensure(listed(&mut first)?.is_empty(), || {
        "an archived session is still listed".to_owned()
    })?;
    let worktrees = first.call("workspace/git/worktrees/list", json!({"cwd": world.work()}))?;
    ensure(
        worktrees["worktrees"] == json!([])
            && worktrees["repositoryRoot"] == world.work().to_string_lossy().as_ref(),
        || format!("workspace/git/worktrees/list answered {worktrees}"),
    )?;
    stop(second)?;
    stop(first)
}

/// The class of a response: `result`, its error code, or `closed` when the
/// server ended the connection.
fn class(rpc: &mut Rpc, method: &str, params: Value) -> Outcome<String> {
    let id = rpc.send(method, params)?;
    match rpc.response_within(id, Duration::from_secs(15)) {
        Ok(response) => Ok(match response.get("error") {
            Some(error) => match &error["code"] {
                Value::String(code) => code.clone(),
                code => code.to_string(),
            },
            None => "result".to_owned(),
        }),
        Err(failure) if failure.0.contains("exited") => Ok("closed".to_owned()),
        Err(failure) => Err(failure),
    }
}

fn sweep(ctx: &mut Ctx, method: &str, bare: &str, rooted: &str) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = server(&world, &[])?;
    let found = class(&mut rpc, method, json!({}))?;
    ensure(found == bare, || {
        format!("{method} without a root answered {found}, expected {bare}")
    })?;
    drop(rpc);
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    let found = class(&mut rpc, method, json!({"sessionId": session}))?;
    ensure(found == rooted, || {
        format!("{method} with a root answered {found}, expected {rooted}")
    })
}

/// Fails when a method enters or leaves the routed inventory without a
/// measured row in [`SWEEP`].
fn covers_the_routed_inventory(_: &mut Ctx) -> Outcome {
    let swept: Vec<&str> = SWEEP.iter().map(|&(method, _, _)| method).collect();
    let routed = vibe_protocol::SERVER_METHODS;
    let unswept: Vec<&&str> = routed.iter().filter(|m| !swept.contains(m)).collect();
    let unrouted: Vec<&&str> = swept.iter().filter(|m| !routed.contains(m)).collect();
    ensure(unswept.is_empty() && unrouted.is_empty(), || {
        format!("routed but not swept: {unswept:?}; swept but not routed: {unrouted:?}")
    })
}

/// Methods whose response class differs on the port, open as E2E-D47.
const SWEEP_DEFECTS: [&str; 11] = [
    "agents/list",
    "diagnostics/list",
    "feedback/record",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/turnDiff",
    "session/history/clear",
    "session/relocate",
    "tools/list",
];

/// `(method, class without a root, class with a root)`: the reference's
/// answer to `{}` on a fresh connection and to `{"sessionId": <root>}` on a
/// connection whose root is a new default-harness session, measured on
/// 2026-10-29.
const SWEEP: &[(&str, &str, &str)] = &[
    ("account/read", "conflict", "result"),
    ("agents/install", "conflict", "invalid_params"),
    ("agents/list", "result", "method_not_found"),
    ("agents/uninstall", "conflict", "invalid_params"),
    ("callback/result", "conflict", "invalid_params"),
    ("config/fields/read", "conflict", "result"),
    ("config/model/write", "conflict", "result"),
    ("config/proxy/read", "conflict", "result"),
    ("config/proxy/write", "conflict", "invalid_params"),
    ("config/read", "result", "result"),
    ("config/reload", "conflict", "result"),
    ("config/schema", "result", "invalid_params"),
    ("config/write", "conflict", "invalid_params"),
    (
        "connector_catalog/auth/request",
        "invalid_params",
        "invalid_params",
    ),
    ("connector_catalog/read", "result", "result"),
    ("connector_catalog/refresh", "result", "result"),
    (
        "connector_catalog/toggle",
        "invalid_params",
        "invalid_params",
    ),
    ("connectors/auth/read", "invalid_params", "invalid_params"),
    ("connectors/read", "invalid_params", "result"),
    ("connectors/refresh", "invalid_params", "invalid_params"),
    ("diagnostics/list", "conflict", "method_not_found"),
    ("diagnostics/logs/read", "conflict", "result"),
    ("events/read", "result", "invalid_params"),
    ("feedback/record", "conflict", "method_not_found"),
    ("feedback/shouldShow", "conflict", "result"),
    ("identity/read", "conflict", "result"),
    ("loops/clear", "conflict", "result"),
    ("loops/create", "conflict", "invalid_params"),
    ("loops/delete", "conflict", "invalid_params"),
    ("loops/list", "conflict", "result"),
    ("mcp/add", "invalid_params", "invalid_params"),
    ("mcp_catalog/add", "invalid_params", "invalid_params"),
    ("mcp_catalog/login", "invalid_params", "invalid_params"),
    ("mcp_catalog/logout", "invalid_params", "invalid_params"),
    ("mcp_catalog/read", "invalid_params", "result"),
    ("mcp_catalog/refresh", "invalid_params", "result"),
    ("mcp_catalog/remove", "invalid_params", "invalid_params"),
    ("mcp_catalog/toggle", "invalid_params", "invalid_params"),
    ("mcp/login", "invalid_params", "invalid_params"),
    ("mcp/logout", "invalid_params", "invalid_params"),
    ("mcp/read", "invalid_params", "result"),
    ("mcp/refresh", "invalid_params", "result"),
    ("mcp/toggle", "invalid_params", "invalid_params"),
    ("narration/summarize", "conflict", "invalid_params"),
    ("plugin_catalog/read", "invalid_params", "result"),
    ("plugin/info", "conflict", "result"),
    ("plugin/reload", "conflict", "result"),
    ("plugins/read", "invalid_params", "result"),
    ("projectLinks/create", "invalid_params", "invalid_params"),
    (
        "projectLinks/inspectRoot",
        "invalid_params",
        "invalid_params",
    ),
    ("projectLinks/link", "invalid_params", "invalid_params"),
    ("projectLinks/list", "result", "invalid_params"),
    (
        "projectLinks/picker/load",
        "invalid_params",
        "invalid_params",
    ),
    (
        "projectLinks/picker/loadMore",
        "invalid_params",
        "invalid_params",
    ),
    (
        "projectLinks/resolveRoot",
        "invalid_params",
        "invalid_params",
    ),
    ("projectLinks/save", "invalid_params", "invalid_params"),
    ("projectLinks/unlink", "invalid_params", "invalid_params"),
    ("providerAuth/read", "conflict", "result"),
    ("review/approve", "conflict", "method_not_found"),
    ("review/baseline", "conflict", "method_not_found"),
    ("review/hunks", "conflict", "method_not_found"),
    ("review/revert", "conflict", "method_not_found"),
    ("review/state", "conflict", "method_not_found"),
    ("review/turnDiff", "conflict", "method_not_found"),
    ("runtime/read", "conflict", "result"),
    ("session/agent/update", "conflict", "invalid_params"),
    ("session/archive", "invalid_params", "invalid_params"),
    (
        "session/backgroundProcess/output",
        "conflict",
        "invalid_params",
    ),
    (
        "session/backgroundProcess/stop",
        "conflict",
        "invalid_params",
    ),
    ("session/compact", "conflict", "internal_error"),
    ("session/context/inject", "conflict", "invalid_params"),
    ("session/continue", "not_found", "invalid_params"),
    ("session/delete", "invalid_params", "result"),
    ("session/fork", "invalid_params", "invalid_params"),
    ("session/history/clear", "internal_error", "result"),
    ("session/history/get", "invalid_params", "not_found"),
    ("session/history/list", "invalid_params", "result"),
    ("session/list", "result", "invalid_params"),
    ("session/log/read", "conflict", "result"),
    ("session/markAsSeen", "invalid_params", "result"),
    ("session/pin", "invalid_params", "invalid_params"),
    ("session/read", "invalid_params", "result"),
    ("session/ready/read", "conflict", "result"),
    ("session/ready/wait", "conflict", "result"),
    ("session/relocate", "invalid_params", "method_not_found"),
    ("session/rename", "invalid_params", "invalid_params"),
    ("session/resume", "invalid_params", "result"),
    ("session/rewind", "conflict", "invalid_params"),
    ("session/rewind/read", "conflict", "invalid_params"),
    ("session/settings/update", "conflict", "invalid_params"),
    ("session/shellCommand", "conflict", "invalid_params"),
    ("session/start", "result", "invalid_params"),
    ("session/stop", "conflict", "result"),
    ("session/title/update", "conflict", "method_not_found"),
    ("session/turn/enqueue", "conflict", "invalid_params"),
    ("session/turn/queue/read", "conflict", "result"),
    ("session/turn/queue/remove", "conflict", "invalid_params"),
    ("session/turn/queue/replace", "conflict", "invalid_params"),
    ("session/turn/queue/resume", "conflict", "result"),
    ("session/turn/queue/steer", "conflict", "invalid_params"),
    ("session/turns/list", "conflict", "result"),
    ("setup/status", "result", "invalid_params"),
    ("setup/store-credential", "invalid_params", "invalid_params"),
    ("setup/submit-choices", "result", "invalid_params"),
    ("shell/interrupt", "conflict", "method_not_found"),
    ("shell/run", "conflict", "method_not_found"),
    ("skills/catalog", "conflict", "result"),
    ("skills/convertLocal", "conflict", "invalid_params"),
    ("skills/detail", "conflict", "invalid_params"),
    ("skills/import", "conflict", "invalid_params"),
    ("skills/installed", "conflict", "result"),
    ("skills/list", "conflict", "result"),
    ("skills/remove", "conflict", "invalid_params"),
    ("skills/setAlias", "conflict", "invalid_params"),
    ("skills/setEnabled", "conflict", "invalid_params"),
    ("skills/setLatest", "conflict", "invalid_params"),
    ("skills/setVersion", "conflict", "invalid_params"),
    ("skills/updates", "conflict", "result"),
    ("skills/versions", "conflict", "invalid_params"),
    ("stats/read", "conflict", "method_not_found"),
    ("telemetry/record", "conflict", "invalid_params"),
    ("tools/list", "conflict", "method_not_found"),
    ("turn/interrupt", "conflict", "invalid_params"),
    ("turn/start", "conflict", "invalid_params"),
    ("turn/steer", "conflict", "invalid_params"),
    ("vibeCode/projects/cancel", "conflict", "invalid_params"),
    ("vibeCode/projects/create", "conflict", "invalid_params"),
    ("vibeCode/projects/loadMore", "conflict", "invalid_params"),
    ("vibeCode/projects/open", "conflict", "invalid_params"),
    ("vibeCode/projects/recover", "conflict", "invalid_params"),
    ("vibeCode/projects/select", "conflict", "invalid_params"),
    ("vibeCode/projects/unlink", "conflict", "invalid_params"),
    ("vibeCode/teleport/cancel", "conflict", "invalid_params"),
    (
        "vibeCode/teleport/push/respond",
        "conflict",
        "invalid_params",
    ),
    ("vibeCode/teleport/start", "conflict", "invalid_params"),
    (
        "workspace/git/checkouts",
        "invalid_params",
        "invalid_params",
    ),
    (
        "workspace/git/worktrees/limit/update",
        "invalid_params",
        "invalid_params",
    ),
    (
        "workspace/git/worktrees/list",
        "invalid_params",
        "invalid_params",
    ),
    ("workspace/git/worktrees/prune", "result", "invalid_params"),
    (
        "workspace/git/worktrees/reap/cancel",
        "invalid_params",
        "invalid_params",
    ),
    (
        "workspace/git/worktrees/reap",
        "invalid_params",
        "invalid_params",
    ),
    (
        "workspace/git/worktrees/remove",
        "invalid_params",
        "invalid_params",
    ),
    ("workspace/prompt/prepare", "conflict", "invalid_params"),
    (
        "workspace/trust/decision",
        "invalid_params",
        "invalid_params",
    ),
    ("workspace/trust/status", "result", "invalid_params"),
    (
        "workspace/trust/untrustedConfig",
        "result",
        "invalid_params",
    ),
];
