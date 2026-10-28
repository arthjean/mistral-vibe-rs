//! US-011: stream faults, HTTP statuses, retries, and missing credentials
//! under `vibe -p`, against counts and exit codes measured on the reference.

use vibe_e2e::{Ctx, Outcome, Reply, Scenario, Setup, ensure};

use crate::cli::{chat_count, exits, vibe};

const RECOVERED: &str = "recovered answer";

/// A stream that ends before the model said it was done. The reference
/// retries it on the Unified Harness and fails the run on the legacy one.
struct Fault {
    name: &'static str,
    reply: fn() -> Reply,
}

const FAULTS: [Fault; 4] = [
    Fault {
        name: "a_stream_cut_mid_body",
        reply: || Reply::text("partial").drop_after(2),
    },
    Fault {
        name: "a_stream_without_finish_reason_or_done",
        reply: || Reply::text("partial").no_finish().no_done(),
    },
    Fault {
        name: "an_sse_error_event",
        reply: || {
            Reply::sse([r#"data: {"error": {"message": "overloaded", "type": "server_error"}}"#])
        },
    },
    Fault {
        name: "a_done_event_without_finish_reason",
        reply: || Reply::text("partial").no_finish(),
    },
];

/// Statuses the reference retries once and then answers through.
const RETRIED: [u16; 8] = [408, 409, 425, 429, 500, 502, 503, 504];

/// Statuses the reference reports after its first request.
const PERMANENT: [u16; 4] = [400, 401, 404, 422];

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for fault in FAULTS {
        scenarios.push(
            Scenario::new(
                format!("faults::unified::{}_is_retried", fault.name),
                move |ctx| retried_fault(ctx, fault.reply),
            )
            .known_defect(
                "E2E-D14",
                "an incomplete SSE stream is not retried on the default harness",
            ),
        );
        scenarios.push(Scenario::new(
            format!("faults::legacy::{}_ends_the_run", fault.name),
            move |ctx| legacy_fault(ctx, fault.reply),
        ));
    }
    scenarios.push(Scenario::new(
        "faults::a_stream_missing_only_its_done_event_is_accepted",
        missing_done,
    ));
    for status in RETRIED {
        scenarios.push(Scenario::new(
            format!("faults::status::{status}_is_retried_once"),
            move |ctx| retried_status(ctx, status),
        ));
    }
    for status in PERMANENT {
        scenarios.push(Scenario::new(
            format!("faults::status::{status}_ends_the_run_at_once"),
            move |ctx| permanent_status(ctx, status),
        ));
    }
    scenarios.extend([
        Scenario::new("faults::retry_after_delays_the_retry", retry_after),
        Scenario::new("faults::exhausted_retries_end_the_run::unified", |ctx| {
            exhausted(ctx, false)
        }),
        Scenario::new("faults::exhausted_retries_end_the_run::legacy", |ctx| {
            exhausted(ctx, true)
        }),
        Scenario::new("faults::a_malformed_sse_line_ends_the_run", malformed),
        Scenario::new("faults::a_length_finish_reason_is_an_answer", length),
    ]);
    for (label, setup) in [("generic", false), ("mistral", true)] {
        scenarios.push(Scenario::new(
            format!("credentials::{label}::no_key_anywhere_ends_the_run"),
            move |ctx| no_key(ctx, setup),
        ));
        scenarios.push(Scenario::new(
            format!("credentials::{label}::a_key_in_the_vibe_home_env_file_is_used"),
            move |ctx| dotenv_key(ctx, setup),
        ));
    }
    scenarios
}

fn retried_fault(ctx: &mut Ctx, fault: fn() -> Reply) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([fault(), Reply::text(RECOVERED)]);
    world.platform().exact();
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 0)?;
    ensure(output.stdout == format!("{RECOVERED}\n"), || {
        output.stdout.clone()
    })?;
    let requests = chat_count(&world, 2)?;
    ensure(requests[0].body == requests[1].body, || {
        "the retry did not repeat the first request".to_owned()
    })
}

fn legacy_fault(ctx: &mut Ctx, fault: fn() -> Reply) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([fault(), Reply::text(RECOVERED)]);
    let output = vibe(
        &world,
        &["-p", "go", "--output", "text", "--legacy-harness"],
    )?;
    exits(&output, 1)?;
    ensure(!output.stdout.contains(RECOVERED), || output.stdout.clone())?;
    chat_count(&world, 1).map(drop)
}

fn missing_done(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .reply(Reply::text("complete answer").cut_before_last());
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 0)?;
    ensure(output.stdout == "complete answer\n", || {
        output.stdout.clone()
    })?;
    chat_count(&world, 1).map(drop)
}

fn status_reply(status: u16) -> Reply {
    Reply::status(status).body(r#"{"message":"scripted failure"}"#)
}

fn retried_status(ctx: &mut Ctx, status: u16) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([status_reply(status), Reply::text(RECOVERED)]);
    world.platform().exact();
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 0)?;
    ensure(output.stdout == format!("{RECOVERED}\n"), || {
        output.stdout.clone()
    })?;
    chat_count(&world, 2).map(drop)
}

fn permanent_status(ctx: &mut Ctx, status: u16) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([status_reply(status), Reply::text(RECOVERED)]);
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 1)?;
    chat_count(&world, 1).map(drop)
}

fn retry_after(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        status_reply(429).header("Retry-After", "1"),
        Reply::text(RECOVERED),
    ]);
    world.platform().exact();
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 2)?;
    let gap = requests[1].at_ms.saturating_sub(requests[0].at_ms);
    ensure(gap >= 1000, || {
        format!("the retry came {gap} ms after the 429")
    })
}

/// With a 2.5 s retry budget, the 0.5, 1, and 2 s backoffs both
/// implementations use leave room for four attempts.
fn exhausted(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world_with(Setup::default().config("api_retry_max_elapsed_time = 2.5"))?;
    world.platform().replies((0..8).map(|_| status_reply(503)));
    let mut arguments = vec!["-p", "go", "--output", "text"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, 1)?;
    chat_count(&world, 4).map(drop)
}

fn malformed(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::text("malformed answer").malformed(),
        Reply::text(RECOVERED),
    ]);
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 1)?;
    ensure(output.stdout.is_empty(), || output.stdout.clone())?;
    chat_count(&world, 1).map(drop)
}

fn length(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .reply(Reply::text("truncated answer").finish("length"));
    world.platform().exact();
    let output = vibe(&world, &["-p", "go", "--output", "text"])?;
    exits(&output, 0)?;
    ensure(output.stdout == "truncated answer\n", || {
        output.stdout.clone()
    })?;
    chat_count(&world, 1).map(drop)
}

fn key_setup(mistral: bool) -> (Setup, &'static str) {
    if mistral {
        (Setup::mistral(), "MISTRAL_API_KEY")
    } else {
        (Setup::default(), vibe_e2e::PROVIDER_KEY_VARIABLE)
    }
}

fn no_key(ctx: &mut Ctx, mistral: bool) -> Outcome {
    let (setup, variable) = key_setup(mistral);
    let world = ctx.world_with(setup)?;
    world.platform().reply(Reply::text(RECOVERED));
    let output = world
        .command("vibe")?
        .args(["-p", "go", "--output", "text"])
        .env_remove(variable)
        .run()?;
    exits(&output, 1)?;
    chat_count(&world, 0)?;
    ensure(output.stderr.contains(variable), || {
        format!("the error does not name {variable}: {}", output.stderr)
    })
}

fn dotenv_key(ctx: &mut Ctx, mistral: bool) -> Outcome {
    let (setup, variable) = key_setup(mistral);
    let world = ctx.world_with(setup)?;
    world.write("vibe-home/.env", format!("{variable}=dotenv-key\n"))?;
    world.platform().reply(Reply::text(RECOVERED));
    world.platform().exact();
    let output = world
        .command("vibe")?
        .args(["-p", "go", "--output", "text"])
        .env_remove(variable)
        .run()?;
    exits(&output, 0)?;
    let requests = chat_count(&world, 1)?;
    let authorization = requests[0].header("authorization");
    ensure(authorization == Some("Bearer dotenv-key"), || {
        format!("the turn carried the authorization {authorization:?}")
    })
}
