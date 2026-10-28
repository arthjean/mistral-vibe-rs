//! US-010: session continuation, agent selection, and configuration layering
//! under `vibe -p`.

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, Scenario, Setup, ensure};

use crate::cli::{
    body_str, chat_count, edit_config, exits, extra_model, last_user, session_id, stream_entries,
    system_text, vibe,
};

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new("sessions::continue_carries_the_latest_history", |ctx| {
            continuation(ctx, Resume::Continue)
        }),
        Scenario::new("sessions::resume_by_full_id_carries_its_history", |ctx| {
            continuation(ctx, Resume::Full)
        }),
        Scenario::new("sessions::resume_by_short_id_carries_its_history", |ctx| {
            continuation(ctx, Resume::Short)
        })
        .known_defect(
            "E2E-D24",
            "`--resume <first 8 characters>` is looked up as a whole id under `vibe -p` too",
        ),
        Scenario::new("sessions::resume_of_an_unknown_id_fails", unknown_session),
        Scenario::new("agents::an_unknown_agent_ends_the_run", unknown_agent).known_defect(
            "E2E-D13",
            "`--agent` naming an unknown agent is accepted and the turn runs",
        ),
        Scenario::new("config::an_untrusted_project_config_is_ignored", |ctx| {
            project_config(ctx, Trust::None)
        }),
        Scenario::new(
            "config::a_project_config_applies_under_the_trust_flag",
            |ctx| project_config(ctx, Trust::Flag),
        )
        .known_defect(
            "E2E-D04",
            "a project `.vibe/config.toml` is ignored under `vibe -p --trust`",
        ),
        Scenario::new(
            "config::a_project_config_applies_in_a_trusted_folder",
            |ctx| project_config(ctx, Trust::File),
        )
        .known_defect(
            "E2E-D04",
            "a project `.vibe/config.toml` is ignored in a folder `trusted_folders.toml` trusts",
        ),
        Scenario::new("config::a_numeric_active_model_is_refused", |ctx| {
            invalid_config(ctx, "active_model", |text| {
                text.replace("active_model = \"e2e-model\"", "active_model = 42")
            })
        })
        .known_defect(
            "E2E-D05",
            "configuration type validation is lost: a numeric active_model is accepted",
        ),
        Scenario::new("config::a_string_input_price_is_refused", |ctx| {
            invalid_config(ctx, "input_price", |text| {
                text.replacen("input_price = 0.0", "input_price = \"abc\"", 1)
            })
        })
        .known_defect(
            "E2E-D05",
            "configuration type validation is lost: a string input_price is accepted",
        ),
        Scenario::new("config::a_malformed_config_is_refused", malformed_config),
        Scenario::new("agents::plan_refuses_a_write_by_policy", |ctx| {
            never_write(ctx, Never::Plan)
        })
        .known_defect(
            "E2E-D09",
            "the plan profile's `never` is raised as an approval request instead of a policy",
        ),
        Scenario::new("agents::a_custom_agent_refuses_a_write_by_policy", |ctx| {
            never_write(ctx, Never::Custom)
        })
        .known_defect(
            "E2E-D09",
            "a custom profile's `never` is raised as an approval request instead of a policy",
        ),
        Scenario::new(
            "agents::the_default_agent_refuses_a_write_by_policy",
            |ctx| never_write(ctx, Never::Default),
        )
        .known_defect(
            "E2E-D09",
            "the `default_agent` profile's `never` is raised as an approval request",
        ),
        Scenario::new(
            "agents::custom_instructions_and_prompt_reach_the_system_message",
            custom_prompt,
        )
        .known_defect(
            "E2E-D16",
            "an agent's `instructions` and `system_prompt_id` are missing from the system \
             prompt on the default harness",
        ),
    ]
}

#[derive(Clone, Copy)]
enum Resume {
    Continue,
    Full,
    Short,
}

fn continuation(ctx: &mut Ctx, resume: Resume) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("first answer"), Reply::text("second answer")]);
    world.platform().exact();
    let first = vibe(&world, &["-p", "first question", "--output", "json"])?;
    exits(&first, 0)?;
    let id = session_id(&first.stdout)?;
    let mut arguments = vec!["-p", "second question", "--output", "text"];
    match resume {
        Resume::Continue => arguments.push("-c"),
        Resume::Full => arguments.extend(["--resume", &id]),
        Resume::Short => arguments.extend(["--resume", &id[..8]]),
    }
    let second = vibe(&world, &arguments)?;
    exits(&second, 0)?;
    ensure(second.stdout == "second answer\n", || second.stdout.clone())?;
    let requests = chat_count(&world, 2)?;
    let history: Vec<(String, String)> = requests[1]
        .messages()
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .map(|m| {
            (
                m.get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                crate::cli::content_text(m),
            )
        })
        .collect();
    let expected = [
        ("user", "first question"),
        ("assistant", "first answer"),
        ("user", "second question"),
    ]
    .map(|(role, text)| (role.to_owned(), text.to_owned()));
    ensure(history == expected, || {
        format!("the resumed turn carried the history {history:?}")
    })
}

fn unknown_session(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let output = vibe(
        &world,
        &["-p", "hello", "--resume", "deadbeef", "--output", "text"],
    )?;
    exits(&output, 1)?;
    chat_count(&world, 0)?;
    ensure(output.stderr.contains("deadbeef"), || {
        format!("the error does not name the id: {}", output.stderr)
    })
}

fn unknown_agent(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    // Answers the port's turn, so a run with the defect ends at once instead
    // of retrying an unscripted request.
    world.platform().reply(Reply::text("the turn ran"));
    let output = vibe(
        &world,
        &["-p", "hello", "--agent", "nosuch", "--output", "text"],
    )?;
    exits(&output, 1)?;
    chat_count(&world, 0)?;
    ensure(output.stderr.contains("nosuch"), || {
        format!("the error does not name the agent: {}", output.stderr)
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Trust {
    None,
    Flag,
    File,
}

/// A project config that selects another model, the plan agent (which
/// refuses `write_file`), and lets `bash` run without approval.
fn project_config(ctx: &mut Ctx, trust: Trust) -> Outcome {
    let world = ctx.world_with(Setup::default().config(extra_model("proj", "e2e-proj-v1")))?;
    world.write(
        "work/.vibe/config.toml",
        "active_model = \"proj\"\ndefault_agent = \"plan\"\n\n[tools.bash]\npermission = \"always\"\n",
    )?;
    let work = world.work().to_string_lossy().into_owned();
    if trust == Trust::File {
        world.write(
            "vibe-home/trusted_folders.toml",
            format!("trusted = [\"{work}\"]\nuntrusted = []\n"),
        )?;
    }
    world.platform().replies([
        Reply::tool_call("write_file", json!({"path": "planned.txt", "content": "x"})),
        Reply::tool_call("bash", json!({"command": "touch bash-ran"})),
        Reply::text("project done"),
    ]);
    world.platform().exact();
    let mut arguments = vec!["-p", "go", "--output", "text"];
    if trust == Trust::Flag {
        arguments.push("--trust");
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 3)?;
    let applied = trust != Trust::None;
    let model = if applied {
        "e2e-proj-v1"
    } else {
        "e2e-model-v1"
    };
    for request in &requests {
        ensure(body_str(request, "model") == Some(model), || {
            format!("a turn named {:?}, not {model}", body_str(request, "model"))
        })?;
    }
    let wrote = world.work().join("planned.txt").exists();
    ensure(wrote != applied, || {
        format!("write_file wrote: {wrote}, with the project's plan agent applied: {applied}")
    })?;
    let ran = world.work().join("bash-ran").exists();
    ensure(ran == applied, || {
        format!("bash ran: {ran}, with the project's bash permission applied: {applied}")
    })?;
    if trust == Trust::Flag {
        let trusted = std::fs::read_to_string(world.vibe_home().join("trusted_folders.toml"))
            .unwrap_or_default();
        ensure(!trusted.contains(&work), || {
            format!("--trust persisted the folder: {trusted}")
        })?;
    }
    Ok(())
}

/// A config the reference refuses before any request, naming `key` in its
/// report.
fn invalid_config(ctx: &mut Ctx, key: &str, edit: fn(String) -> String) -> Outcome {
    let world = ctx.world()?;
    edit_config(&world, edit)?;
    world.platform().reply(Reply::text("the turn ran"));
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    ensure(!output.success(), || "the run succeeded".to_owned())?;
    chat_count(&world, 0)?;
    // The reference reports configuration errors on stdout.
    ensure(
        output.stdout.contains(key) || output.stderr.contains(key),
        || {
            format!(
                "the report does not name {key}:\nstdout:\n{}\nstderr:\n{}",
                output.stdout, output.stderr
            )
        },
    )
}

fn malformed_config(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    edit_config(&world, |text| format!("{text}\n[unclosed\n"))?;
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    ensure(!output.success(), || "the run succeeded".to_owned())?;
    chat_count(&world, 0).map(drop)
}

#[derive(Clone, Copy)]
enum Never {
    Plan,
    Custom,
    Default,
}

const GUARDED: &str = "display_name = \"Guarded\"\ndescription = \"Never writes files\"\n\n\
                       [tools.write_file]\npermission = \"never\"\n";

fn never_write(ctx: &mut Ctx, never: Never) -> Outcome {
    let setup = match never {
        Never::Default => Setup::default().config("default_agent = \"guarded\""),
        Never::Plan | Never::Custom => Setup::default(),
    };
    let world = ctx.world_with(setup)?;
    world.write("vibe-home/agents/guarded.toml", GUARDED)?;
    world.platform().replies([
        Reply::tool_call("write_file", json!({"path": "refused.txt", "content": "x"})),
        Reply::text("after the refusal"),
    ]);
    world.platform().exact();
    let mut arguments = vec!["-p", "go", "--output", "streaming"];
    match never {
        Never::Plan => arguments.extend(["--agent", "plan"]),
        Never::Custom => arguments.extend(["--agent", "guarded"]),
        Never::Default => {}
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, 0)?;
    let entries = stream_entries(&output.stdout)?;
    let callbacks: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("callback"))
        .collect();
    ensure(callbacks.is_empty(), || {
        format!("the refusal raised an approval request: {callbacks:?}")
    })?;
    ensure(output.stdout.contains("after the refusal"), || {
        format!("the turn reached no final answer: {}", output.stdout)
    })?;
    chat_count(&world, 2)?;
    ensure(!world.work().join("refused.txt").exists(), || {
        "the refused write created the file".to_owned()
    })
}

fn custom_prompt(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.write(
        "vibe-home/agents/marked.toml",
        "display_name = \"Marked\"\ndescription = \"Carries markers\"\n\
         instructions = \"Always honor E2E-INSTRUCTIONS-MARK.\"\nsystem_prompt_id = \"marked\"\n",
    )?;
    world.write(
        "vibe-home/prompts/marked.md",
        "You are a test agent carrying E2E-PROMPT-MARK.\n",
    )?;
    world.platform().reply(Reply::text("marked"));
    let output = vibe(
        &world,
        &["-p", "go", "--agent", "marked", "--output", "text"],
    )?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    let system = system_text(&requests[0]);
    for marker in ["E2E-INSTRUCTIONS-MARK", "E2E-PROMPT-MARK"] {
        ensure(system.contains(marker), || {
            format!("the system message misses {marker}")
        })?;
    }
    ensure(last_user(&requests[0]) == "go", || {
        "the turn lost the prompt".to_owned()
    })
}
