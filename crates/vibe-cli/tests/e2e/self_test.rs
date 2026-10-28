//! The harness proving itself against the real binary: the jail, the world's
//! isolation, the egress verdict, the service routes, and the subject switch.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use vibe_e2e::{Ctx, Outcome, Reply, RouteReply, Scenario, Setup, Subject, ensure};

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new(
            "self_test::jail::trials_run_inside_the_network_namespace",
            jailed,
        )
        .needs_jail(),
        Scenario::new(
            "self_test::jail::a_public_connect_fails_within_a_second",
            public_connect,
        )
        .needs_jail(),
        Scenario::new(
            "self_test::jail::a_remote_provider_fails_the_trial_as_egress",
            remote_provider,
        ),
        Scenario::new(
            "self_test::world::credentials_color_and_the_user_config_stay_outside",
            isolation,
        ),
        Scenario::new("self_test::world::environment_probe", environment_probe),
        Scenario::new(
            "self_test::world::a_closed_world_leaves_nothing_behind",
            closed_world,
        ),
        Scenario::new(
            "self_test::platform::telemetry_reaches_the_datalake_route",
            telemetry,
        ),
        Scenario::new(
            "self_test::platform::neutral_routes_leave_a_turn_as_absent_routes_do",
            neutral_routes,
        ),
        Scenario::new(
            "self_test::subject::the_subject_switch_changes_the_binary",
            subject_switch,
        ),
    ]
}

fn jailed(_ctx: &mut Ctx) -> Outcome {
    ensure(
        std::env::var(vibe_e2e::jail::JAILED_VARIABLE).as_deref() == Ok("1"),
        || "the trial did not run under the jail wrapper".to_owned(),
    )?;
    #[cfg(target_os = "linux")]
    {
        let foreign = vibe_e2e::jail::foreign_interfaces();
        ensure(foreign.is_empty(), || {
            format!("interfaces besides loopback are visible: {foreign:?}")
        })?;
    }
    Ok(())
}

fn public_connect(_ctx: &mut Ctx) -> Outcome {
    let address: SocketAddr = "1.1.1.1:443".parse().map_err(|error| format!("{error}"))?;
    let started = Instant::now();
    let outcome = TcpStream::connect_timeout(&address, Duration::from_secs(5));
    let took = started.elapsed();
    ensure(outcome.is_err(), || {
        "a public address was reachable".to_owned()
    })?;
    ensure(took < Duration::from_secs(1), || {
        format!("the public connect took {took:?} to fail")
    })
}

fn remote_provider(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::default().api_base("https://api.e2e-egress.example/v1"))?;
    let child = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"])
        .spawn()?;
    world.await_egress(Duration::from_secs(30))?;
    drop(child);
    let failures = ctx.close(world);
    ensure(
        failures
            .iter()
            .any(|f| f == "egress to api.e2e-egress.example:443 recorded by the sentinel"),
        || format!("the verdict did not name the remote provider: {failures:?}"),
    )
}

/// Runs [`environment_probe`] in a child of this binary whose own environment
/// carries a key, a color switch, and a home holding a user config that would
/// redirect the turn if any of them reached a world child.
fn isolation(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let outside = world.root().join("outside-home");
    world.write(
        "outside-home/.vibe/config.toml",
        format!(
            "active_model = \"leaked\"\n\n[[providers]]\nname = \"leaked\"\napi_base = \"{}/leaked/v1\"\n\
             api_key_env_var = \"MISTRAL_API_KEY\"\nbackend = \"generic\"\n\n[[models]]\n\
             name = \"leaked-model\"\nprovider = \"leaked\"\nalias = \"leaked\"\n",
            world.platform().base()
        ),
    )?;
    let mut probe = world
        .program(std::env::current_exe()?)
        .args(["--exact", "self_test::world::environment_probe"])
        .env("MISTRAL_API_KEY", "leaked-key")
        .env("FORCE_COLOR", "1")
        .env("HOME", outside.to_string_lossy())
        .env_remove("VIBE_HOME")
        .deadline(Duration::from_secs(120));
    for name in [
        vibe_e2e::jail::JAILED_VARIABLE,
        vibe_e2e::jail::UNJAILED_VARIABLE,
        "VIBE_E2E_SUBJECT",
        "VIBE_REFERENCE",
        "VIBE_PARITY_PYTHON",
        vibe_e2e::TIME_SCALE_VARIABLE,
    ] {
        if let Ok(value) = std::env::var(name) {
            probe = probe.env(name, value);
        }
    }
    let output = probe.run()?;
    ensure(
        output.success() && output.stdout.contains("1 passed"),
        || {
            format!(
                "the environment probe failed:\n{}\n{}",
                output.stdout, output.stderr
            )
        },
    )?;
    ensure(world.platform().journal().is_empty(), || {
        "the outside user config reached the platform".to_owned()
    })
}

fn environment_probe(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let env = world.program("env").run()?;
    for leaked in ["MISTRAL_API_KEY", "FORCE_COLOR", "outside-home"] {
        ensure(!env.stdout.contains(leaked), || {
            format!("{leaked} reached a world child:\n{}", env.stdout)
        })?;
    }
    world.platform().reply(Reply::text("isolated answer"));
    let output = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"])
        .run()?;
    ensure(output.success(), || {
        format!("vibe -p failed: {}", output.stderr)
    })?;
    ensure(output.stdout.contains("isolated answer"), || {
        output.stdout.clone()
    })?;
    ensure(!output.stdout.contains('\u{1b}'), || {
        format!("the output carries ANSI escapes: {:?}", output.stdout)
    })?;
    let requests = world.platform().chat_requests();
    let model = requests
        .first()
        .and_then(|r| r.body.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_owned();
    ensure(model == world.provider().model_name(), || {
        format!("the turn named the model {model:?}")
    })
}

fn scratchpads() -> Vec<String> {
    std::fs::read_dir(std::env::temp_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("vibe-scratchpad-"))
                .collect()
        })
        .unwrap_or_default()
}

fn closed_world(ctx: &mut Ctx) -> Outcome {
    let before = scratchpads();
    let world = ctx.world()?;
    world.platform().reply(Reply::text("done"));
    let output = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"])
        .run()?;
    ensure(output.success(), || {
        format!("vibe -p failed: {}", output.stderr)
    })?;
    let root = world.root().to_path_buf();
    let failures = ctx.close(world);
    ensure(failures.is_empty(), || format!("{failures:?}"))?;
    ensure(!root.exists(), || {
        format!("{} was left behind", root.display())
    })?;
    let left: Vec<String> = scratchpads()
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    ensure(left.is_empty(), || {
        format!("scratchpads left in the system temporary directory: {left:?}")
    })
}

fn telemetry(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world_with(Setup::mistral())?;
    world.platform().reply(Reply::text("measured"));
    let output = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"])
        .run()?;
    ensure(output.success(), || {
        format!("vibe -p failed: {}", output.stderr)
    })?;
    let events = world
        .platform()
        .journal()
        .into_iter()
        .filter(|entry| entry.queue == "datalake")
        .count();
    ensure(events > 0, || {
        "no telemetry event reached the datalake route".to_owned()
    })
}

/// The service routes a Mistral-backend turn contacts at startup.
const SERVICE_ROUTES: [&str; 6] = [
    "managed_config",
    "whoami",
    "identity",
    "experiments",
    "connectors_bootstrap",
    "datalake",
];

fn neutral_routes(ctx: &mut Ctx) -> Outcome {
    let mut run = |absent: bool| -> Outcome<(Option<i32>, String, String)> {
        let world = ctx.world_with(Setup::mistral())?;
        if absent {
            for route in SERVICE_ROUTES {
                world.platform().route(route, RouteReply::not_found());
            }
        }
        world.platform().reply(Reply::text("same answer"));
        let output = world
            .command("vibe")?
            .args(["-p", "hello", "--output", "text"])
            .run()?;
        Ok((output.code(), output.stdout, output.stderr))
    };
    let neutral = run(false)?;
    let absent = run(true)?;
    ensure(neutral == absent, || {
        format!("neutral routes changed the run: {neutral:?} against {absent:?}")
    })
}

/// The user agents of every request one Mistral-backend turn makes. Both
/// subjects stamp their telemetry with their own version; the reference's
/// chat request carries its SDK's default instead.
fn user_agents(ctx: &mut Ctx, subject: Subject) -> Outcome<Vec<String>> {
    let world = ctx.world_with(Setup::mistral())?;
    world.platform().reply(Reply::text("hi"));
    let output = world
        .command_for(subject, "vibe")?
        .args(["-p", "hello", "--output", "text"])
        .run()?;
    ensure(output.success(), || {
        format!("{subject} vibe -p failed: {}", output.stderr)
    })?;
    let mut agents: Vec<String> = world
        .platform()
        .journal()
        .iter()
        .filter_map(|request| request.header("user-agent").map(str::to_owned))
        .collect();
    agents.sort();
    agents.dedup();
    Ok(agents)
}

fn subject_switch(ctx: &mut Ctx) -> Outcome {
    let stamped = |agents: &[String], version: &str| {
        agents
            .iter()
            .any(|agent| agent.contains(&format!("Mistral-Vibe/{version}")))
    };
    let current = user_agents(ctx, ctx.subject())?;
    match ctx.subject() {
        Subject::Port => ensure(stamped(&current, env!("CARGO_PKG_VERSION")), || {
            format!("no port request carried the workspace version: {current:?}")
        }),
        Subject::Reference => {
            let port = user_agents(ctx, Subject::Port)?;
            ensure(
                stamped(&current, vibe_core::parity::REFERENCE_VERSION),
                || format!("no reference request carried its package version: {current:?}"),
            )?;
            ensure(port != current, || {
                format!("both subjects sent the same user agents: {current:?}")
            })
        }
    }
}
