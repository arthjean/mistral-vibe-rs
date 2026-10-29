//! US-021: provider routing from `config.toml`, authentication, and the golden
//! prompt.

use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, PROVIDER_KEY_VARIABLE, Reply, Scenario, Setup, World, ensure};

use crate::editor::{
    editor, initialize, new_session, prompt, prompt_ends, spawn_command, update_kinds,
};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios: Vec<Scenario> = PROVIDERS
        .iter()
        .map(|&(name, provider)| {
            Scenario::new(
                format!("acp::routing::{name}::the_prompt_reaches_the_configured_provider"),
                move |ctx| routed_prompt(ctx, provider),
            )
            .known_defect(
                "E2E-D02",
                "`vibe-acp` takes its provider from port-only variables, not `config.toml`",
            )
        })
        .collect();
    scenarios.extend([
        Scenario::new(
            "acp::auth::session_new_without_a_key_asks_for_sign_in",
            no_key,
        )
        .known_defect(
            "E2E-D03",
            "`vibe-acp` accepts `session/new` without a key and fails at the first prompt",
        ),
        prompting(Scenario::new(
            "acp::golden::thinking_a_read_and_an_answer_stream_in_order",
            golden_prompt,
        )),
        prompting(Scenario::new(
            "acp::golden::a_401_from_the_provider_fails_the_prompt",
            unauthorized,
        )),
    ]);
    scenarios
}

/// Declares what stops every prompt on the port while E2E-D02 is open: the
/// provider of `config.toml` is never the one a prompt reaches.
pub fn prompting(scenario: Scenario) -> Scenario {
    scenario.known_defect(
        "E2E-D02",
        "a `vibe-acp` prompt never reaches the provider `config.toml` configures",
    )
}

#[derive(Clone, Copy)]
enum Routed {
    /// A Mistral backend on loopback.
    Mistral,
    /// A generic backend keyed by its own variable.
    Generic,
    /// A generic backend that names no key variable, as a local model does.
    Local,
}

const PROVIDERS: [(&str, Routed); 3] = [
    ("mistral", Routed::Mistral),
    ("generic", Routed::Generic),
    ("local", Routed::Local),
];

/// The turn requests, as opposed to probes and utility requests.
pub fn turns(world: &World) -> Vec<vibe_e2e::Entry> {
    world
        .platform()
        .chat_requests()
        .into_iter()
        .filter(|entry| entry.queue == "turn")
        .collect()
}

fn routed_prompt(ctx: &mut Ctx, provider: Routed) -> Outcome {
    let world = ctx.world_with(match provider {
        Routed::Mistral => Setup::mistral(),
        Routed::Generic | Routed::Local => Setup::default(),
    })?;
    let mut command = world.command("vibe-acp")?;
    if let Routed::Local = provider {
        let path = world.vibe_home().join("config.toml");
        let text = std::fs::read_to_string(&path)?;
        let local = text.replace(
            &format!("api_key_env_var = \"{PROVIDER_KEY_VARIABLE}\""),
            "api_key_env_var = \"\"",
        );
        ensure(local != text, || {
            "the world names no key variable".to_owned()
        })?;
        std::fs::write(&path, local)?;
        command = command.env_remove(PROVIDER_KEY_VARIABLE);
    }
    world.platform().reply(Reply::text("routed"));
    let mut rpc = spawn_command(command)?;
    initialize(&mut rpc, json!({}))?;
    let session = new_session(&mut rpc, &world)?;
    let id = rpc.send(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "hello"}]}),
    )?;
    let response = rpc.response_within(id, Duration::from_secs(10))?;
    ensure(response["result"]["stopReason"] == "end_turn", || {
        format!("session/prompt answered {response}")
    })?;
    let requests = turns(&world);
    ensure(requests.len() == 1, || {
        format!(
            "expected one turn request, the platform received {}",
            requests.len()
        )
    })?;
    let model = requests[0].body["model"].clone();
    ensure(model == world.provider().model_name(), || {
        format!(
            "the turn request names the model {model}, not {}",
            world.provider().model_name()
        )
    })
}

fn no_key(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::mistral())?;
    let mut rpc = spawn_command(world.command("vibe-acp")?.env_remove("MISTRAL_API_KEY"))?;
    let initialized = initialize(&mut rpc, json!({}))?;
    let methods = initialized["authMethods"].as_array().map_or(0, Vec::len);
    ensure(methods > 0, || {
        format!("initialize advertised no authentication method: {initialized}")
    })?;
    let response = rpc.request(
        "session/new",
        json!({"cwd": world.work(), "mcpServers": []}),
    )?;
    ensure(response["error"]["code"] == -32000, || {
        format!("session/new without a key answered {response}, expected error -32000")
    })?;
    ensure(world.platform().chat_requests().is_empty(), || {
        "a chat request was sent without a key".to_owned()
    })
}

/// The content updates of the golden prompt, in the order the reference
/// streams them; usage and command updates interleave as they come.
const GOLDEN_ORDER: [&str; 5] = [
    "user_message_chunk",
    "agent_thought_chunk",
    "tool_call",
    "tool_call_update",
    "agent_message_chunk",
];

/// Updates the reference sends around the content, in no fixed position.
const GOLDEN_ALSO: [&str; 3] = [
    "usage_update",
    "available_commands_update",
    "session_info_update",
];

fn golden_prompt(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.write("work/seed.txt", "seed contents\n")?;
    world.platform().replies([
        Reply::tool_call("read_file", json!({"path": "seed.txt"})).reasoning("weighing the file"),
        Reply::text("the file is read"),
    ]);
    world.platform().exact();
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    prompt_ends(&mut rpc, &session, "read the seed", "end_turn")?;
    // The command list is announced once the session settles, which may be
    // after the prompt ends.
    if !update_kinds(&rpc)
        .iter()
        .any(|k| k == "available_commands_update")
    {
        rpc.await_notification(
            |m| m["params"]["update"]["sessionUpdate"] == "available_commands_update",
            Duration::from_secs(10),
        )?;
    }
    let kinds = update_kinds(&rpc);
    let content: Vec<&str> = kinds
        .iter()
        .map(String::as_str)
        .filter(|kind| !GOLDEN_ALSO.contains(kind))
        .collect();
    ensure(content == GOLDEN_ORDER, || {
        format!("content updates {content:?}, expected {GOLDEN_ORDER:?} (all: {kinds:?})")
    })?;
    for kind in GOLDEN_ALSO {
        ensure(kinds.iter().any(|k| k == kind), || {
            format!("no {kind} among the updates {kinds:?}")
        })?;
    }
    let tool_updates: Vec<&Value> = rpc
        .notifications()
        .iter()
        .map(|m| &m["params"]["update"])
        .filter(|u| u["sessionUpdate"] == "tool_call" || u["sessionUpdate"] == "tool_call_update")
        .collect();
    let first = tool_updates.first().ok_or("no tool call update")?;
    let last = tool_updates.last().ok_or("no tool call update")?;
    ensure(
        first["kind"] == "read" && last["status"] == "completed",
        || format!("the read was announced as {first} and ended as {last}"),
    )?;
    ensure(
        tool_updates
            .iter()
            .all(|u| u["toolCallId"] == first["toolCallId"]),
        || "the tool call updates name more than one call".to_owned(),
    )
}

fn unauthorized(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .reply(Reply::status(401).body(r#"{"message":"Unauthorized","type":"invalid_request"}"#));
    world.platform().exact();
    let mut rpc = editor(&world, &[])?;
    let session = new_session(&mut rpc, &world)?;
    let response = prompt(&mut rpc, &session, "hello")?;
    // The reference answers with its authentication error code, naming the
    // variable the key was read from, and does not retry.
    let error = &response["error"];
    ensure(
        error["code"] == -31002
            && error["message"]
                .as_str()
                .is_some_and(|m| m.contains(PROVIDER_KEY_VARIABLE)),
        || format!("a 401 answered {response}, expected error -31002 naming the key variable"),
    )?;
    let requests = turns(&world);
    ensure(requests.len() == 1, || {
        format!(
            "a 401 was followed by {} turn requests, expected 1",
            requests.len()
        )
    })
}
