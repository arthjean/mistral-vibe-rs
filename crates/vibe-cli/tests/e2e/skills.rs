//! US-019: user, project, plugin, and builtin skills, plugins with an MCP
//! server, and connectors, under `vibe -p`.

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Mcp, Outcome, Reply, RouteReply, Scenario, Setup, World, ensure, text_result};

use crate::cli::{chat_count, edit_config, exits, last_user, system_text, vibe};
use crate::mcp::outcome;
use crate::tools::{tool_message, turn_requests};

#[derive(Clone, Copy, PartialEq)]
enum Trust {
    None,
    Flag,
    File,
}

impl Trust {
    const ALL: [Self; 3] = [Self::None, Self::Flag, Self::File];

    fn name(self) -> &'static str {
        match self {
            Self::None => "untrusted",
            Self::Flag => "trust_flag",
            Self::File => "trusted_folders",
        }
    }
}

const PROJECT_SKILLS: &str = "the default harness never loads project skills";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (harness, legacy) in [("unified", false), ("legacy", true)] {
        for trust in Trust::ALL {
            let name = format!("skills::offered::{harness}::{}", trust.name());
            let expected = OFFERED
                .iter()
                .find(|(cell, _)| name.ends_with(cell))
                .map_or("", |(_, line)| *line);
            let scenario = Scenario::new(name, move |ctx| offered(ctx, legacy, trust, expected));
            scenarios.push(if !legacy && trust != Trust::None {
                scenario.known_defect("E2E-D18", PROJECT_SKILLS)
            } else {
                scenario
            });
        }
        scenarios.push(Scenario::new(
            format!("skills::slash_command::{harness}"),
            move |ctx| slash_command(ctx, legacy),
        ));
    }
    scenarios.extend([
        Scenario::new(
            "skills::a_malformed_skill_is_skipped_beside_a_valid_one",
            malformed_skill,
        ),
        Scenario::new("plugins::skills_and_mcp_tools", plugin_tools),
        Scenario::new("plugins::materialized_state", plugin_state).known_defect(
            "E2E-D34",
            "the plugin store has no `blobs/` or `manifests/`, and its shards and `tmp/` are \
             created with wider modes",
        ),
        Scenario::new("connectors::without_a_key", |ctx| {
            connectors(
                ctx,
                false,
                "listed=false call=TypeError bootstrap_requests=0 gateway_calls=0",
            )
        }),
        Scenario::new("connectors::with_a_key", |ctx| {
            connectors(
                ctx,
                true,
                "listed=true call=ok \"found report.pdf\" bootstrap_requests=1 gateway_calls=1",
            )
        }),
    ]);
    scenarios
}

fn skill(name: &str) -> String {
    format!("---\nname: {name}\ndescription: Skill {name} for e2e\n---\n\nBODY-{name}\n")
}

/// Every skill name a world may offer, plugin and builtin ones included.
const CANDIDATES: &[&str] = &[
    "user-skill",
    "vibe-proj",
    "agents-proj",
    "broken",
    "e2e_tools:plug-skill",
    "proj_tools:proj-plug-skill",
    "skill-creator",
    "worktree",
    "create-plugin",
];

/// The candidate names the first turn's system message carries.
fn named(world: &World) -> Outcome<String> {
    let requests = chat_count(world, 1)?;
    let system = system_text(&requests[0]);
    Ok(CANDIDATES
        .iter()
        .filter(|name| system.contains(*name))
        .copied()
        .collect::<Vec<_>>()
        .join(","))
}

fn offered(ctx: &mut Ctx, legacy: bool, trust: Trust, expected: &str) -> Outcome {
    let world = ctx.world()?;
    world.write("vibe-home/skills/user-skill/SKILL.md", skill("user-skill"))?;
    world.write("work/.vibe/skills/vibe-proj/SKILL.md", skill("vibe-proj"))?;
    world.write(
        "work/.agents/skills/agents-proj/SKILL.md",
        skill("agents-proj"),
    )?;
    // A front matter that does not parse, beside the valid skills.
    world.write(
        "vibe-home/skills/broken/SKILL.md",
        "---\nname: [broken\n---\nBODY-broken\n",
    )?;
    plugins(&world)?;
    if trust == Trust::File {
        let work = world.work().to_string_lossy().into_owned();
        world.write(
            "vibe-home/trusted_folders.toml",
            format!("trusted = [{}]\nuntrusted = []\n", Value::String(work)),
        )?;
    }
    world.platform().reply(Reply::text("done"));
    let mut arguments = vec!["-p", "go", "--output", "text"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    if trust == Trust::Flag {
        arguments.push("--trust");
    }
    exits(&vibe(&world, &arguments)?, 0)?;
    let found = named(&world)?;
    ensure(found == expected, || {
        format!("the system message names {found:?}, expected {expected:?}")
    })
}

/// A skill whose front matter does not parse is left out without a word,
/// and the valid skill beside it loads.
fn malformed_skill(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.write("vibe-home/skills/user-skill/SKILL.md", skill("user-skill"))?;
    world.write(
        "vibe-home/skills/broken/SKILL.md",
        "---\nname: [broken\n---\nBODY-broken\n",
    )?;
    world.platform().reply(Reply::text("done"));
    let output = vibe(&world, &["-p", "go", "--output", "text", "--trust"])?;
    exits(&output, 0)?;
    let found = named(&world)?;
    ensure(
        found.contains("user-skill") && !found.contains("broken"),
        || format!("the system message names {found:?}"),
    )?;
    ensure(!output.stderr.contains("broken"), || {
        format!("the run reported the malformed skill: {}", output.stderr)
    })
}

/// `(harness::trust, skills named)`, measured against the reference.
const OFFERED: &[(&str, &str)] = &[
    ("legacy::untrusted", "user-skill,skill-creator"),
    (
        "legacy::trust_flag",
        "user-skill,vibe-proj,agents-proj,skill-creator",
    ),
    (
        "legacy::trusted_folders",
        "user-skill,vibe-proj,agents-proj,skill-creator",
    ),
    (
        "unified::trusted_folders",
        "user-skill,vibe-proj,agents-proj,e2e_tools:plug-skill,proj_tools:proj-plug-skill,skill-creator,worktree,create-plugin",
    ),
    (
        "unified::trust_flag",
        "user-skill,vibe-proj,agents-proj,e2e_tools:plug-skill,proj_tools:proj-plug-skill,skill-creator,worktree,create-plugin",
    ),
    (
        "unified::untrusted",
        "user-skill,e2e_tools:plug-skill,skill-creator,worktree,create-plugin",
    ),
];

fn slash_command(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    world.write("vibe-home/skills/user-skill/SKILL.md", skill("user-skill"))?;
    world.platform().reply(Reply::text("done"));
    let mut arguments = vec!["-p", "/user-skill tidy up", "--output", "text"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    exits(&vibe(&world, &arguments)?, 0)?;
    let requests = chat_count(&world, 1)?;
    let carried: String = requests[0]
        .messages()
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .map(Value::to_string)
        .collect();
    ensure(carried.contains("BODY-user-skill"), || {
        format!(
            "the turn does not carry the skill body; its last user message is {:?}",
            last_user(&requests[0])
        )
    })
}

/// A user plugin with a skill and a stdio MCP server, and a project plugin
/// with a skill.
fn plugins(world: &World) -> Outcome {
    let manifest = |name: &str| {
        json!({
            "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
            "name": name, "version": "1.0.0", "description": "An e2e plugin.",
        })
        .to_string()
    };
    world.write(
        "vibe-home/plugins/e2e-tools/plugin.json",
        manifest("e2e-tools"),
    )?;
    world.write(
        "vibe-home/plugins/e2e-tools/skills/plug-skill/SKILL.md",
        skill("plug-skill"),
    )?;
    let server = world.mcp_stdio(
        "plugin",
        &Mcp::new()
            .tool(json!({"name": "lookup", "inputSchema": {"type": "object"}}))
            .react("lookup", text_result("from the plugin")),
    )?;
    let launch = [&[server.command().to_owned()], server.args()].concat();
    world.write(
        "vibe-home/plugins/e2e-tools/mcp.json",
        json!({
            "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
            // A plugin launches a bare executable or one inside its root, so
            // `env` starts the server.
            "mcpServers": {"lookup": {"type": "stdio", "command": "env",
                                      "args": launch}},
        })
        .to_string(),
    )?;
    world.write(
        "work/.vibe/plugins/proj-tools/plugin.json",
        manifest("proj-tools"),
    )?;
    world.write(
        "work/.vibe/plugins/proj-tools/skills/proj-plug-skill/SKILL.md",
        skill("proj-plug-skill"),
    )?;
    Ok(())
}

fn plugin_tools(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    plugins(&world)?;
    world.platform().replies([
        Reply::tool_call(
            "search_tool_functions",
            json!({"mode": "all_connector_capabilities", "connectors": ["mcp_lookup"]}),
        ),
        Reply::tool_call(
            "run_typescript",
            json!({"code": "return await tools.mcp_lookup.lookup({});"}),
        ),
        Reply::text("done"),
    ]);
    let output = vibe(
        &world,
        &["-p", "go", "--output", "text", "--trust", "--auto-approve"],
    )?;
    exits(&output, 0)?;
    let requests = turn_requests(&world)?;
    let system = system_text(&requests[0]);
    for name in ["e2e_tools:plug-skill", "proj_tools:proj-plug-skill"] {
        ensure(system.contains(name), || {
            format!("the system message does not offer the plugin skill {name}")
        })?;
    }
    let last = requests.last().ok_or("no turn request")?;
    let capabilities = tool_message(last, "e2e000001")?;
    ensure(capabilities.contains("mcp_lookup.lookup"), || {
        format!("the plugin server's tool is not listed: {capabilities}")
    })?;
    let call = outcome(&tool_message(last, "e2e000002")?);
    ensure(call == r#"ok "from the plugin""#, || {
        format!("the plugin tool call ended {call}")
    })
}

/// The plugin store under the vibe home, each digest, shard, and session id
/// replaced, with the mode of each entry.
fn plugin_store(world: &World) -> String {
    let root = world.vibe_home().join("logs/session/plugins");
    let mut lines = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let normalized: Vec<String> = relative
                .split('/')
                .map(|part| {
                    let hex = part.chars().all(|c| c.is_ascii_hexdigit());
                    match part.len() {
                        64 if hex => "<digest>".to_owned(),
                        2 if hex => "<shard>".to_owned(),
                        36 if part.matches('-').count() == 4 => "<session>".to_owned(),
                        _ => part.to_owned(),
                    }
                })
                .collect();
            let metadata = entry.metadata();
            #[cfg(unix)]
            let mode = metadata.as_ref().map_or(0, |m| {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o777
            });
            #[cfg(not(unix))]
            let mode = 0;
            lines.push(format!("{:o} {}", mode, normalized.join("/")));
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    lines.sort();
    lines.dedup();
    lines.join("\n")
}

fn plugin_state(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    plugins(&world)?;
    world.platform().reply(Reply::text("done"));
    exits(
        &vibe(&world, &["-p", "go", "--output", "text", "--trust"])?,
        0,
    )?;
    let found = plugin_store(&world);
    ensure(found == PLUGIN_STORE, || {
        format!("the plugin store holds:\n{found}\nexpected:\n{PLUGIN_STORE}")
    })
}

/// The reference's plugin store after one run with both plugins.
const PLUGIN_STORE: &str = "400 blobs/<shard>/<digest>\n\
                            400 manifests/<shard>/<digest>\n\
                            400 packages/<shard>/<digest>/mcp.json\n\
                            400 packages/<shard>/<digest>/plugin.json\n\
                            400 packages/<shard>/<digest>/skills/create-plugin/SKILL.md\n\
                            400 packages/<shard>/<digest>/skills/plug-skill/SKILL.md\n\
                            400 packages/<shard>/<digest>/skills/proj-plug-skill/SKILL.md\n\
                            400 packages/<shard>/<digest>/skills/skill-creator/SKILL.md\n\
                            400 packages/<shard>/<digest>/skills/vibe/SKILL.md\n\
                            400 packages/<shard>/<digest>/skills/worktree/SKILL.md\n\
                            500 packages/<shard>/<digest>\n\
                            500 packages/<shard>/<digest>/skills\n\
                            500 packages/<shard>/<digest>/skills/create-plugin\n\
                            500 packages/<shard>/<digest>/skills/plug-skill\n\
                            500 packages/<shard>/<digest>/skills/proj-plug-skill\n\
                            500 packages/<shard>/<digest>/skills/skill-creator\n\
                            500 packages/<shard>/<digest>/skills/vibe\n\
                            500 packages/<shard>/<digest>/skills/worktree\n\
                            700 blobs/<shard>\n\
                            700 manifests/<shard>\n\
                            700 packages/<shard>\n\
                            700 tmp\n\
                            755 blobs\n\
                            755 data\n\
                            755 data/<session>\n\
                            755 data/<session>/e2e-tools\n\
                            755 data/<session>/proj-tools\n\
                            755 data/<session>/vibe\n\
                            755 manifests\n\
                            755 packages";

fn connectors(ctx: &mut Ctx, key: bool, expected: &str) -> Outcome {
    let setup = if key {
        Setup::mistral()
    } else {
        Setup::default()
    };
    let world = ctx.world_with(setup)?;
    edit_config(&world, |text| {
        format!("{text}\n[[connectors]]\nname = \"Drive\"\n")
    })?;
    world.platform().route(
        "connectors_bootstrap",
        RouteReply::json(
            200,
            &json!({"connectors": [{
                "id": "c-drive", "name": "Drive", "protocol": "mcp", "status": {"is_ready": true},
                "tools": [{"name": "search", "inputSchema": {"type": "object",
                           "properties": {"query": {"type": "string"}}, "required": ["query"]}}],
            }]}),
        ),
    );
    world.platform().gateway(
        "c-drive",
        &Mcp::new()
            .tool(json!({"name": "search", "inputSchema": {"type": "object"}}))
            .react("search", text_result("found report.pdf")),
    );
    world.platform().replies([
        Reply::tool_call(
            "search_tool_functions",
            json!({"mode": "all_connector_capabilities", "connectors": ["connector_Drive"]}),
        ),
        Reply::tool_call(
            "run_typescript",
            json!({"code": "return await tools.connector_Drive.search({query: 'report'});"}),
        ),
        Reply::text("done"),
    ]);
    let output = vibe(&world, &["-p", "go", "--output", "text", "--auto-approve"])?;
    exits(&output, 0)?;
    let last = turn_requests(&world)?.pop().ok_or("no turn request")?;
    let listed = tool_message(&last, "e2e000001")?.contains("connector_Drive.search");
    let call = outcome(&tool_message(&last, "e2e000002")?);
    let bootstrap = world
        .platform()
        .journal()
        .iter()
        .filter(|entry| entry.queue == "connectors_bootstrap")
        .count();
    let tools_called = world
        .platform()
        .mcp_requests("gateway-c-drive")
        .iter()
        .filter(|entry| entry.body.get("method").and_then(Value::as_str) == Some("tools/call"))
        .count();
    let found = format!(
        "listed={listed} call={call} bootstrap_requests={} gateway_calls={tools_called}",
        bootstrap.min(1)
    );
    ensure(found == expected, || {
        format!("observed:\n  {found}\nexpected:\n  {expected}")
    })
}
