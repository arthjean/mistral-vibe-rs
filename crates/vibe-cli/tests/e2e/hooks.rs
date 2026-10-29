//! US-017: `pre_tool`, `post_tool`, and `post_agent` hooks under `vibe -p` on
//! both harnesses: their payloads, a denial, added context, how often
//! `post_agent` runs, and the failure paths.

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, Scenario, World, ensure};

use crate::cli::{content_text, exits, vibe, with_role};
use crate::tools::{tool_message, turn_requests};

const HARNESSES: [(&str, bool); 2] = [("unified", false), ("legacy", true)];

const NO_HOOK_RUNS: &str = "no hook runs on the default harness";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (harness, legacy) in HARNESSES {
        let tagged = |scenario: Scenario| {
            if legacy {
                scenario
            } else {
                scenario.known_defect("E2E-D12", NO_HOOK_RUNS)
            }
        };
        scenarios.extend([
            tagged(Scenario::new(
                format!("hooks::{harness}::each_hook_runs_with_the_reference_payload_keys"),
                move |ctx| recorders(ctx, legacy),
            )),
            tagged(Scenario::new(
                format!("hooks::{harness}::a_pre_tool_denial_stops_the_command"),
                move |ctx| denial(ctx, legacy),
            )),
            tagged(Scenario::new(
                format!("hooks::{harness}::post_tool_context_reaches_the_next_request"),
                move |ctx| context(ctx, legacy),
            )),
            tagged(Scenario::new(
                format!("hooks::{harness}::post_agent_runs_once_per_turn"),
                move |ctx| post_agent_once(ctx, legacy),
            )),
        ]);
        for failure in Failure::ALL {
            let name = format!("hooks::{harness}::failure::{}", failure.name());
            let expected = FAILURES
                .iter()
                .find(|(cell, _)| name.ends_with(cell))
                .map_or("", |(_, line)| *line);
            let scenario = Scenario::new(name, move |ctx| failing(ctx, legacy, failure, expected));
            // Only the trusted project file runs a hook the defect suppresses.
            scenarios.push(match failure {
                Failure::Untrusted => scenario,
                _ => tagged(scenario),
            });
        }
    }
    scenarios
}

/// A hook that appends its stdin, one JSON line per run, to `<name>.jsonl` at
/// the world root, then runs `then`.
fn recorder(world: &World, name: &str, kind: &str, then: &str) -> String {
    let log = world.root().join(format!("{name}.jsonl"));
    let log = log.to_string_lossy();
    format!(
        "[[hooks]]\nname = \"{name}\"\ntype = \"{kind}\"\n\
         command = '''cat >> {log}; echo >> {log}; {then}'''\n\n"
    )
}

/// The invocations a recorder hook received.
fn records(world: &World, name: &str) -> Vec<Value> {
    std::fs::read_to_string(world.root().join(format!("{name}.jsonl")))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn keys(record: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = record
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

/// Runs `vibe -p` with the tool calls scripted, and the final answer.
fn run(
    world: &World,
    legacy: bool,
    calls: Vec<Reply>,
    extra: &[&str],
) -> Outcome<vibe_e2e::Output> {
    world.platform().replies(calls);
    world.platform().reply(Reply::text("done"));
    let mut arguments = vec!["-p", "go", "--output", "text", "--auto-approve"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    arguments.extend(extra);
    vibe(world, &arguments)
}

fn touch(marker: &str) -> Reply {
    Reply::tool_call("bash", json!({"command": format!("touch {marker}")}))
}

const SESSION_KEYS: [&str; 5] = [
    "cwd",
    "hook_event_name",
    "parent_session_id",
    "session_id",
    "transcript_path",
];

fn recorders(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    let hooks = [
        recorder(&world, "pre", "pre_tool", "true"),
        recorder(&world, "post", "post_tool", "true"),
        recorder(&world, "agent", "post_agent", "true"),
    ];
    world.write("vibe-home/hooks.toml", hooks.concat())?;
    exits(&run(&world, legacy, vec![touch("marker-bash")], &[])?, 0)?;
    let tool = ["tool_call_id", "tool_input", "tool_name"];
    let post = [
        "duration_ms",
        "tool_error",
        "tool_output",
        "tool_output_text",
        "tool_status",
    ];
    for (name, extra) in [
        ("pre", tool.to_vec()),
        ("post", [tool.as_slice(), &post].concat()),
        ("agent", Vec::new()),
    ] {
        let mut expected: Vec<&str> = [SESSION_KEYS.as_slice(), &extra].concat();
        expected.sort_unstable();
        let found = records(&world, name);
        ensure(found.len() == 1, || {
            format!("the {name} hook ran {} time(s), not once", found.len())
        })?;
        ensure(keys(&found[0]) == expected, || {
            format!(
                "the {name} hook received the keys {:?}, not {expected:?}",
                keys(&found[0])
            )
        })?;
    }
    Ok(())
}

const REASON: &str = "E2E-DENY-REASON";

fn denial(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    let answer = format!(r#"echo '{{"decision": "deny", "reason": "{REASON}"}}'"#);
    world.write(
        "vibe-home/hooks.toml",
        recorder(&world, "deny", "pre_tool", &answer),
    )?;
    exits(&run(&world, legacy, vec![touch("marker-bash")], &[])?, 0)?;
    ensure(!world.work().join("marker-bash").exists(), || {
        "the denied command ran".to_owned()
    })?;
    let last = turn_requests(&world)?.pop().ok_or("no turn request")?;
    let message = tool_message(&last, "e2e000001")?;
    ensure(message.contains(REASON), || {
        format!("the tool message does not carry the denial reason: {message}")
    })
}

const CONTEXT: &str = "E2E-CONTEXT-MARK";

fn context(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    let answer =
        format!(r#"echo '{{"hook_specific_output": {{"additional_context": "{CONTEXT}"}}}}'"#);
    world.write(
        "vibe-home/hooks.toml",
        recorder(&world, "post", "post_tool", &answer),
    )?;
    exits(&run(&world, legacy, vec![touch("marker-bash")], &[])?, 0)?;
    let last = turn_requests(&world)?.pop().ok_or("no turn request")?;
    let carried = with_role(&last, "tool")
        .into_iter()
        .any(|m| content_text(m).contains(CONTEXT));
    ensure(carried, || {
        format!("the next turn request does not carry {CONTEXT}")
    })
}

/// A turn of two tool steps still ends once.
fn post_agent_once(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    world.write(
        "vibe-home/hooks.toml",
        recorder(&world, "agent", "post_agent", "true"),
    )?;
    let output = run(&world, legacy, vec![touch("one"), touch("two")], &[])?;
    exits(&output, 0)?;
    let runs = records(&world, "agent").len();
    ensure(runs == 1, || {
        format!("post_agent ran {runs} time(s) in one turn")
    })
}

#[derive(Clone, Copy)]
enum Failure {
    Timeout,
    Strict,
    Malformed,
    Untrusted,
    Trusted,
}

impl Failure {
    const ALL: [Self; 5] = [
        Self::Timeout,
        Self::Strict,
        Self::Malformed,
        Self::Untrusted,
        Self::Trusted,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Timeout => "a_pre_tool_hook_that_times_out",
            Self::Strict => "a_strict_pre_tool_hook_that_fails",
            Self::Malformed => "a_pre_tool_hook_answering_malformed_json",
            Self::Untrusted => "a_project_hook_file_in_an_untrusted_folder",
            Self::Trusted => "a_project_hook_file_under_the_trust_flag",
        }
    }
}

const STDERR: &str = "E2E-HOOK-STDERR";

/// Runs a `bash` call past one failing or project-declared `pre_tool` hook
/// and describes the outcome: the exit code, how often the hook ran, whether
/// the command ran, whether the tool message is an error, and whether it
/// quotes the hook's stderr.
fn failing(ctx: &mut Ctx, legacy: bool, failure: Failure, expected: &str) -> Outcome {
    let world = ctx.world()?;
    let (file, hook, extra): (&str, String, &[&str]) = match failure {
        Failure::Timeout => (
            "vibe-home/hooks.toml",
            recorder(&world, "hook", "pre_tool", "sleep 5")
                .replace("command", "timeout = 1.0\ncommand"),
            &[],
        ),
        Failure::Strict => (
            "vibe-home/hooks.toml",
            recorder(
                &world,
                "hook",
                "pre_tool",
                &format!("echo {STDERR} >&2; exit 1"),
            )
            .replace("command", "strict = true\ncommand"),
            &[],
        ),
        Failure::Malformed => (
            "vibe-home/hooks.toml",
            recorder(&world, "hook", "pre_tool", "echo '{not json'"),
            &[],
        ),
        Failure::Untrusted => (
            "work/.vibe/hooks.toml",
            recorder(&world, "hook", "pre_tool", "true"),
            &[],
        ),
        Failure::Trusted => (
            "work/.vibe/hooks.toml",
            recorder(&world, "hook", "pre_tool", "true"),
            &["--trust"],
        ),
    };
    world.write(file, hook)?;
    let output = run(&world, legacy, vec![touch("marker-bash")], extra)?;
    let message = turn_requests(&world)?
        .last()
        .and_then(|last| tool_message(last, "e2e000001").ok())
        .unwrap_or_default();
    let error = message.starts_with("<tool_error>")
        || serde_json::from_str::<Value>(&message)
            .ok()
            .is_some_and(|parsed| parsed.pointer("/result/error").is_some());
    let found = format!(
        "exit={} ran={} marker={} error={error} stderr={}",
        output
            .code()
            .map_or("signal".to_owned(), |code| code.to_string()),
        records(&world, "hook").len(),
        world.work().join("marker-bash").exists(),
        message.contains(STDERR)
    );
    ensure(found == expected, || {
        format!("observed:\n  {found}\nexpected:\n  {expected}\ntool message: {message}")
    })
}

/// `(harness::failure::case, observation)`, measured against the reference.
const FAILURES: &[(&str, &str)] = &[
    (
        "legacy::failure::a_pre_tool_hook_answering_malformed_json",
        "exit=0 ran=1 marker=true error=false stderr=false",
    ),
    (
        "legacy::failure::a_pre_tool_hook_that_times_out",
        "exit=0 ran=1 marker=true error=false stderr=false",
    ),
    (
        "legacy::failure::a_project_hook_file_in_an_untrusted_folder",
        "exit=0 ran=0 marker=true error=false stderr=false",
    ),
    (
        "legacy::failure::a_project_hook_file_under_the_trust_flag",
        "exit=0 ran=1 marker=true error=false stderr=false",
    ),
    (
        "legacy::failure::a_strict_pre_tool_hook_that_fails",
        "exit=0 ran=1 marker=false error=true stderr=true",
    ),
    (
        "unified::failure::a_pre_tool_hook_answering_malformed_json",
        "exit=0 ran=1 marker=true error=false stderr=false",
    ),
    (
        "unified::failure::a_pre_tool_hook_that_times_out",
        "exit=0 ran=1 marker=true error=false stderr=false",
    ),
    (
        "unified::failure::a_project_hook_file_in_an_untrusted_folder",
        "exit=0 ran=0 marker=true error=false stderr=false",
    ),
    (
        "unified::failure::a_project_hook_file_under_the_trust_flag",
        "exit=0 ran=1 marker=true error=false stderr=false",
    ),
    (
        "unified::failure::a_strict_pre_tool_hook_that_fails",
        "exit=0 ran=1 marker=false error=false stderr=true",
    ),
];
