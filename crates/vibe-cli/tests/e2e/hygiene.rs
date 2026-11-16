//! US-016: code-mode deadlines, the process API, large tool outputs, the
//! processes a turn leaves behind, and text that is not valid UTF-8, on the
//! default harness.

use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, Scenario, World, ensure};

use crate::cli::{exits, vibe};
use crate::mcp::outcome;
use crate::tools::{fingerprint, normalize, tool_message, turn_requests};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        Scenario::new(
            "hygiene::code_mode::an_endless_loop_returns_its_deadline_to_the_model",
            |ctx| runaway(ctx, "while (true) {}", "InvalidToolCall"),
        )
        .known_defect(
            "E2E-D19",
            "a code-mode deadline fails the whole turn instead of returning to the model",
        ),
        Scenario::new(
            "hygiene::code_mode::a_promise_that_never_settles_returns_an_error",
            |ctx| runaway(ctx, "await new Promise(() => {});", "ExecutionError"),
        ),
        Scenario::new(
            "hygiene::code_mode::the_process_api_starts_reads_and_waits",
            process_api,
        ),
    ];
    for tool in ["read_file", "bash", "run_typescript"] {
        scenarios.push(
            Scenario::new(
                format!("hygiene::large_output::{tool}_is_saved_under_tool_results"),
                move |ctx| large_output(ctx, tool),
            )
            .known_defect(
                "E2E-D20",
                "tool output above 40,000 characters is sent whole instead of being saved",
            ),
        );
    }
    scenarios.extend([
        // The reference leaves a detached job running once `vibe -p` exits,
        // which contradicts this story's contract; the trial keeps the
        // contract for the port.
        Scenario::new(
            "hygiene::processes::a_background_command_leaves_no_process",
            |ctx| leftovers(ctx, &json!({"command": "sleep 300 > /dev/null 2>&1 &"})),
        )
        .windows_ignore(vibe_e2e::NO_SURVIVOR_SCAN)
        .port_only(
            "the reference leaves a background `bash` job alive after `vibe -p` exits \
             (measured 2026-10-28)",
        ),
        Scenario::new(
            "hygiene::processes::a_command_past_its_timeout_leaves_no_process",
            |ctx| {
                leftovers(
                    ctx,
                    &json!({"command": "sleep 300; echo after", "timeout_seconds": 1}),
                )
            },
        )
        .windows_ignore(vibe_e2e::NO_SURVIVOR_SCAN),
    ]);
    scenarios.push(
        Scenario::new(
            "hygiene::processes::an_interrupted_turn_leaves_no_process",
            interrupted,
        )
        .windows_ignore(vibe_e2e::NO_SURVIVOR_SCAN)
        .known_defect(
            "E2E-D21",
            "a `bash` command interrupted with its turn leaves its process alive",
        ),
    );
    for tool in ["read_file", "bash"] {
        scenarios.push(Scenario::new(
            format!("hygiene::encoding::{tool}_of_mixed_valid_and_invalid_utf8"),
            move |ctx| encoding(ctx, tool),
        ));
    }
    scenarios
}

/// Runs `calls` then a final answer under `vibe -p --auto-approve`, and
/// returns the world's last turn request's tool messages in call order.
fn run(world: &World, calls: Vec<Reply>) -> Outcome<Vec<String>> {
    let count = calls.len();
    world.platform().replies(calls);
    world.platform().reply(Reply::text("done"));
    let output = vibe(world, &["-p", "go", "--output", "text", "--auto-approve"])?;
    exits(&output, 0)?;
    ensure(output.stdout == "done\n", || {
        format!("the turn reached no final answer: {:?}", output.stdout)
    })?;
    let last = turn_requests(world)?.pop().ok_or("no turn request")?;
    (1..=count)
        .map(|n| tool_message(&last, &format!("e2e{n:06}")))
        .collect()
}

fn typescript(code: &str) -> Reply {
    Reply::tool_call("run_typescript", json!({ "code": code }))
}

/// The error a tool message reports: a `run_typescript` failure's name, or
/// the name of a structured tool error.
fn error_name(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|parsed| {
            parsed
                .pointer("/result/error/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| outcome(message))
}

fn runaway(ctx: &mut Ctx, code: &str, expected: &str) -> Outcome {
    let world = ctx.world()?;
    let messages = run(&world, vec![typescript(code)])?;
    let found = error_name(&messages[0]);
    ensure(found == expected, || {
        format!(
            "the program ended {found:?}, expected {expected:?}: {}",
            messages[0]
        )
    })
}

/// Starts a command that prints twice, reads its output until it exits, and
/// returns what the model would read.
const PROCESS_PROGRAM: &str = "\
const started = await tools.process.start({command: \"printf 'one\\\\n'; sleep 0.3; printf 'two\\\\n'\"});
let read = await tools.process.output({processId: started.processId, from: 'start', waitMs: 5000});
let output = read.output;
while (read.status === 'running') {
  read = await tools.process.output({processId: started.processId, from: 'start', cursor: read.nextCursor, waitMs: 5000});
  output += read.output;
}
return {started: started.status, status: read.status, exitCode: read.exitCode, output};";

fn process_api(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let messages = run(&world, vec![typescript(PROCESS_PROGRAM)])?;
    let expected = PROCESS_RESULT;
    ensure(messages[0] == expected, || {
        format!("the program returned {}, expected {expected}", messages[0])
    })
}

/// What the reference returns for [`PROCESS_PROGRAM`].
const PROCESS_RESULT: &str =
    r#"{"started":"running","status":"completed","exitCode":0,"output":"one\r\ntwo\r\n"}"#;

/// A call of `tool` producing more than 40,000 characters: `bash` keeps a
/// bounded stdout, so its case prints bytes JSON escapes six times over.
fn large_call(tool: &str) -> Reply {
    match tool {
        "read_file" => Reply::tool_call("read_file", json!({"path": "wide.txt"})),
        "bash" => Reply::tool_call("bash", json!({"command": "head -c 15000 /dev/zero"})),
        _ => typescript("return 'b'.repeat(50000);"),
    }
}

fn large_output(ctx: &mut Ctx, tool: &str) -> Outcome {
    let world = ctx.world()?;
    // 1,000 lines of 60 characters stay within `read_file`'s line limit.
    world.write(
        "work/wide.txt",
        format!("{}\n", "w".repeat(59)).repeat(1000),
    )?;
    let messages = run(&world, vec![large_call(tool)])?;
    let message = &messages[0];
    let sessions = world.vibe_home().join("logs/session/unified");
    let saved: Vec<std::path::PathBuf> = std::fs::read_dir(&sessions)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|session| {
            std::fs::read_dir(session.path().join("tool-results"))
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|entry| entry.path())
        .collect();
    let [path] = saved.as_slice() else {
        return Err(format!(
            "expected one saved output under tool-results/, found {saved:?}; the tool message \
             holds {} characters",
            message.chars().count()
        )
        .into());
    };
    let whole = std::fs::read_to_string(path)?;
    ensure(
        message.contains(&path.to_string_lossy().into_owned()),
        || format!("the tool message does not name {}", path.display()),
    )?;
    ensure(
        whole.chars().count() > 40_000 && message.chars().count() < whole.chars().count(),
        || {
            format!(
                "the saved output holds {} characters and the message {}",
                whole.chars().count(),
                message.chars().count()
            )
        },
    )
}

fn leftovers(ctx: &mut Ctx, arguments: &Value) -> Outcome {
    let world = ctx.world()?;
    run(&world, vec![Reply::tool_call("bash", arguments.clone())])?;
    let survivors = ctx.close(world);
    ensure(survivors.is_empty(), || survivors.join("\n"))
}

fn interrupted(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call(
            "bash",
            json!({"command": "touch started; sleep 300; echo after"}),
        ),
        Reply::text("done"),
    ]);
    let child = world
        .command("vibe")?
        .args(["-p", "go", "--output", "text", "--auto-approve"])
        .spawn()?;
    let started = world.work().join("started");
    let until = std::time::Instant::now() + vibe_e2e::scaled(Duration::from_secs(20));
    while !started.exists() {
        ensure(std::time::Instant::now() < until, || {
            "the command never started".to_owned()
        })?;
        std::thread::sleep(Duration::from_millis(50));
    }
    child.interrupt()?;
    child.wait_within(Duration::from_secs(10))?;
    let survivors = ctx.close(world);
    ensure(survivors.is_empty(), || survivors.join("\n"))
}

/// Valid UTF-8, then two bytes that are not, then valid UTF-8 again.
/// shared_with_reference: `read_file` decodes such a file as Latin-1 in both
/// implementations, valid sequences included, while `bash` replaces only the
/// invalid bytes.
const MIXED: &[u8] = b"caf\xc3\xa9 ok \xff\xfe end \xe2\x82\xac\n";

fn encoding(ctx: &mut Ctx, tool: &str) -> Outcome {
    let world = ctx.world()?;
    world.write("work/mixed.txt", MIXED)?;
    let call = if tool == "bash" {
        Reply::tool_call("bash", json!({"command": "cat mixed.txt"}))
    } else {
        Reply::tool_call("read_file", json!({"path": "mixed.txt"}))
    };
    let messages = run(&world, vec![call])?;
    let normalized = normalize(&world, tool, false, &messages[0]);
    let found = fingerprint(&normalized);
    let expected = ENCODINGS
        .iter()
        .find(|(name, _)| *name == tool)
        .map_or("", |(_, hash)| *hash);
    ensure(found == expected, || {
        format!("fingerprint {found}, expected {expected:?}\ntool message:\n{normalized}")
    })
}

/// `(tool, fingerprint of the normalized tool message)`.
const ENCODINGS: &[(&str, &str)] = &[
    ("read_file", "a9e4eec3ba3abffe"),
    ("bash", "10f747b84065197b"),
];
