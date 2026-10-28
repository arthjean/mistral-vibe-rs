//! US-012: what a long session depends on, measured at the binary boundary:
//! reasoning replay, subagents, compaction, project context, wire metadata,
//! and paced streaming.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Output, Reply, Scenario, Setup, World, ensure};

use crate::cli::{
    body_str, chat_count, chats, exits, interrupted_entries, stream_entries, system_text, vibe,
    with_role,
};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (provider, mistral) in [("mistral_thinking", true), ("generic_reasoning", false)] {
        for (harness, legacy) in [("unified", false), ("legacy", true)] {
            scenarios.push(Scenario::new(
                format!("engine::reasoning::{provider}_is_replayed::{harness}"),
                move |ctx| reasoning(ctx, mistral, legacy),
            ));
        }
    }
    scenarios.extend([
        Scenario::new(
            "engine::subagents::a_custom_subagent_runs_on_its_own_prompt",
            subagent_prompt,
        )
        .known_defect(
            "E2E-D16",
            "a custom subagent's `system_prompt_id` is missing from its system prompt",
        ),
        Scenario::new(
            "engine::subagents::the_spawn_event_names_the_agent",
            subagent_name,
        )
        .known_defect(
            "E2E-D17",
            "`subagent.spawn` always publishes a null agent name",
        ),
        Scenario::new(
            "engine::compaction::the_summary_request_streams_on_the_compaction_model",
            compaction,
        )
        .known_defect(
            "E2E-D15",
            "compaction ignores `[compaction_model]` and sends an unstreamed request",
        ),
        Scenario::new(
            "engine::compaction::a_summary_with_tool_calls_is_retried_once",
            compaction_retry,
        )
        .known_defect(
            "E2E-D15",
            "a compaction summary answered with tool calls is not retried",
        ),
    ]);
    for case in CONTEXT_CASES {
        scenarios.push(Scenario::new(
            format!("engine::project_context::{}", case.name),
            move |ctx| project_context(ctx, &case),
        ));
    }
    scenarios.push(
        Scenario::new(
            "engine::wire::legacy_turns_offer_tool_choice_auto",
            tool_choice,
        )
        .known_defect(
            "E2E-D17",
            "legacy turn requests omit `tool_choice: \"auto\"`",
        ),
    );
    for (harness, legacy) in [("unified", false), ("legacy", true)] {
        scenarios.push(Scenario::new(
            format!("engine::wire::mistral_turn_metadata::{harness}"),
            move |ctx| metadata(ctx, legacy),
        ));
    }
    scenarios.extend([
        Scenario::new(
            "engine::streaming::paced_output_streams_and_stops_on_sigint",
            paced,
        ),
        Scenario::new(
            "engine::streaming::an_interrupted_answer_is_not_printed",
            interrupted_answer,
        )
        .known_defect(
            "E2E-D37",
            "SIGINT during `--output streaming` prints the partial answer as a completed entry",
        ),
    ]);
    scenarios
}

fn reasoning(ctx: &mut Ctx, mistral: bool, legacy: bool) -> Outcome {
    let (setup, first, last) = if mistral {
        (
            Setup::mistral(),
            Reply::tool_call("bash", json!({"command": "echo hi"})).thinking("E2E-THINK-MARK"),
            Reply::text("final").thinking("E2E-TAIL-MARK"),
        )
    } else {
        (
            Setup::default(),
            Reply::tool_call("bash", json!({"command": "echo hi"})).reasoning("E2E-REASON-MARK"),
            Reply::text("final").reasoning("E2E-TAIL-MARK"),
        )
    };
    let world = ctx.world_with(setup)?;
    world.platform().replies([first, last]);
    world.platform().exact();
    let mut arguments = vec!["-p", "go", "--output", "text", "--auto-approve"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, 0)?;
    // shared_with_reference: whether text output shows reasoning is the
    // reference's behavior, not a contract (PRD shared-defect list).
    ensure(output.stdout == "final\n", || {
        format!("the text output is {:?}", output.stdout)
    })?;
    let requests = chat_count(&world, 2)?;
    let assistant = with_role(&requests[1], "assistant")
        .first()
        .map(|m| (*m).clone())
        .ok_or("the second turn replays no assistant message")?;
    if mistral {
        // The Unified Harness replays the thinking chunk as content; the legacy
        // loop drops it.
        let replayed = assistant.to_string().contains("E2E-THINK-MARK");
        ensure(replayed != legacy, || {
            format!("thinking replayed: {replayed}, on the legacy harness: {legacy}: {assistant}")
        })
    } else {
        let replayed = assistant.get("reasoning_content").and_then(Value::as_str);
        ensure(replayed == Some("E2E-REASON-MARK"), || {
            format!("the replayed assistant message is {assistant}")
        })
    }
}

const SUBAGENT_MARK: &str = "E2E-SUBAGENT-MARK";

/// A parent turn that spawns the custom `scout` subagent from code mode and
/// waits for it. The child's answer is routed by the marker its prompt file
/// carries.
fn subagent_world(ctx: &mut Ctx) -> Outcome<(World, vibe_e2e::Output)> {
    let world = ctx.world()?;
    world.write(
        "vibe-home/agents/scout.toml",
        "display_name = \"Scout\"\ndescription = \"Scouts the work tree\"\n\
         agent_type = \"subagent\"\nsystem_prompt_id = \"scout\"\n",
    )?;
    world.write(
        "vibe-home/prompts/scout.md",
        format!("You scout the work tree. {SUBAGENT_MARK}\n"),
    )?;
    world
        .platform()
        .when(SUBAGENT_MARK, Reply::text("child done"));
    world.platform().replies([
        Reply::tool_call(
            "run_typescript",
            json!({"code": "await tools.subagent.spawn({agentName: \"scout-one\", message: \
                    \"look around\", agentType: \"scout\"});\nreturn await \
                    tools.subagent.wait({agentName: \"scout-one\", timeoutMs: 20000});"}),
        ),
        Reply::text("parent done"),
    ]);
    world.platform().exact();
    let output = vibe(
        &world,
        &["-p", "go", "--output", "streaming", "--auto-approve"],
    )?;
    exits(&output, 0)?;
    Ok((world, output))
}

fn subagent_prompt(ctx: &mut Ctx) -> Outcome {
    let (world, output) = subagent_world(ctx)?;
    let child: Vec<_> = chats(&world)
        .into_iter()
        .filter(|entry| entry.queue == format!("when:{SUBAGENT_MARK}"))
        .collect();
    ensure(child.len() == 1, || {
        format!("{} request(s) carried the subagent's prompt", child.len())
    })?;
    ensure(system_text(&child[0]).contains(SUBAGENT_MARK), || {
        "the marker is not in the child's system message".to_owned()
    })?;
    ensure(output.stdout.contains("parent done"), || {
        format!("the parent turn did not finish: {}", output.stdout)
    })
}

fn subagent_name(ctx: &mut Ctx) -> Outcome {
    let (_world, output) = subagent_world(ctx)?;
    let spawns: Vec<Value> = stream_entries(&output.stdout)?
        .into_iter()
        .filter(|e| e.pointer("/detail/toolName").and_then(Value::as_str) == Some("subagent.spawn"))
        .collect();
    ensure(spawns.len() == 1, || {
        format!("{} subagent.spawn entries were published", spawns.len())
    })?;
    let name = spawns[0].pointer("/detail/agentName");
    ensure(name.and_then(Value::as_str) == Some("scout-one"), || {
        format!("the spawn entry names the agent {name:?}")
    })
}

const COMPACTION_SETUP: &str = "auto_compact_threshold = 20000\n\n[compaction_model]\n\
                                name = \"e2e-compact-v1\"\nprovider = \"e2e-provider\"\n\
                                alias = \"compact\"\ntemperature = 0.7\n";

/// A first turn whose usage crosses the threshold, so compaction runs before
/// the next request.
fn crossing() -> Reply {
    Reply::tool_call("bash", json!({"command": "echo hi"})).usage(30_000, 5)
}

fn compaction(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::default().config(COMPACTION_SETUP))?;
    world.platform().replies([
        crossing(),
        Reply::text("E2E-SUMMARY-MARK"),
        Reply::text("after compaction"),
    ]);
    world.platform().exact();
    let output = vibe(&world, &["-p", "go", "--output", "text", "--auto-approve"])?;
    exits(&output, 0)?;
    ensure(output.stdout == "after compaction\n", || {
        output.stdout.clone()
    })?;
    let requests = chat_count(&world, 3)?;
    let summary = &requests[1];
    ensure(body_str(summary, "model") == Some("e2e-compact-v1"), || {
        format!("the summary request named {:?}", body_str(summary, "model"))
    })?;
    ensure(summary.body.get("stream") == Some(&json!(true)), || {
        format!(
            "the summary request was not streamed: {:?}",
            summary.body.get("stream")
        )
    })?;
    ensure(summary.body.get("temperature") == Some(&json!(0.7)), || {
        format!(
            "the summary request used the temperature {:?}",
            summary.body.get("temperature")
        )
    })?;
    let next = with_role(&requests[2], "user")
        .iter()
        .any(|m| m.to_string().contains("E2E-SUMMARY-MARK"));
    ensure(next, || {
        "the next turn does not carry the summary".to_owned()
    })
}

fn compaction_retry(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::default().config(COMPACTION_SETUP))?;
    let summary_call = || Reply::tool_call("bash", json!({"command": "echo summary"}));
    world.platform().replies([
        crossing(),
        summary_call(),
        summary_call(),
        summary_call(),
        Reply::text("unexpected answer"),
    ]);
    let output = vibe(&world, &["-p", "go", "--output", "text", "--auto-approve"])?;
    ensure(!output.success(), || {
        format!("the run succeeded: {}", output.stdout)
    })?;
    chat_count(&world, 3).map(drop)
}

struct ContextCase {
    name: &'static str,
    trusted: bool,
    config: &'static str,
    cwd: &'static str,
    markers: &'static [&'static str],
}

const CONTEXT_CASES: [ContextCase; 7] = [
    ContextCase {
        name: "trusted_root_reads_user_and_project_docs",
        trusted: true,
        config: "",
        cwd: "",
        markers: &["E2E-USER-DOC", "E2E-PROJECT-DOC"],
    },
    ContextCase {
        name: "trusted_subdirectory_reads_its_nearest_project_doc",
        trusted: true,
        config: "",
        cwd: "sub",
        markers: &["E2E-USER-DOC", "E2E-NESTED-DOC"],
    },
    ContextCase {
        name: "untrusted_reads_the_user_doc_only",
        trusted: false,
        config: "",
        cwd: "sub",
        markers: &["E2E-USER-DOC"],
    },
    ContextCase {
        name: "include_project_context_off_reads_no_doc",
        trusted: true,
        config: "include_project_context = false",
        cwd: "sub",
        markers: &[],
    },
    // The other `include_*` settings gate sections of their own, never the
    // AGENTS.md docs (`vibe/core/system_prompt.py`, `vibe/app_server/_runtime.py`).
    ContextCase {
        name: "include_prompt_detail_off_keeps_the_docs",
        trusted: true,
        config: "include_prompt_detail = false",
        cwd: "",
        markers: &["E2E-USER-DOC", "E2E-PROJECT-DOC"],
    },
    ContextCase {
        name: "include_model_info_off_keeps_the_docs",
        trusted: true,
        config: "include_model_info = false",
        cwd: "",
        markers: &["E2E-USER-DOC", "E2E-PROJECT-DOC"],
    },
    ContextCase {
        name: "include_commit_signature_off_keeps_the_docs",
        trusted: true,
        config: "include_commit_signature = false",
        cwd: "",
        markers: &["E2E-USER-DOC", "E2E-PROJECT-DOC"],
    },
];

const DOC_MARKERS: [&str; 3] = ["E2E-USER-DOC", "E2E-PROJECT-DOC", "E2E-NESTED-DOC"];

/// Asserted by marker, never by length: the prompt names the weekday.
fn project_context(ctx: &mut Ctx, case: &ContextCase) -> Outcome {
    let world = ctx.world_with(Setup::default().config(case.config))?;
    world.write("vibe-home/AGENTS.md", "User guidance E2E-USER-DOC\n")?;
    world.write("work/AGENTS.md", "Project guidance E2E-PROJECT-DOC\n")?;
    world.write("work/sub/AGENTS.md", "Nested guidance E2E-NESTED-DOC\n")?;
    world.platform().reply(Reply::text("ok"));
    let cwd: PathBuf = world.work().join(case.cwd);
    let mut arguments = vec!["-p", "go", "--output", "text"];
    if case.trusted {
        arguments.push("--trust");
    }
    let output = world
        .command("vibe")?
        .args(arguments)
        .current_dir(cwd)
        .run()?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    let system = system_text(&requests[0]);
    let present: Vec<&str> = DOC_MARKERS
        .into_iter()
        .filter(|marker| system.contains(marker))
        .collect();
    ensure(present == case.markers, || {
        format!(
            "the system message carries {present:?}, not {:?}",
            case.markers
        )
    })
}

fn tool_choice(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("ok"));
    let output = vibe(
        &world,
        &["-p", "go", "--output", "text", "--legacy-harness"],
    )?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    ensure(
        body_str(&requests[0], "tool_choice") == Some("auto"),
        || {
            format!(
                "the turn offered tool_choice {:?}",
                requests[0].body.get("tool_choice")
            )
        },
    )
}

/// The metadata keys a Mistral-backend turn carries.
const METADATA_KEYS: [&str; 14] = [
    "agent_entrypoint",
    "agent_version",
    "arch",
    "call_source",
    "call_type",
    "client_name",
    "client_version",
    "host_kind",
    "message_id",
    "os",
    "os_version",
    "session_id",
    "terminal_emulator",
    "version",
];

fn metadata(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world_with(Setup::mistral())?;
    let version_output = vibe(&world, &["--version"])?;
    exits(&version_output, 0)?;
    let version = version_output
        .stdout
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_owned();
    world.platform().reply(Reply::text("ok"));
    let mut arguments = vec!["-p", "go", "--output", "text"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    let turn = &requests[0];
    let metadata = turn
        .body
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or("the turn carries no metadata")?;
    let mut keys: Vec<&str> = metadata.keys().map(String::as_str).collect();
    keys.sort_unstable();
    ensure(keys == METADATA_KEYS, || {
        format!("the metadata keys are {keys:?}")
    })?;
    let field = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    for (key, expected) in [
        ("agent_entrypoint", "programmatic"),
        ("client_name", "vibe_programmatic"),
        ("call_type", "main_call"),
        ("call_source", "vibe_code"),
        ("agent_version", version.as_str()),
        ("client_version", version.as_str()),
        ("version", version.as_str()),
    ] {
        ensure(field(key) == expected, || {
            format!("metadata {key} is {:?}, not {expected:?}", field(key))
        })?;
    }
    let affinity = turn.header("x-affinity").unwrap_or_default();
    ensure(
        !affinity.is_empty() && affinity == field("session_id"),
        || {
            format!(
                "the affinity header {affinity:?} is not the session {:?}",
                field("session_id")
            )
        },
    )?;
    let agent = turn.header("user-agent").unwrap_or_default();
    // The reference's Unified Harness sends its SDK's default user agent,
    // which carries no Vibe version (US-008); the legacy loop stamps it.
    let stamped = agent.contains(&format!("Mistral-Vibe/{version}"));
    ensure(
        agent.starts_with("mistral-client-python/") && (stamped || !legacy),
        || format!("the user agent is {agent:?} for version {version}"),
    )
}

/// `--output streaming` writes whole entries, never deltas, in both
/// implementations (shared_with_reference, measured on 2026-10-28): the user
/// message reaches stdout before the model streams, and the assistant message
/// only once its stream ends. The run is interrupted after three pieces.
fn interrupted_paced_run(ctx: &mut Ctx) -> Outcome<Output> {
    let world = ctx.world()?;
    let pieces: Vec<String> = (1..=10).map(|n| format!("piece {n} ")).collect();
    world.platform().reply(Reply::chunks(pieces).paced(200));
    let mut child = world
        .command("vibe")?
        .args(["-p", "go", "--output", "streaming"])
        .spawn()?;
    let first: Value = serde_json::from_str(&child.next_line(Duration::from_secs(30))?)?;
    // The role chunk, ten pieces, the finish chunk, and `[DONE]`.
    let total = 13;
    let served = world.platform().events_served(1);
    ensure(
        first.get("role").and_then(Value::as_str) == Some("user") && served < total,
        || format!("after {served} of {total} events, the first entry is {first}"),
    )?;
    // The role chunk and three pieces.
    world
        .platform()
        .await_events(1, 4, Duration::from_secs(30))?;
    ensure(!child.stdout().contains("piece "), || {
        format!(
            "assistant text reached stdout before its stream ended: {}",
            child.stdout()
        )
    })?;
    child.interrupt()?;
    let output = child.wait_within(Duration::from_secs(5))?;
    exits(&output, 0)?;
    ensure(!output.stdout.contains("piece 10"), || {
        format!("the interrupt did not stop the stream: {}", output.stdout)
    })?;
    interrupted_entries(&output.stdout)?;
    Ok(output)
}

fn paced(ctx: &mut Ctx) -> Outcome {
    interrupted_paced_run(ctx).map(drop)
}

fn interrupted_answer(ctx: &mut Ctx) -> Outcome {
    let output = interrupted_paced_run(ctx)?;
    ensure(!output.stdout.contains("piece "), || {
        format!("the interrupted answer was printed: {}", output.stdout)
    })
}
