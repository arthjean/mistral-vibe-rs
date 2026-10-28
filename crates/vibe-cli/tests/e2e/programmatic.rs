//! US-009: `vibe -p` answers through the provider `config.toml` declares, in
//! every output format, from every prompt source, and through a tool loop on
//! both harnesses.

use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, Scenario, Setup, World, ensure};

use crate::cli::{chat_count, content_text, exits, last_user, stream_entries, vibe, with_role};

const ANSWER: &str = "golden answer";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for format in ["text", "json", "streaming"] {
        scenarios.push(Scenario::new(
            format!("programmatic::golden::{format}_output_through_a_generic_provider"),
            move |ctx| golden(ctx, format),
        ));
    }
    scenarios.extend([
        Scenario::new(
            "programmatic::golden::a_mistral_provider_sends_its_configured_key",
            mistral_key,
        ),
        Scenario::new("programmatic::prompt::read_from_stdin", from_stdin),
        Scenario::new(
            "programmatic::prompt::an_empty_stdin_is_refused",
            empty_stdin,
        ),
        Scenario::new(
            "programmatic::prompt::a_positional_after_the_prompt_flag_is_ignored",
            positional_after_prompt,
        ),
        Scenario::new(
            "programmatic::prompt::a_bare_prompt_flag_does_not_take_the_positional",
            positional_with_bare_flag,
        ),
        Scenario::new(
            "programmatic::prompt::a_300_kb_prompt_reaches_the_platform_whole",
            large_prompt,
        ),
        Scenario::new("programmatic::tool_loop::unified_harness", |ctx| {
            tool_loop(ctx, false)
        }),
        Scenario::new("programmatic::tool_loop::legacy_harness", |ctx| {
            tool_loop(ctx, true)
        }),
        Scenario::new(
            "programmatic::errors::an_unauthorized_provider_ends_the_run",
            unauthorized,
        ),
    ]);
    scenarios
}

/// The assistant text of a `--output json` history or a streaming entry.
fn entry_text(entry: &Value) -> String {
    entry
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect()
}

fn last_assistant(entries: &[Value]) -> Option<&Value> {
    entries
        .iter()
        .rev()
        .find(|e| e.get("role").and_then(Value::as_str) == Some("assistant"))
}

fn golden(ctx: &mut Ctx, format: &str) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text(ANSWER));
    world.platform().exact();
    let output = world
        .command("vibe")?
        .args(["-p", "hello", "--output", format])
        .env_remove("MISTRAL_API_KEY")
        .run()?;
    exits(&output, 0)?;
    let entries = match format {
        "text" => {
            return check_single_turn(&world).and_then(|()| {
                ensure(output.stdout == format!("{ANSWER}\n"), || {
                    format!("the text output is {:?}", output.stdout)
                })
            });
        }
        "json" => match serde_json::from_str::<Value>(&output.stdout)? {
            Value::Array(entries) => entries,
            other => return Err(format!("the json output is not a history: {other}").into()),
        },
        _ => stream_entries(&output.stdout)?,
    };
    let answer = last_assistant(&entries).map(entry_text);
    ensure(answer.as_deref() == Some(ANSWER), || {
        format!("the {format} output carries no assistant answer {ANSWER:?}: {entries:?}")
    })?;
    check_single_turn(&world)
}

/// Exactly one turn request, naming the configured model.
fn check_single_turn(world: &World) -> Outcome {
    let requests = chat_count(world, 1)?;
    let model = requests[0].body.get("model").and_then(Value::as_str);
    ensure(model == Some(world.provider().model_name()), || {
        format!(
            "the turn named {model:?}, not the configured {}",
            world.provider().model_name()
        )
    })
}

fn mistral_key(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::mistral())?;
    world.platform().reply(Reply::text(ANSWER));
    world.platform().exact();
    let output = vibe(&world, &["-p", "hello", "--output", "text"])?;
    exits(&output, 0)?;
    ensure(output.stdout == format!("{ANSWER}\n"), || {
        output.stdout.clone()
    })?;
    let requests = chat_count(&world, 1)?;
    let authorization = requests[0].header("authorization");
    ensure(authorization == Some("Bearer e2e-key"), || {
        format!("the turn carried the authorization {authorization:?}")
    })
}

/// Runs `vibe` with `arguments`, writes `stdin`, and closes it.
fn with_stdin(world: &World, arguments: &[&str], stdin: &str) -> Outcome<vibe_e2e::Output> {
    let mut child = world
        .command("vibe")?
        .args(arguments.iter().copied())
        .stdin_piped()
        .spawn()?;
    child.write(stdin)?;
    child.close_stdin();
    child.wait()
}

fn from_stdin(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text(ANSWER));
    let output = with_stdin(&world, &["-p", "--output", "text"], "from stdin")?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    ensure(last_user(&requests[0]) == "from stdin", || {
        format!(
            "the prompt reached the platform as {:?}",
            last_user(&requests[0])
        )
    })
}

fn empty_stdin(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let output = with_stdin(&world, &["-p", "--output", "text"], "")?;
    exits(&output, 1)?;
    chat_count(&world, 0).map(drop)
}

fn positional_after_prompt(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text(ANSWER));
    let output = vibe(
        &world,
        &["-p", "hello", "positional words", "--output", "text"],
    )?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    ensure(last_user(&requests[0]) == "hello", || {
        format!(
            "the prompt reached the platform as {:?}",
            last_user(&requests[0])
        )
    })
}

/// A bare `-p` reads its prompt from stdin, here the null device, and leaves
/// the positional unused.
fn positional_with_bare_flag(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text(ANSWER));
    let output = vibe(
        &world,
        &["--output", "text", "-p", "--", "positional words"],
    )?;
    exits(&output, 1)?;
    chat_count(&world, 0).map(drop)
}

fn large_prompt(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text(ANSWER));
    let prompt: String = "abcdefghij".repeat(30_000);
    let output = with_stdin(&world, &["-p", "--output", "text"], &prompt)?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    let sent = last_user(&requests[0]);
    ensure(sent == prompt, || {
        format!(
            "the {} byte prompt reached the platform as {} bytes",
            prompt.len(),
            sent.len()
        )
    })
}

fn tool_loop(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    // The legacy file tools take an absolute `file_path`, the Unified Harness
    // ones a `path` relative to the work tree.
    let file = world.work().join("loop.txt");
    let (write, read) = if legacy {
        let path = file.to_string_lossy().into_owned();
        (
            json!({"file_path": path, "content": "one"}),
            json!({"file_path": path}),
        )
    } else {
        (
            json!({"path": "loop.txt", "content": "one"}),
            json!({"path": "loop.txt"}),
        )
    };
    world.platform().replies([
        Reply::tool_call("write_file", write),
        Reply::tool_call("read_file", read),
        Reply::text("loop done"),
    ]);
    world.platform().exact();
    let mut arguments = vec!["-p", "go", "--output", "text", "--auto-approve"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, 0)?;
    ensure(output.stdout == "loop done\n", || output.stdout.clone())?;
    ensure(
        std::fs::read_to_string(&file).ok().as_deref() == Some("one"),
        || format!("{} does not hold the written content", file.display()),
    )?;
    let requests = chat_count(&world, 3)?;
    for (position, request) in requests.iter().enumerate() {
        let calls: Vec<&str> = with_role(request, "assistant")
            .into_iter()
            .flat_map(|m| {
                m.get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .collect();
        let answered: Vec<&str> = with_role(request, "tool")
            .into_iter()
            .filter_map(|m| m.get("tool_call_id").and_then(Value::as_str))
            .collect();
        let expected: Vec<String> = (1..=position).map(|n| format!("e2e{n:06}")).collect();
        ensure(calls == expected && answered == expected, || {
            format!(
                "turn request #{} carries the tool calls {calls:?} and the results {answered:?}, \
                 not {expected:?}",
                position + 1
            )
        })?;
    }
    let read_result = with_role(&requests[2], "tool")
        .last()
        .map(|m| content_text(m))
        .unwrap_or_default();
    ensure(read_result.contains("one"), || {
        format!("the read_file result does not carry the file content: {read_result}")
    })
}

fn unauthorized(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .reply(Reply::status(401).body(r#"{"message":"Unauthorized"}"#));
    let output = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"])
        .deadline(Duration::from_secs(10))
        .run()?;
    ensure(!output.success(), || "the run succeeded".to_owned())?;
    chat_count(&world, 1)?;
    ensure(
        output.stderr.contains("API key") && output.stderr.contains("E2E_PROVIDER_KEY"),
        || {
            format!(
                "the error does not name the key and its variable: {}",
                output.stderr
            )
        },
    )
}
