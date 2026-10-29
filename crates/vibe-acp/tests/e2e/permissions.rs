//! US-022: permissions, cancellation, retries, MCP tools, and client
//! delegation, as an editor drives them.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_e2e::{Answer, Ctx, Mcp, Outcome, Reply, Rpc, Scenario, World, ensure, text_result};

use crate::editor::{PROMPT, editor, initialize, new_session, prompt, prompt_ends, spawn};
use crate::routing::{prompting, turns};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for &(name, answer) in ANSWERS {
        scenarios.push(prompting(Scenario::new(
            format!("acp::permissions::answer::{name}"),
            move |ctx| approval(ctx, answer),
        )));
    }
    scenarios.extend([
        prompting(Scenario::new(
            "acp::permissions::a_path_outside_the_work_tree_offers_path_scopes",
            outside_options,
        )),
        Scenario::new(
            "acp::cancel::a_running_command_and_its_children_stop",
            cancel_running,
        )
        .known_defect(
            "E2E-D21",
            "a `bash` command canceled with its prompt leaves its children alive",
        ),
        prompting(
            Scenario::new(
                "acp::cancel::a_pending_permission_answered_cancelled",
                cancel_pending,
            )
            .port_only(
                "the reference rejects the cancelled answer (`callback result has no pending \
                 registration`) and never ends the prompt; answered before the cancel, it fails \
                 the prompt with -32603 (measured 2026-10-29)",
            ),
        ),
        Scenario::new("acp::retries::a_500_then_a_200", |ctx| retried(ctx, 500)).known_defect(
            "E2E-D31",
            "`vibe-acp` sends no `_session/retrying` notification",
        ),
        Scenario::new("acp::retries::a_429_then_a_200", |ctx| retried(ctx, 429)).known_defect(
            "E2E-D31",
            "`vibe-acp` sends no `_session/retrying` notification",
        ),
        Scenario::new("acp::mcp::legacy::an_approved_config_tool_runs", legacy_mcp).known_defect(
            "E2E-D11",
            "on the legacy harness `vibe-acp` never asks permission for an MCP tool",
        ),
        prompting(Scenario::new(
            "acp::mcp::unified::an_editor_server_answers_through_run_typescript",
            editor_mcp,
        )),
        prompting(Scenario::new(
            "acp::client::legacy::reads_writes_and_commands_go_through_the_editor",
            client_delegation,
        )),
    ]);
    scenarios
}

/// What the editor answers a permission request with.
#[derive(Clone, Copy, Debug)]
enum Choice {
    Select(&'static str),
    /// The editor closed the dialog: the `cancelled` outcome, without
    /// canceling the prompt.
    Dismiss,
}

impl Choice {
    fn json(self) -> Value {
        match self {
            Self::Select(option) => json!({"outcome": {"outcome": "selected", "optionId": option}}),
            Self::Dismiss => json!({"outcome": {"outcome": "cancelled"}}),
        }
    }

    fn answer(self) -> Answer {
        Answer::result(self.json())
    }
}

/// Answers every permission request with `choice`.
fn answer_permissions(rpc: &mut Rpc, choice: Choice) {
    rpc.respond("session/request_permission", move |_| choice.answer());
}

/// The permission requests the editor answered so far.
fn asked(rpc: &Rpc) -> usize {
    rpc.exchanges()
        .iter()
        .filter(|e| e.method == "session/request_permission")
        .count()
}

/// `(case, answer, file written, asked again on the next call)`, measured
/// against the reference on 2026-10-29.
#[allow(clippy::type_complexity)]
const ANSWERS: &[(&str, (Choice, bool, bool))] = &[
    ("allow_once", (Choice::Select("allow_once"), true, true)),
    (
        "allow_always",
        (Choice::Select("allow_always"), true, false),
    ),
    ("reject_once", (Choice::Select("reject_once"), false, true)),
    ("dismissed", (Choice::Dismiss, false, true)),
];

fn approval(ctx: &mut Ctx, (choice, written, asks_again): (Choice, bool, bool)) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "touch first.txt"})),
        Reply::text("first done"),
        Reply::tool_call("bash", json!({"command": "touch second.txt"})),
        Reply::text("second done"),
    ]);
    world.platform().exact();
    let mut rpc = editor(&world, &[])?;
    answer_permissions(&mut rpc, choice);
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "make the first file", "end_turn")?;
    ensure(asked(&rpc) == 1, || {
        format!(
            "the first command raised {} permission requests",
            asked(&rpc)
        )
    })?;
    let first = world.work().join("first.txt").exists();
    ensure(first == written, || {
        format!("after {choice:?} first.txt exists: {first}, expected {written}")
    })?;
    prompt_ends(&mut rpc, &session, "make the second file", "end_turn")?;
    let again = asked(&rpc) == 2;
    ensure(again == asks_again, || {
        format!("after {choice:?} the next command asked again: {again}, expected {asks_again}")
    })?;
    let second = world.work().join("second.txt").exists();
    ensure(second == written, || {
        format!("after {choice:?} second.txt exists: {second}, expected {written}")
    })
}

/// The option ids the reference offers for a write outside the work tree,
/// measured on 2026-10-29.
const OUTSIDE_OPTIONS: [&str; 4] = [
    "allow_once",
    "allow_session_exact",
    "allow_permanent_exact",
    "reject_once",
];

fn outside_options(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let outside = world.tmp().join("outside.txt");
    world.platform().replies([
        Reply::tool_call(
            "write_file",
            json!({"path": outside, "content": "out of tree"}),
        ),
        Reply::text("declined"),
    ]);
    let mut rpc = editor(&world, &[])?;
    answer_permissions(&mut rpc, Choice::Select("reject_once"));
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "write outside", "end_turn")?;
    let exchange = rpc
        .exchanges()
        .iter()
        .find(|e| e.method == "session/request_permission")
        .ok_or("no permission request was raised")?;
    let offered: Vec<&str> = exchange.params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| option["optionId"].as_str())
        .collect();
    ensure(offered == OUTSIDE_OPTIONS, || {
        format!("a write outside the work tree offered {offered:?}")
    })?;
    ensure(!outside.exists(), || {
        "the rejected write happened".to_owned()
    })
}

/// Waits until `path` exists.
fn await_file(path: &std::path::Path, deadline: Duration) -> Outcome {
    let until = Instant::now() + vibe_e2e::scaled(deadline);
    while !path.exists() {
        if Instant::now() >= until {
            return Err(format!("{} did not appear within {deadline:?}", path.display()).into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn cancel_running(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([Reply::tool_call(
        "bash",
        json!({"command": "sh -c 'touch started.txt; sleep 3; echo late > late.txt'"}),
    )]);
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    let id = rpc.send(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "wait"}]}),
    )?;
    // Answered by hand: nothing reads the connection while the file is
    // awaited.
    let request = rpc.await_request(|m| m["method"] == "session/request_permission", PROMPT)?;
    rpc.reply(&request, Choice::Select("allow_once").json())?;
    await_file(&world.work().join("started.txt"), PROMPT)?;
    rpc.notify("session/cancel", json!({"sessionId": session}))?;
    let response = rpc.response_within(id, PROMPT)?;
    ensure(response["result"]["stopReason"] == "cancelled", || {
        format!("the canceled prompt answered {response}")
    })?;
    // The command would write its file 3 s after it started.
    let late = world.work().join("late.txt");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        ensure(!late.exists(), || {
            "late.txt was written after the prompt was canceled".to_owned()
        })?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn cancel_pending(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([Reply::tool_call(
        "bash",
        json!({"command": "touch never.txt"}),
    )]);
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    let id = rpc.send(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "ask me"}]}),
    )?;
    let request = rpc.await_request(|m| m["method"] == "session/request_permission", PROMPT)?;
    // Zed's order: the cancel first, then the cancelled answer to every
    // pending request.
    rpc.notify("session/cancel", json!({"sessionId": session}))?;
    rpc.reply(&request, json!({"outcome": {"outcome": "cancelled"}}))?;
    let response = rpc.response_within(id, PROMPT)?;
    ensure(response["result"]["stopReason"] == "cancelled", || {
        format!("the canceled prompt answered {response}")
    })?;
    let call = request["params"]["toolCall"]["toolCallId"].clone();
    let last = rpc
        .notifications()
        .iter()
        .map(|m| &m["params"]["update"])
        .rfind(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == call)
        .cloned();
    ensure(
        last.as_ref().is_some_and(|u| u["status"] != "in_progress"),
        || format!("the tool call {call} was last updated as {last:?}"),
    )?;
    ensure(!world.work().join("never.txt").exists(), || {
        "the canceled command ran".to_owned()
    })
}

fn retried(ctx: &mut Ctx, status: u16) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::status(status), Reply::text("recovered")]);
    world.platform().exact();
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "retry me", "end_turn")?;
    let retrying: Vec<&Value> = rpc
        .notifications()
        .iter()
        .filter(|m| m["method"] == "_session/retrying")
        .collect();
    ensure(
        retrying.len() == 1 && retrying[0]["params"]["sessionId"] == session.as_str(),
        || format!("one retry sent the notifications {retrying:?}"),
    )?;
    ensure(turns(&world).len() == 2, || {
        format!("expected 2 turn requests, got {}", turns(&world).len())
    })
}

/// A stdio server `e2e` publishing `lookup`, which answers `forty-two`.
fn lookup_server() -> Mcp {
    Mcp::new()
        .tool(json!({
            "name": "lookup",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}},
                            "required": ["query"]},
        }))
        .react("lookup", json!({"result": text_result("forty-two")}))
}

/// The content of the tool message of the last turn request.
fn last_tool_message(world: &World) -> Outcome<String> {
    let last = turns(world).pop().ok_or("no turn request")?;
    last.messages()
        .iter()
        .rfind(|m| m["role"] == "tool")
        .map(|m| match &m["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .ok_or_else(|| "the last turn request carries no tool message".into())
}

fn legacy_mcp(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let server = world.mcp_stdio("e2e", &lookup_server())?;
    let path = world.vibe_home().join("config.toml");
    let config = std::fs::read_to_string(&path)?;
    std::fs::write(&path, format!("{config}\n{}", server.toml("")))?;
    world.platform().replies([
        Reply::tool_call("e2e_lookup", json!({"query": "x"})),
        Reply::text("looked up"),
    ]);
    world.platform().exact();
    let mut rpc = editor(&world, &["--legacy-harness"])?;
    answer_permissions(&mut rpc, Choice::Select("allow_once"));
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "look it up", "end_turn")?;
    ensure(asked(&rpc) == 1, || {
        format!("the MCP call raised {} permission requests", asked(&rpc))
    })?;
    let message = last_tool_message(&world)?;
    ensure(message.contains("forty-two"), || {
        format!("the MCP result did not reach the model: {message}")
    })
}

fn editor_mcp(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let server = world.mcp_stdio("e2e", &lookup_server())?;
    world.platform().replies([
        Reply::tool_call(
            "run_typescript",
            json!({"code": "return await tools.mcp_e2e.lookup({query: 'x'});"}),
        ),
        Reply::text("looked up"),
    ]);
    world.platform().exact();
    let mut rpc = editor(&world, &[])?;
    answer_permissions(&mut rpc, Choice::Select("allow_once"));
    let created = rpc.call(
        "session/new",
        json!({"cwd": world.work(), "mcpServers": [{
            "name": "e2e", "command": server.command(), "args": server.args(), "env": [],
        }]}),
    )?;
    let session = created["sessionId"]
        .as_str()
        .ok_or_else(|| format!("session/new answered {created}"))?
        .to_owned();
    prompt_ends(&mut rpc, &session, "look it up", "end_turn")?;
    // Measured on 2026-10-29: the reference starts the server again for the
    // call, which reaches it, and reports the call as a transport failure.
    let message = last_tool_message(&world)?;
    ensure(message.contains("mcp_transport_failed"), || {
        format!("the call through the editor's MCP server returned {message}")
    })?;
    let calls = server
        .messages()
        .into_iter()
        .filter(|m| m["method"] == "tools/call")
        .count();
    ensure(calls == 1, || {
        format!("the server received {calls} tools/call")
    })
}

/// The client requests a legacy-harness read, write, and command raise, in
/// the reference's order, measured on 2026-10-29.
const DELEGATED: &[&str] = &[
    "fs/read_text_file",
    "fs/write_text_file",
    "terminal/create",
    "terminal/wait_for_exit",
    "terminal/output",
    "terminal/release",
];

fn client_delegation(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let work = world.work();
    let file = |name: &str| work.join(name).to_string_lossy().into_owned();
    world.platform().replies([
        Reply::tool_call("read_file", json!({"file_path": file("seed.txt")})),
        Reply::tool_call(
            "write_file",
            json!({"file_path": file("new.txt"), "content": "from the model"}),
        ),
        Reply::tool_call("bash", json!({"command": "echo through the editor"})),
        Reply::text("delegated"),
    ]);
    world.platform().exact();
    world.write("work/seed.txt", "seed on disk\n")?;
    let mut rpc = spawn(&world, &["--legacy-harness"])?;
    initialize(
        &mut rpc,
        json!({"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}),
    )?;
    answer_permissions(&mut rpc, Choice::Select("allow_once"));
    rpc.respond("fs/read_text_file", |_| {
        Answer::result(json!({"content": "editor buffer\n"}))
    });
    rpc.respond("fs/write_text_file", |_| Answer::result(Value::Null));
    rpc.respond("terminal/create", |_| {
        Answer::result(json!({"terminalId": "term-1"}))
    });
    rpc.respond("terminal/output", |_| {
        Answer::result(json!({"output": "editor output\n", "truncated": false,
                              "exitStatus": {"exitCode": 0, "signal": null}}))
    });
    rpc.respond("terminal/wait_for_exit", |_| {
        Answer::result(json!({"exitCode": 0, "signal": null}))
    });
    rpc.respond("terminal/*", |_| Answer::result(json!({})));
    let session = new_session(&mut rpc, &world)?;
    let response = prompt(&mut rpc, &session, "delegate")?;
    ensure(response["result"]["stopReason"] == "end_turn", || {
        format!("the delegating prompt answered {response}")
    })?;
    let order: Vec<&str> = rpc
        .exchanges()
        .iter()
        .map(|e| e.method.as_str())
        .filter(|m| m.starts_with("fs/") || m.starts_with("terminal/"))
        .collect();
    ensure(order == DELEGATED, || {
        format!("the client requests came in the order {order:?}")
    })?;
    ensure(!world.work().join("new.txt").exists(), || {
        "the write reached the disk instead of the editor".to_owned()
    })
}
