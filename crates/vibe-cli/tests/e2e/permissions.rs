//! US-015: every approval mode against every permission rule and call on the
//! default harness, one trial per cell, and the workspace boundaries.
//!
//! A cell is judged on one observation line: the exit code, whether the marker
//! file the call would create exists, how many smart-approve classifications
//! the platform answered, the approval record of each effect the streaming
//! output printed, and the outcome the tool message reports. Each expected
//! line was measured against the pinned reference.

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, Scenario, Setup, World, ensure};

use crate::cli::{chats, stream_entries, vibe};
use crate::tools::{tool_message, turn_requests};

#[derive(Clone, Copy)]
enum Mode {
    Ask,
    Auto,
    Smart,
}

impl Mode {
    const ALL: [Self; 3] = [Self::Ask, Self::Auto, Self::Smart];

    fn name(self) -> &'static str {
        match self {
            Self::Ask => "no_flag",
            Self::Auto => "auto_approve",
            Self::Smart => "smart_approve",
        }
    }

    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Ask => None,
            Self::Auto => Some("--auto-approve"),
            Self::Smart => Some("--smart-approve"),
        }
    }
}

#[derive(Clone, Copy)]
enum Rule {
    Default,
    Enabled,
    Disabled,
    Never,
    Always,
    Allowlist,
    Denylist,
}

impl Rule {
    const ALL: [Self; 7] = [
        Self::Default,
        Self::Enabled,
        Self::Disabled,
        Self::Never,
        Self::Always,
        Self::Allowlist,
        Self::Denylist,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Default => "no_rule",
            Self::Enabled => "enabled_tools_excluding_it",
            Self::Disabled => "disabled_tools_naming_it",
            Self::Never => "permission_never",
            Self::Always => "permission_always",
            Self::Allowlist => "allowlist_pattern",
            Self::Denylist => "denylist_pattern",
        }
    }
}

#[derive(Clone, Copy)]
enum Call {
    Bash,
    WriteFile,
    BashThroughTypescript,
}

impl Call {
    const ALL: [Self; 3] = [Self::Bash, Self::WriteFile, Self::BashThroughTypescript];

    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::WriteFile => "write_file",
            Self::BashThroughTypescript => "bash_through_run_typescript",
        }
    }

    /// The tool a rule names.
    fn target(self) -> &'static str {
        match self {
            Self::Bash | Self::BashThroughTypescript => "bash",
            Self::WriteFile => "write_file",
        }
    }

    /// The marker file the call creates when it runs.
    fn marker(self) -> &'static str {
        match self {
            Self::Bash => "marker-bash",
            Self::WriteFile => "marker-write.txt",
            Self::BashThroughTypescript => "marker-ts",
        }
    }

    fn reply(self) -> Reply {
        match self {
            Self::Bash => Reply::tool_call("bash", json!({"command": "touch marker-bash"})),
            Self::WriteFile => Reply::tool_call(
                "write_file",
                json!({"path": "marker-write.txt", "content": "x"}),
            ),
            Self::BashThroughTypescript => Reply::tool_call(
                "run_typescript",
                json!({"code": "return await tools.file_system.bash({command: 'touch marker-ts'});"}),
            ),
        }
    }

    /// A pattern the rule's list matches against this call: a command prefix
    /// for the shell, a path glob for a file tool.
    fn pattern(self) -> &'static str {
        match self {
            Self::Bash | Self::BashThroughTypescript => "touch",
            Self::WriteFile => "*marker*",
        }
    }
}

/// What a classification answers when smart approve asks: safe, low risk.
const SAFE: &str =
    r#"{"safe": true, "risk_level": "low", "user_authorization": "unknown", "reason": "e2e"}"#;

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for mode in Mode::ALL {
        for rule in Rule::ALL {
            for call in Call::ALL {
                let name = format!(
                    "permissions::matrix::{}::{}::{}",
                    mode.name(),
                    rule.name(),
                    call.name()
                );
                let expected = MATRIX
                    .iter()
                    .find(|(cell, _)| name.ends_with(cell))
                    .map_or("", |(_, line)| *line);
                let scenario = Scenario::new(name.clone(), move |ctx| {
                    cell(ctx, mode, rule, call, expected)
                });
                scenarios.push(tagged(&name, scenario));
            }
        }
    }
    scenarios.push(
        Scenario::new(
            "permissions::permission_never_beats_the_default_allowlist",
            never_beats_the_allowlist,
        )
        .known_defect(
            "E2E-D08",
            "`[tools.bash] permission = \"never\"` loses to the default allowlist",
        ),
    );
    scenarios.push(
        Scenario::new(
            "permissions::add_dir::reads_and_writes_inside_an_added_directory",
            added_directory,
        )
        .known_defect(
            "E2E-D10",
            "file tools ignore `--add-dir` on the default harness",
        ),
    );
    for mode in [Mode::Ask, Mode::Auto] {
        for place in Place::ALL {
            for (verb, write) in [("read", false), ("write", true)] {
                let cell = format!("{}::{}::{verb}", mode.name(), place.name());
                let expected = BOUNDARIES
                    .iter()
                    .find(|(name, _)| *name == cell)
                    .map_or("", |(_, line)| *line);
                let name = format!("permissions::boundary::{cell}");
                let scenario = Scenario::new(name.clone(), move |ctx| {
                    boundary(ctx, mode, place, write, expected)
                });
                scenarios.push(tagged(&name, scenario));
            }
        }
    }
    scenarios
}

/// Declares the open defect the trial `name` reproduces on the port, if any.
fn tagged(name: &str, scenario: Scenario) -> Scenario {
    let cell = name
        .strip_prefix("permissions::matrix::")
        .or_else(|| name.strip_prefix("permissions::"))
        .unwrap_or(name);
    match cell {
        "auto_approve::enabled_tools_excluding_it::bash"
        | "auto_approve::enabled_tools_excluding_it::write_file"
        | "auto_approve::enabled_tools_excluding_it::bash_through_run_typescript"
        | "auto_approve::disabled_tools_naming_it::bash"
        | "auto_approve::disabled_tools_naming_it::write_file"
        | "auto_approve::disabled_tools_naming_it::bash_through_run_typescript" => scenario
            .known_defect(
                "E2E-D06",
                "`--auto-approve` bypasses `--enabled-tools` and `--disabled-tools` on the \
                 default harness",
            ),
        "no_flag::disabled_tools_naming_it::write_file"
        | "no_flag::enabled_tools_excluding_it::write_file" => scenario.known_defect(
            "E2E-D07",
            "`write_file` left out by `--disabled-tools` or `--enabled-tools` still writes \
             without an approval flag",
        ),
        "no_flag::denylist_pattern::write_file" => scenario.known_defect(
            "E2E-D39",
            "a `[tools.write_file] denylist` pattern does not refuse a matching write",
        ),
        "boundary::no_flag::a_dotenv_file::write"
        | "boundary::no_flag::a_path_outside_the_work_tree::write"
        | "boundary::no_flag::a_symlink_leaving_the_work_tree::write" => scenario.known_defect(
            "E2E-D40",
            "`write_file` approves a sensitive or out-of-tree path by config instead of asking",
        ),
        _ => scenario,
    }
}

/// `(cell, observation)`, the cell written `mode::rule::call`.
const MATRIX: &[(&str, &str)] = &[
    (
        "no_flag::allowlist_pattern::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::allowlist_pattern::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::allowlist_pattern::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::denylist_pattern::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::denylist_pattern::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::denylist_pattern::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::disabled_tools_naming_it::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::disabled_tools_naming_it::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::disabled_tools_naming_it::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::enabled_tools_excluding_it::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::enabled_tools_excluding_it::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::enabled_tools_excluding_it::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::no_rule::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/ask/user] tool=tool_denied",
    ),
    (
        "no_flag::no_rule::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/ask/user] tool=tool_denied",
    ),
    (
        "no_flag::no_rule::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::permission_always::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::permission_always::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::permission_always::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/config] tool=ok",
    ),
    (
        "no_flag::permission_never::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::permission_never::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "no_flag::permission_never::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/config] tool=ok",
    ),
    (
        "auto_approve::allowlist_pattern::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::allowlist_pattern::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::allowlist_pattern::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::denylist_pattern::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::denylist_pattern::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::denylist_pattern::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::disabled_tools_naming_it::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "auto_approve::disabled_tools_naming_it::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "auto_approve::disabled_tools_naming_it::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "auto_approve::enabled_tools_excluding_it::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "auto_approve::enabled_tools_excluding_it::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "auto_approve::enabled_tools_excluding_it::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "auto_approve::no_rule::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::no_rule::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::no_rule::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::permission_always::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::permission_always::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::permission_always::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::permission_never::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::permission_never::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/bypass] tool=ok",
    ),
    (
        "auto_approve::permission_never::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok",
    ),
    (
        "smart_approve::allowlist_pattern::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::allowlist_pattern::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::allowlist_pattern::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::denylist_pattern::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::denylist_pattern::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::denylist_pattern::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::disabled_tools_naming_it::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::disabled_tools_naming_it::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::disabled_tools_naming_it::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::enabled_tools_excluding_it::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::enabled_tools_excluding_it::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::enabled_tools_excluding_it::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::no_rule::bash",
        "exit=0 marker=true classified=1 effects=[file_system.bash:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::no_rule::bash_through_run_typescript",
        "exit=0 marker=true classified=1 effects=[file_system.bash:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::no_rule::write_file",
        "exit=0 marker=true classified=1 effects=[file_system.write_file:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::permission_always::bash",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::permission_always::bash_through_run_typescript",
        "exit=0 marker=true classified=0 effects=[file_system.bash:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::permission_always::write_file",
        "exit=0 marker=true classified=0 effects=[file_system.write_file:completed:execute/always/smart] tool=ok",
    ),
    (
        "smart_approve::permission_never::bash",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::permission_never::bash_through_run_typescript",
        "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied",
    ),
    (
        "smart_approve::permission_never::write_file",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/never/never] tool=tool_denied",
    ),
];

fn cell(ctx: &mut Ctx, mode: Mode, rule: Rule, call: Call, expected: &str) -> Outcome {
    let target = call.target();
    let config = match rule {
        Rule::Never => format!("[tools.{target}]\npermission = \"never\"\n"),
        Rule::Always => format!("[tools.{target}]\npermission = \"always\"\n"),
        Rule::Allowlist => format!("[tools.{target}]\nallowlist = [\"{}\"]\n", call.pattern()),
        Rule::Denylist => format!("[tools.{target}]\ndenylist = [\"{}\"]\n", call.pattern()),
        Rule::Default | Rule::Enabled | Rule::Disabled => String::new(),
    };
    let world = ctx.world_with(Setup::default().config(config))?;
    script(&world, mode, call.reply());
    let mut arguments = vec!["-p", "go", "--output", "streaming"];
    arguments.extend(mode.flag());
    match rule {
        Rule::Enabled => arguments.extend(["--enabled-tools", "read_file,run_typescript"]),
        Rule::Disabled => arguments.extend(["--disabled-tools", target]),
        _ => {}
    }
    let found = observe(&world, &arguments, call.marker())?;
    ensure(found == expected, || {
        format!("observed:\n  {found}\nexpected:\n  {expected}")
    })
}

/// Queues the call, the final answer, and, under smart approve, a safe
/// verdict for every classification (a request offering no tools).
fn script(world: &World, mode: Mode, call: Reply) {
    world.platform().replies([call, Reply::text("done")]);
    if let Mode::Smart = mode {
        for _ in 0..3 {
            world.platform().utility(Reply::text(SAFE));
        }
    }
}

/// Runs `vibe` and describes what the cell left, in one line.
fn observe(world: &World, arguments: &[&str], marker: &str) -> Outcome<String> {
    let output = vibe(world, arguments)?;
    let entries = stream_entries(&output.stdout).unwrap_or_default();
    let effects: Vec<String> = entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("effect"))
        .map(effect)
        .collect();
    let classifications = chats(world)
        .iter()
        .filter(|entry| entry.queue == "utility")
        .count();
    let outcome = match turn_requests(world)?.last() {
        Some(last) => tool_message(last, "e2e000001")
            .map_or_else(|_| "none".to_owned(), |text| outcome(&text)),
        None => "none".to_owned(),
    };
    Ok(format!(
        "exit={} marker={} classified={classifications} effects=[{}] tool={outcome}",
        output
            .code()
            .map_or("signal".to_owned(), |code| code.to_string()),
        world.work().join(marker).exists(),
        effects.join(", ")
    ))
}

/// An effect entry as `title:status:decision/type/source`.
fn effect(entry: &Value) -> String {
    let title = entry.get("title").and_then(Value::as_str).unwrap_or("?");
    let state = entry.get("state").unwrap_or(&Value::Null);
    let status = state.get("status").and_then(Value::as_str).unwrap_or("?");
    let approval = find_approval(state).unwrap_or(&Value::Null);
    let field = |name: &str| approval.get(name).and_then(Value::as_str).unwrap_or("-");
    format!(
        "{title}:{status}:{}/{}/{}",
        field("decision"),
        field("approvalType"),
        field("approvalSource")
    )
}

/// The object carrying `approvalSource` nearest the top of `value`.
fn find_approval(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(map) if map.contains_key("approvalSource") => Some(value),
        Value::Object(map) => map.values().find_map(find_approval),
        _ => None,
    }
}

/// `ok`, or the name of the error the tool message reports.
fn outcome(text: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(text)
        && let Some(name) = parsed.pointer("/result/error/name").and_then(Value::as_str)
    {
        return name.to_owned();
    }
    if let Some((_, error)) = text.split_once("run_typescript failed: ")
        && let Ok(parsed) = serde_json::from_str::<Value>(error)
    {
        return parsed
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
    }
    "ok".to_owned()
}

/// `[tools.bash] permission = "never"` against a command the default
/// allowlist would let run.
fn never_beats_the_allowlist(ctx: &mut Ctx) -> Outcome {
    let world =
        ctx.world_with(Setup::default().config("[tools.bash]\npermission = \"never\"\n"))?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "echo e2e"})),
        Reply::text("done"),
    ]);
    let found = observe(&world, &["-p", "go", "--output", "streaming"], "absent")?;
    let expected = NEVER_ALLOWLISTED;
    ensure(found == expected, || {
        format!("observed:\n  {found}\nexpected:\n  {expected}")
    })
}

/// A file read then written inside a directory `--add-dir` grants, under an
/// agent that accepts edits.
fn added_directory(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.write("extra/inside.txt", "inside\n")?;
    let extra = world.root().join("extra");
    let path = |name: &str| extra.join(name).to_string_lossy().into_owned();
    world.platform().replies([
        Reply::tool_call("read_file", json!({"path": path("inside.txt")})),
        Reply::tool_call(
            "write_file",
            json!({"path": path("new.txt"), "content": "x"}),
        ),
        Reply::text("done"),
    ]);
    world.platform().exact();
    let extra_arg = extra.to_string_lossy().into_owned();
    let output = vibe(
        &world,
        &[
            "-p",
            "go",
            "--output",
            "text",
            "--agent",
            "accept-edits",
            "--add-dir",
            &extra_arg,
        ],
    )?;
    crate::cli::exits(&output, 0)?;
    let requests = turn_requests(&world)?;
    let read = tool_message(&requests[1], "e2e000001")?;
    let write = tool_message(&requests[2], "e2e000002")?;
    ensure(read.contains("inside") && outcome(&read) == "ok", || {
        format!("the read inside the added directory failed: {read}")
    })?;
    ensure(
        outcome(&write) == "ok" && extra.join("new.txt").exists(),
        || format!("the write inside the added directory failed: {write}"),
    )
}

#[derive(Clone, Copy)]
enum Place {
    Outside,
    Symlink,
    DotEnv,
}

impl Place {
    const ALL: [Self; 3] = [Self::Outside, Self::Symlink, Self::DotEnv];

    fn name(self) -> &'static str {
        match self {
            Self::Outside => "a_path_outside_the_work_tree",
            Self::Symlink => "a_symlink_leaving_the_work_tree",
            Self::DotEnv => "a_dotenv_file",
        }
    }

    /// The path the model names, and the file it resolves to.
    fn paths(self, world: &World) -> (String, std::path::PathBuf) {
        let outside = world.root().join("outside/secret.txt");
        match self {
            Self::Outside => (outside.to_string_lossy().into_owned(), outside),
            Self::Symlink => ("link/secret.txt".to_owned(), outside),
            Self::DotEnv => (".env".to_owned(), world.work().join(".env")),
        }
    }
}

/// Reads or writes `place` and describes what happened.
fn boundary(ctx: &mut Ctx, mode: Mode, place: Place, write: bool, expected: &str) -> Outcome {
    let world = ctx.world()?;
    world.write("outside/secret.txt", "SECRET=outside\n")?;
    world.write("work/.env", "SECRET=dotenv\n")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(world.root().join("outside"), world.work().join("link"))?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(world.root().join("outside"), world.work().join("link"))?;
    let (named, resolved) = place.paths(&world);
    let call = if write {
        Reply::tool_call(
            "write_file",
            json!({"path": named, "content": "e2e-written"}),
        )
    } else {
        Reply::tool_call("read_file", json!({"path": named}))
    };
    script(&world, mode, call);
    let mut arguments = vec!["-p", "go", "--output", "streaming"];
    arguments.extend(mode.flag());
    let mut found = observe(&world, &arguments, "absent")?;
    let written = std::fs::read_to_string(&resolved).unwrap_or_default() == "e2e-written";
    found.push_str(&format!(" written={written}"));
    if !write {
        let leaked = turn_requests(&world)?
            .last()
            .and_then(|last| tool_message(last, "e2e000001").ok())
            .is_some_and(|text| text.contains("SECRET="));
        found.push_str(&format!(" leaked={leaked}"));
    }
    ensure(found == expected, || {
        format!("observed:\n  {found}\nexpected:\n  {expected}")
    })
}

const NEVER_ALLOWLISTED: &str = "exit=0 marker=false classified=0 effects=[file_system.bash:failed:skip/never/never] tool=tool_denied";

/// `(mode::place::read|write, observation)`.
const BOUNDARIES: &[(&str, &str)] = &[
    (
        "no_flag::a_dotenv_file::read",
        "exit=0 marker=false classified=0 effects=[file_system.read_file:failed:skip/ask/user] tool=tool_denied written=false leaked=false",
    ),
    (
        "no_flag::a_dotenv_file::write",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/ask/user] tool=tool_denied written=false",
    ),
    (
        "no_flag::a_path_outside_the_work_tree::read",
        "exit=0 marker=false classified=0 effects=[file_system.read_file:failed:skip/ask/user] tool=tool_denied written=false leaked=false",
    ),
    (
        "no_flag::a_path_outside_the_work_tree::write",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/ask/user] tool=tool_denied written=false",
    ),
    (
        "no_flag::a_symlink_leaving_the_work_tree::read",
        "exit=0 marker=false classified=0 effects=[file_system.read_file:failed:skip/ask/user] tool=tool_denied written=false leaked=false",
    ),
    (
        "no_flag::a_symlink_leaving_the_work_tree::write",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:failed:skip/ask/user] tool=tool_denied written=false",
    ),
    (
        "auto_approve::a_dotenv_file::read",
        "exit=0 marker=false classified=0 effects=[file_system.read_file:completed:execute/always/bypass] tool=ok written=false leaked=true",
    ),
    (
        "auto_approve::a_dotenv_file::write",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok written=true",
    ),
    (
        "auto_approve::a_path_outside_the_work_tree::read",
        "exit=0 marker=false classified=0 effects=[file_system.read_file:completed:execute/always/bypass] tool=ok written=false leaked=true",
    ),
    (
        "auto_approve::a_path_outside_the_work_tree::write",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok written=true",
    ),
    (
        "auto_approve::a_symlink_leaving_the_work_tree::read",
        "exit=0 marker=false classified=0 effects=[file_system.read_file:completed:execute/always/bypass] tool=ok written=false leaked=true",
    ),
    (
        "auto_approve::a_symlink_leaving_the_work_tree::write",
        "exit=0 marker=false classified=0 effects=[file_system.write_file:completed:execute/always/bypass] tool=ok written=true",
    ),
];
