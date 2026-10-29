//! US-018: MCP servers over stdio and streamable HTTP under `vibe -p`: their
//! tools through `run_typescript` on the default harness and as functions on
//! the legacy one, each call outcome, each server setting, and `vibe mcp`.

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Mcp, Outcome, Reply, Scenario, StdioMcp, World, ensure, text_result};

use crate::cli::{edit_config, exits, vibe};
use crate::tools::{canonical, tool_message, turn_requests};

/// Measured on windows-2022 outside the harness too: `vibe -p` on either
/// harness never starts a stdio server `config.toml` declares, while the
/// same server answers `initialize` when started by hand.
const STDIO_ON_WINDOWS: &str =
    "windows: E2E-D60 `vibe` never starts a stdio MCP server `config.toml` declares (US-034)";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        Scenario::new(
            "mcp::unified::a_stdio_tool_returns_its_structured_result",
            structured_result,
        )
        .windows_ignore(STDIO_ON_WINDOWS),
        Scenario::new(
            "mcp::unified::the_declaration_types_the_return_from_the_output_schema",
            typed_declaration,
        )
        .windows_ignore(STDIO_ON_WINDOWS)
        .known_defect(
            "E2E-D41",
            "the declaration of an MCP tool types its return as `unknown`, losing `outputSchema`",
        ),
        Scenario::new("mcp::legacy::auto_approve_runs_an_mcp_tool", |ctx| {
            legacy_call(ctx, true)
        })
        .windows_ignore(STDIO_ON_WINDOWS)
        .known_defect(
            "E2E-D11",
            "on the legacy harness `--auto-approve` does not cover MCP tools",
        ),
        Scenario::new("mcp::legacy::permission_always_runs_an_mcp_tool", |ctx| {
            legacy_call(ctx, false)
        })
        .windows_ignore(STDIO_ON_WINDOWS),
        Scenario::new(
            "mcp::settings::env_and_cwd_reach_a_stdio_server",
            env_and_cwd,
        )
        .windows_ignore(STDIO_ON_WINDOWS),
        Scenario::new(
            "mcp::settings::headers_and_the_api_key_reach_an_http_server",
            headers,
        ),
        Scenario::new("mcp::settings::disabled_tools_are_withheld", disabled_tools)
            .windows_ignore(STDIO_ON_WINDOWS),
        Scenario::new(
            "mcp::a_server_whose_command_does_not_exist_leaves_the_turn_running",
            missing_command,
        )
        .windows_ignore(STDIO_ON_WINDOWS),
    ];
    for (case, calls, expected) in HTTP_CASES {
        let scenario = Scenario::new(format!("mcp::http::{case}"), move |ctx| {
            http_case(ctx, calls, expected)
        });
        scenarios.push(if *case == "a_call_past_its_timeout" {
            scenario.known_defect(
                "E2E-D42",
                "an MCP call past `tool_timeout_sec` reports `mcp_transport_failed`, not \
                 `mcp_timeout`",
            )
        } else {
            scenario
        });
    }
    for (case, arguments, code, servers) in COMMAND_CASES {
        scenarios.push(Scenario::new(format!("mcp::command::{case}"), move |ctx| {
            command_case(ctx, arguments, *code, servers)
        }));
    }
    scenarios
}

/// A stdio server `e2e` publishing `lookup`, whose output schema types an
/// `answer` and an optional `score`.
fn lookup_server(world: &World) -> Outcome<StdioMcp> {
    let script = Mcp::new()
        .tool(json!({
            "name": "lookup",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}},
                            "required": ["query"]},
            "outputSchema": {"type": "object",
                             "properties": {"answer": {"type": "string"}, "score": {"type": "number"}},
                             "required": ["answer"]},
        }))
        .react(
            "lookup",
            json!({"result": {
                "content": [{"type": "text", "text": "{\"answer\":\"forty-two\",\"score\":1}"}],
                "structuredContent": {"answer": "forty-two", "score": 1},
            }}),
        );
    world.mcp_stdio("e2e", &script)
}

fn add_servers(world: &World, toml: &str) -> Outcome {
    edit_config(world, |text| format!("{text}\n{toml}"))
}

fn typescript(code: &str) -> Reply {
    Reply::tool_call("run_typescript", json!({ "code": code }))
}

/// Runs `vibe -p` over `calls` then a final answer, and returns the content of
/// each tool message in call order.
fn run(world: &World, calls: Vec<Reply>, extra: &[&str]) -> Outcome<Vec<String>> {
    let count = calls.len();
    world.platform().replies(calls);
    world.platform().reply(Reply::text("done"));
    let mut arguments = vec!["-p", "go", "--output", "text"];
    arguments.extend(extra);
    let output = vibe(world, &arguments)?;
    exits(&output, 0)?;
    ensure(output.stdout == "done\n", || {
        format!("the turn reached no final answer: {:?}", output.stdout)
    })?;
    let last = turn_requests(world)?.pop().ok_or("no turn request")?;
    (1..=count)
        .map(|n| tool_message(&last, &format!("e2e{n:06}")))
        .collect()
}

/// `ok <returned value>`, or the error name a `run_typescript` failure carries.
pub fn outcome(message: &str) -> String {
    message
        .strip_prefix("run_typescript failed: ")
        .and_then(|error| serde_json::from_str::<Value>(error).ok())
        .and_then(|error| error.get("name").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| format!("ok {message}"))
}

fn structured_result(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let server = lookup_server(&world)?;
    add_servers(&world, &server.toml(""))?;
    let messages = run(
        &world,
        vec![typescript(
            "return await tools.mcp_e2e.lookup({query: 'x'});",
        )],
        &["--auto-approve"],
    )?;
    let found = outcome(&messages[0]);
    ensure(found == r#"ok {"answer":"forty-two","score":1}"#, || {
        format!("the call returned {found}")
    })?;
    let calls = server
        .messages()
        .into_iter()
        .filter(|m| m.get("method").and_then(Value::as_str) == Some("tools/call"))
        .count();
    ensure(calls == 1, || {
        format!("the server received {calls} tools/call")
    })
}

fn typed_declaration(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let server = lookup_server(&world)?;
    add_servers(&world, &server.toml(""))?;
    let messages = run(
        &world,
        vec![Reply::tool_call(
            "search_tool_functions",
            json!({"mode": "details", "functionNames": ["mcp_e2e.lookup"]}),
        )],
        &[],
    )?;
    let declared = messages[0]
        .split("type LookupReturned = ")
        .nth(1)
        .and_then(|rest| rest.split("};").next())
        .unwrap_or_default();
    let typed = ["answer: string;", "score?: number;"]
        .iter()
        .all(|field| declared.contains(field));
    ensure(typed, || {
        format!("the declaration does not type the return:\n{}", messages[0])
    })
}

fn legacy_call(ctx: &mut Ctx, auto: bool) -> Outcome {
    let world = ctx.world()?;
    let server = lookup_server(&world)?;
    let mut toml = server.toml("");
    if !auto {
        toml.push_str("[tools.e2e_lookup]\npermission = \"always\"\n");
    }
    add_servers(&world, &toml)?;
    let mut extra = vec!["--legacy-harness"];
    if auto {
        extra.push("--auto-approve");
    }
    let messages = run(
        &world,
        vec![Reply::tool_call("e2e_lookup", json!({"query": "x"}))],
        &extra,
    )?;
    ensure(messages[0].contains("forty-two"), || {
        format!("the MCP result did not reach the model: {}", messages[0])
    })
}

/// `(case, calls in order, outcome of each)`, against the HTTP server `web`
/// whose `tool_timeout_sec` is 1.
const HTTP_CASES: &[(&str, &[&str], &[&str])] = &[
    ("a_call_that_succeeds", &["ok"], &[r#"ok "web ok""#]),
    (
        "a_call_answered_with_a_json_rpc_error",
        &["rpc"],
        &["mcp_transport_failed"],
    ),
    (
        "a_call_whose_result_is_an_error",
        &["bad"],
        &["mcp_tool_error"],
    ),
    ("a_call_past_its_timeout", &["slow"], &["mcp_timeout"]),
    (
        "a_crash_then_a_restart_before_the_next_call",
        &["boom", "ok"],
        &["mcp_transport_failed", r#"ok "web ok""#],
    ),
];

fn web_server() -> Mcp {
    let tools = ["ok", "rpc", "bad", "slow", "boom"];
    tools
        .iter()
        .fold(Mcp::new(), |script, name| {
            script.tool(json!({"name": name, "inputSchema": {"type": "object"}}))
        })
        .react("ok", text_result("web ok"))
        .react(
            "rpc",
            json!({"error": {"code": -32050, "message": "e2e rpc failure"}}),
        )
        .react(
            "bad",
            json!({"result": {"content": [{"type": "text", "text": "e2e tool failure"}],
                              "isError": true}}),
        )
        .react("slow", json!({"delayMs": 4000, "result": {"content": []}}))
        .react("boom", json!({"crash": true}))
}

fn http_toml(world: &World, name: &str, extra: &str) -> String {
    format!(
        "[[mcp_servers]]\nname = \"{name}\"\ntransport = \"streamable-http\"\nurl = \"{}\"\n{extra}\n",
        world.platform().mcp_url(name)
    )
}

fn http_case(ctx: &mut Ctx, calls: &[&str], expected: &[&str]) -> Outcome {
    let world = ctx.world()?;
    world.platform().mcp("web", &web_server());
    add_servers(&world, &http_toml(&world, "web", "tool_timeout_sec = 1"))?;
    let replies = calls
        .iter()
        .map(|tool| typescript(&format!("return await tools.mcp_web.{tool}({{}});")))
        .collect();
    let messages = run(&world, replies, &["--auto-approve"])?;
    let found: Vec<String> = messages.iter().map(|m| outcome(m)).collect();
    ensure(found == expected, || {
        format!("the calls ended {found:?}, expected {expected:?}")
    })
}

fn env_and_cwd(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.write("srvdir/keep", "")?;
    let server = world.mcp_stdio(
        "e2e",
        &Mcp::new()
            .tool(json!({"name": "lookup", "inputSchema": {"type": "object"}}))
            .record_env("E2E_MCP_ENV"),
    )?;
    let cwd = world.root().join("srvdir");
    let extra = format!(
        "cwd = {}",
        Value::String(cwd.to_string_lossy().into_owned())
    );
    add_servers(
        &world,
        &server.toml_with_env(&extra, &[("E2E_MCP_ENV", "env-value")]),
    )?;
    run(&world, Vec::new(), &[])?;
    let starts = server.starts();
    let expected = json!({"cwd": cwd.to_string_lossy(), "env": {"E2E_MCP_ENV": "env-value"}});
    ensure(starts.first() == Some(&expected), || {
        format!("the server started as {starts:?}, expected {expected}")
    })
}

fn headers(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(vibe_e2e::Setup::default().env("E2E_MCP_TOKEN", "tok-123"))?;
    world.platform().mcp("web", &web_server());
    add_servers(
        &world,
        &http_toml(
            &world,
            "web",
            "headers = { \"X-E2E\" = \"hdr\" }\napi_key_env = \"E2E_MCP_TOKEN\"",
        ),
    )?;
    run(&world, Vec::new(), &[])?;
    let requests = world.platform().mcp_requests("web");
    ensure(!requests.is_empty(), || {
        "the server was never contacted".to_owned()
    })?;
    for request in &requests {
        let sent = (request.header("x-e2e"), request.header("authorization"));
        ensure(sent == (Some("hdr"), Some("Bearer tok-123")), || {
            format!(
                "{} {} carried the headers {sent:?}",
                request.method, request.path
            )
        })?;
    }
    Ok(())
}

fn disabled_tools(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let server = world.mcp_stdio(
        "e2e",
        &["lookup", "hidden"]
            .iter()
            .fold(Mcp::new(), |script, name| {
                script.tool(json!({"name": name, "inputSchema": {"type": "object"}}))
            }),
    )?;
    add_servers(&world, &server.toml("disabled_tools = [\"hidden\"]"))?;
    let messages = run(
        &world,
        vec![
            Reply::tool_call(
                "search_tool_functions",
                json!({"mode": "all_connector_capabilities", "connectors": ["mcp_e2e"]}),
            ),
            typescript("return await tools.mcp_e2e.hidden({});"),
        ],
        &["--auto-approve"],
    )?;
    ensure(
        messages[0].contains("mcp_e2e.lookup") && !messages[0].contains("mcp_e2e.hidden"),
        || format!("the capabilities listed: {}", messages[0]),
    )?;
    ensure(outcome(&messages[1]) == "TypeError", || {
        format!("calling the withheld tool ended {}", messages[1])
    })
}

fn missing_command(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let server = lookup_server(&world)?;
    add_servers(
        &world,
        &format!(
            "{}[[mcp_servers]]\nname = \"ghost\"\ntransport = \"stdio\"\n\
             command = \"/nonexistent/e2e-mcp-server\"\n",
            server.toml("")
        ),
    )?;
    let messages = run(
        &world,
        vec![
            typescript("return await tools.mcp_ghost.anything({});"),
            typescript("return await tools.mcp_e2e.lookup({query: 'x'});"),
        ],
        &["--auto-approve"],
    )?;
    let found: Vec<String> = messages.iter().map(|m| outcome(m)).collect();
    let expected = ["TypeError", r#"ok {"answer":"forty-two","score":1}"#];
    ensure(found == expected, || {
        format!("the calls ended {found:?}, expected {expected:?}")
    })
}

/// `(case, arguments, exit code, servers configured afterward)`, the servers
/// written as the JSON of `config.toml`'s `mcp_servers` with sorted keys. Each case starts
/// from a configuration holding the stdio server `kept`.
const COMMAND_CASES: &[(&str, &[&str], i32, &str)] = &[
    (
        "add_a_stdio_server",
        &[
            "add",
            "e2e",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
            "--arg",
            "x",
            "--env",
            "A=1",
        ],
        0,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"},{\"args\":[\"x\"],\"command\":\"/bin/true\",\"env\":{\"A\":\"1\"},\"name\":\"e2e\",\"transport\":\"stdio\"}]",
    ),
    (
        "add_an_http_server",
        &[
            "add",
            "web",
            "--url",
            "http://127.0.0.1:9/mcp",
            "--header",
            "X-A=1",
            "--api-key-env",
            "TOK",
        ],
        0,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"},{\"auth\":{\"api_key_env\":\"TOK\",\"headers\":{\"X-A\":\"1\"},\"type\":\"static\"},\"name\":\"web\",\"transport\":\"streamable-http\",\"url\":\"http://127.0.0.1:9/mcp\"}]",
    ),
    (
        "add_a_name_already_configured",
        &[
            "add",
            "kept",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
        ],
        2,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"}]",
    ),
    (
        "add_an_http_server_without_a_url",
        &["add", "web", "--no-login"],
        2,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"}]",
    ),
    (
        "add_a_stdio_server_without_a_command",
        &["add", "e2e", "--transport", "stdio"],
        2,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"}]",
    ),
    (
        "add_an_env_without_a_value",
        &[
            "add",
            "e2e",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
            "--env",
            "BAD",
        ],
        2,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"}]",
    ),
    (
        "add_a_plain_http_url_on_a_remote_host",
        &[
            "add",
            "web",
            "--url",
            "http://example.invalid/mcp",
            "--no-login",
        ],
        2,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"}]",
    ),
    ("remove_a_configured_server", &["remove", "kept"], 0, "[]"),
    (
        "remove_an_unknown_server",
        &["remove", "nosuch"],
        0,
        "[{\"command\":\"true\",\"name\":\"kept\",\"transport\":\"stdio\"}]",
    ),
];

fn command_case(ctx: &mut Ctx, arguments: &[&str], code: i32, expected: &str) -> Outcome {
    let world = ctx.world()?;
    add_servers(
        &world,
        "[[mcp_servers]]\nname = \"kept\"\ntransport = \"stdio\"\ncommand = \"true\"\n",
    )?;
    let output = world
        .command("vibe")?
        .arg("mcp")
        .args(arguments.iter().copied())
        .run()?;
    let text = std::fs::read_to_string(world.vibe_home().join("config.toml"))?;
    let parsed: toml::Value =
        toml::from_str(&text).map_err(|error| format!("config.toml does not parse: {error}"))?;
    let servers = canonical(&serde_json::to_value(parsed.get("mcp_servers"))?);
    let found = format!("exit={:?} servers={servers}", output.code());
    let expected = format!("exit={:?} servers={expected}", Some(code));
    ensure(found == expected, || {
        format!(
            "observed:\n  {found}\nexpected:\n  {expected}\nstderr: {}",
            output.stderr
        )
    })
}
