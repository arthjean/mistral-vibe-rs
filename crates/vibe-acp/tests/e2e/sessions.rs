//! US-023: sessions across restarts and clients, the extensions an editor
//! calls, slash commands, and protocol robustness.

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, Rpc, Scenario, Subject, World, ensure};

use crate::editor::{PROMPT, editor, new_session, prompt_ends};
use crate::routing::{prompting, turns};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        prompting(Scenario::new(
            "acp::sessions::list_load_fork_close_and_resume_across_a_restart",
            across_restart,
        )),
        Scenario::new(
            "acp::sessions::a_vibe_p_session_is_listed_and_loaded",
            cli_session,
        ),
        Scenario::new("acp::extensions::set_title_renames_the_session", set_title),
        Scenario::new(
            "acp::extensions::delete_removes_a_saved_session",
            delete_saved,
        ),
        Scenario::new(
            "acp::extensions::delete_of_a_live_session_without_a_turn",
            delete_live,
        )
        .known_defect(
            "E2E-D44",
            "`_session/delete` of a live session with no turn yet fails with -32603",
        ),
        Scenario::new("acp::extensions::trust_status_and_decision", trust),
        prompting(Scenario::new(
            "acp::extensions::rewind_restores_a_written_file",
            rewind,
        )),
        Scenario::new(
            "acp::commands::the_advertised_slash_commands",
            advertised_commands,
        ),
        Scenario::new(
            "acp::protocol::initialize_reports_the_package_version",
            agent_version,
        ),
    ];
    for &(name, line, code) in WIRE_CASES {
        scenarios.push(Scenario::new(
            format!("acp::protocol::{name}"),
            move |ctx| wire_case(ctx, (line, code)),
        ));
    }
    scenarios.push(
        Scenario::new(
            "acp::protocol::a_relative_cwd_opens_a_session",
            relative_cwd,
        )
        .known_defect(
            "E2E-D45",
            "`session/new` with a relative `cwd` fails with -31002",
        ),
    );
    for &command in LOCAL_COMMANDS {
        scenarios.push(Scenario::new(
            format!(
                "acp::commands::{}_answers_without_the_model",
                command.replace('-', "_")
            ),
            move |ctx| local_command(ctx, command),
        ));
    }
    scenarios
}

/// The `session/new`, `session/load`, `session/fork`, and `session/resume`
/// parameters for the world's work tree.
fn opening(world: &World, session: Option<&str>) -> Value {
    let mut params = json!({"cwd": world.work(), "mcpServers": []});
    if let Some(session) = session {
        params["sessionId"] = json!(session);
    }
    params
}

/// The ids `session/list` returns for the world's work tree.
fn listed(rpc: &mut Rpc, world: &World) -> Outcome<Vec<String>> {
    let result = rpc.call("session/list", json!({"cwd": world.work()}))?;
    Ok(result["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["sessionId"].as_str().map(str::to_owned))
        .collect())
}

/// `user:text` and `agent:text` for each message chunk received since
/// notification `from`, with consecutive chunks of one side joined.
fn replayed(rpc: &Rpc, from: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for message in &rpc.notifications()[from..] {
        let update = &message["params"]["update"];
        let side = match update["sessionUpdate"].as_str() {
            Some("user_message_chunk") => "user",
            Some("agent_message_chunk") => "agent",
            _ => continue,
        };
        let text = update["content"]["text"].as_str().unwrap_or_default();
        match lines.last_mut() {
            Some(last) if last.starts_with(&format!("{side}:")) => last.push_str(text),
            _ => lines.push(format!("{side}:{text}")),
        }
    }
    lines
}

fn across_restart(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("answer one"), Reply::text("answer two")]);
    world.platform().exact();
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "prompt one", "end_turn")?;
    rpc.hang_up().map(drop)?;

    let mut rpc = editor(&world, &[])?;
    let ids = listed(&mut rpc, &world)?;
    ensure(ids == [session.clone()], || {
        format!("after a restart session/list returned {ids:?}, expected {session}")
    })?;
    let from = rpc.notifications().len();
    rpc.call("session/load", opening(&world, Some(&session)))?;
    let history = replayed(&rpc, from);
    ensure(history == ["user:prompt one", "agent:answer one"], || {
        format!("session/load replayed {history:?}")
    })?;
    let forked = rpc.call("session/fork", opening(&world, Some(&session)))?;
    let fork = forked["sessionId"].as_str().unwrap_or_default().to_owned();
    ensure(!fork.is_empty() && fork != session, || {
        format!("session/fork answered {forked}")
    })?;
    rpc.call("session/close", json!({"sessionId": fork}))?;
    rpc.call("session/resume", opening(&world, Some(&session)))?;
    prompt_ends(&mut rpc, &session, "prompt two", "end_turn")?;
    let last = turns(&world).pop().ok_or("no turn request")?;
    let users: Vec<String> = last
        .messages()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect();
    ensure(
        users.iter().any(|u| u.contains("prompt one"))
            && users.iter().any(|u| u.contains("prompt two")),
        || format!("the resumed turn carried the user messages {users:?}"),
    )?;
    rpc.hang_up().map(drop)
}

fn cli_session(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let session = cli_turn(&world, "cli answer")?;
    let mut rpc = editor(&world, &[])?;
    let ids = listed(&mut rpc, &world)?;
    ensure(ids.contains(&session), || {
        format!("session/list returned {ids:?}, missing the vibe -p session {session}")
    })?;
    let from = rpc.notifications().len();
    rpc.call("session/load", opening(&world, Some(&session)))?;
    let history = replayed(&rpc, from);
    ensure(history == ["user:from the cli", "agent:cli answer"], || {
        format!("session/load replayed {history:?}")
    })?;
    rpc.hang_up().map(drop)
}

fn set_title(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    let result = rpc.call(
        "_session/set_title",
        json!({"sessionId": session, "title": "Renamed by the editor"}),
    )?;
    ensure(result == json!({}), || {
        format!("_session/set_title answered {result}")
    })?;
    let update = rpc.await_notification(
        |m| {
            m["params"]["update"]["sessionUpdate"] == "session_info_update"
                && m["params"]["update"]["title"] == "Renamed by the editor"
        },
        PROMPT,
    )?;
    ensure(update["params"]["sessionId"] == session.as_str(), || {
        format!("the title update named {update}")
    })?;
    rpc.hang_up().map(drop)
}

/// Runs `vibe -p` once and returns the session it saved.
fn cli_turn(world: &World, answer: &str) -> Outcome<String> {
    world.platform().reply(Reply::text(answer));
    let output = world
        .command("vibe")?
        .args(["-p", "from the cli", "--output", "json"])
        .run()?;
    ensure(output.success(), || {
        format!("vibe -p failed: {}{}", output.stdout, output.stderr)
    })?;
    let printed: Value = serde_json::from_str(&output.stdout)?;
    printed
        .pointer("/0/sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("vibe -p printed no session id: {}", output.stdout).into())
}

fn delete_saved(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let session = cli_turn(&world, "saved")?;
    let mut rpc = editor(&world, &[])?;
    let before = listed(&mut rpc, &world)?;
    ensure(before == [session.clone()], || {
        format!("session/list returned {before:?}, expected {session}")
    })?;
    let result = rpc.call("_session/delete", json!({"sessionId": session}))?;
    ensure(result == json!({}), || {
        format!("_session/delete answered {result}")
    })?;
    let after = listed(&mut rpc, &world)?;
    ensure(after.is_empty(), || {
        format!("after _session/delete session/list returned {after:?}")
    })?;
    rpc.hang_up().map(drop)
}

fn delete_live(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    let response = rpc.request("_session/delete", json!({"sessionId": session}))?;
    ensure(response.get("result") == Some(&json!({})), || {
        format!("_session/delete of a live session with no turn answered {response}")
    })?;
    rpc.hang_up().map(drop)
}

fn trust(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    // A project configuration is what a trust decision is about.
    world.write("work/.vibe/config.toml", "active_model = \"e2e-model\"\n")?;
    let mut rpc = editor(&world, &[])?;
    let status = |rpc: &mut Rpc| -> Outcome<Value> {
        Ok(rpc.call("_trust/status", json!({"cwd": world.work()}))?["trust_status"].clone())
    };
    let before = status(&mut rpc)?;
    ensure(before == "untrusted", || {
        format!("_trust/status reported {before}")
    })?;
    // A decision is anchored to a session Vibe created.
    let refused = rpc.request(
        "_trust/decision",
        json!({"cwd": world.work(), "decision": "trust_cwd"}),
    )?;
    ensure(refused["error"]["code"] == -32602, || {
        format!("a session-less trust decision answered {refused}")
    })?;
    let session = new_session(&mut rpc, &world)?;
    let invalid = rpc.request(
        "_trust/decision",
        json!({"sessionId": session, "cwd": world.work(), "decision": "always"}),
    )?;
    ensure(invalid["error"]["code"] == -32602, || {
        format!("an unknown trust decision answered {invalid}")
    })?;
    let decided = rpc.call(
        "_trust/decision",
        json!({"sessionId": session, "cwd": world.work(), "decision": "trust_cwd"}),
    )?;
    let after = status(&mut rpc)?;
    ensure(
        decided["trust_status"] == "trusted" && after == "trusted",
        || format!("trusting the folder answered {decided}, then the status is {after}"),
    )?;
    let file = std::fs::read_to_string(world.vibe_home().join("trusted_folders.toml"))?;
    let work = world.work().to_string_lossy().into_owned();
    ensure(file.contains(&work), || {
        format!("trusted_folders.toml does not name the work tree: {file}")
    })?;
    rpc.hang_up().map(drop)
}

fn rewind(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call(
            "write_file",
            json!({"path": "notes.txt", "content": "written\n"}),
        ),
        Reply::text("written"),
    ]);
    let mut rpc = editor(&world, &[])?;
    rpc.respond("session/request_permission", |_| {
        vibe_e2e::Answer::result(
            json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
        )
    });
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "write the notes", "end_turn")?;
    let notes = world.work().join("notes.txt");
    ensure(notes.exists(), || {
        "the model's write did not happen".to_owned()
    })?;
    let user = rpc
        .notifications()
        .iter()
        .map(|m| &m["params"]["update"])
        .find(|u| u["sessionUpdate"] == "user_message_chunk")
        .and_then(|u| u["messageId"].as_str())
        .ok_or("the prompt echoed no user message id")?
        .to_owned();
    let written = json!([notes]);
    let preview = rpc.call(
        "_rewind/preview",
        json!({"sessionId": session, "messageId": user}),
    )?;
    ensure(preview["paths"] == written, || {
        format!("_rewind/preview answered {preview}")
    })?;
    let rewound = rpc.call(
        "_rewind/to",
        json!({"sessionId": session, "messageId": user}),
    )?;
    ensure(
        rewound["messageContent"] == "write the notes"
            && rewound["restoredPaths"] == written
            && rewound["restoreErrors"] == json!([]),
        || format!("_rewind/to answered {rewound}"),
    )?;
    ensure(!notes.exists(), || {
        "the rewind left the written file".to_owned()
    })?;
    rpc.hang_up().map(drop)
}

/// The slash commands both implementations advertise to an editor,
/// measured on 2026-10-29.
const ADVERTISED: [&str; 14] = [
    "compact",
    "data-retention",
    "help",
    "leanstall",
    "log",
    "mcp",
    "proxy-setup",
    "reload",
    "retry",
    "teleport",
    "unleanstall",
    "vibe:create-plugin",
    "vibe:skill-creator",
    "vibe:worktree",
];

fn advertised_commands(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = editor(&world, &[])?;
    new_session(&mut rpc, &world)?;
    let update = rpc.await_notification(
        |m| m["params"]["update"]["sessionUpdate"] == "available_commands_update",
        PROMPT,
    )?;
    let names: Vec<&str> = update["params"]["update"]["availableCommands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["name"].as_str())
        .collect();
    ensure(names == ADVERTISED, || {
        format!("the editor was offered the commands {names:?}")
    })?;
    rpc.hang_up().map(drop)
}

/// Slash commands an editor runs without a model request.
const LOCAL_COMMANDS: &[&str] = &["help", "log", "reload", "data-retention"];

fn local_command(ctx: &mut Ctx, command: &str) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    let from = rpc.notifications().len();
    prompt_ends(&mut rpc, &session, &format!("/{command}"), "end_turn")?;
    let lines = replayed(&rpc, from);
    ensure(
        lines.len() == 2
            && lines[0] == format!("user:/{command}")
            && lines[1].len() > "agent:".len(),
        || format!("/{command} streamed {lines:?}"),
    )?;
    ensure(world.platform().chat_requests().is_empty(), || {
        format!("/{command} sent a chat request")
    })
}

fn agent_version(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = crate::editor::spawn(&world, &[])?;
    let result = crate::editor::initialize(&mut rpc, json!({}))?;
    let expected = match ctx.subject() {
        Subject::Port => env!("CARGO_PKG_VERSION"),
        Subject::Reference => vibe_core::parity::REFERENCE_VERSION,
    };
    ensure(result["agentInfo"]["version"] == expected, || {
        format!(
            "initialize reported the agent {}, expected version {expected}",
            result["agentInfo"]
        )
    })
}

/// `(case, line written, error code of the answer to id 7 or null when the
/// line is answered by nothing)`, measured on 2026-10-29.
const WIRE_CASES: &[(&str, &str, Option<i64>)] = &[
    (
        "an_unknown_method",
        r#"{"jsonrpc":"2.0","id":7,"method":"vibe/no_such_method","params":{}}"#,
        Some(-32601),
    ),
    (
        "a_malformed_line",
        r#"{"jsonrpc":"2.0","id":7,"method":"#,
        None,
    ),
];

fn wire_case(ctx: &mut Ctx, (line, code): (&str, Option<i64>)) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = editor(&world, &[])?;
    rpc.write_raw(format!("{line}\n"))?;
    // A request sent after the line tells an unanswered line from a slow
    // answer, and shows the connection survived.
    let probe = rpc.send("session/list", json!({"cwd": world.work()}))?;
    let mut answers = Vec::new();
    loop {
        let message: Value = serde_json::from_str(&rpc.raw_line(PROMPT)?)?;
        if message.get("method").is_some() {
            continue;
        }
        if message["id"] == probe {
            break;
        }
        answers.push(message);
    }
    let found: Vec<Option<i64>> = answers
        .iter()
        .map(|answer| answer["error"]["code"].as_i64())
        .collect();
    let expected: Vec<Option<i64>> = code.into_iter().map(Some).collect();
    ensure(found == expected, || {
        format!("the line was answered by {answers:?}, expected error codes {expected:?}")
    })
}

fn relative_cwd(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = editor(&world, &[])?;
    let response = rpc.request("session/new", json!({"cwd": "work", "mcpServers": []}))?;
    ensure(response["result"]["sessionId"].is_string(), || {
        format!("session/new with a relative cwd answered {response}")
    })?;
    rpc.hang_up().map(drop)
}
