//! US-032: the log file `vibe -p` writes, and the spans it exports to an OTLP
//! collector on the platform's trace route.
//!
//! The expectations were measured against the pinned reference on 2026-10-29
//! (`vibe/observability/logging.py`, `vibe/core/tracing.py`).

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Outcome, Reply, RouteReply, Scenario, World, ensure};

use crate::cli::{edit_config, exits, vibe};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios: Vec<Scenario> = LEVELS
        .iter()
        .map(|&(case, variable, value)| {
            Scenario::new(
                format!("observability::log::{case}::records_land_in_an_owner_only_file"),
                move |ctx| log_file(ctx, variable, value),
            )
            .known_defect(
                "E2E-D30",
                "`vibe.log` stays empty, created with modes 644 and 755 instead of 600 and 700",
            )
        })
        .collect();
    scenarios.extend([
        Scenario::new(
            "observability::log::an_invalid_log_max_bytes_leaves_the_run_without_a_log",
            invalid_ceiling,
        )
        .port_only(
            "the reference exits 1 on an invalid `LOG_MAX_BYTES`; this port keeps the run, the \
             divergence `docs/parity.md` ledgers",
        ),
        Scenario::new(
            "observability::log::a_small_log_max_bytes_bounds_the_file",
            small_ceiling,
        )
        .port_only(
            "the reference lets the file grow past `LOG_MAX_BYTES`; this port keeps the ceiling, \
             the divergence `docs/parity.md` ledgers",
        )
        .known_defect(
            "E2E-D30",
            "`vibe.log` stays empty, so no ceiling is reached",
        ),
        Scenario::new(
            "observability::spans::legacy::a_tool_turn_exports_the_reference_spans",
            legacy_spans,
        )
        .known_defect(
            "E2E-D56",
            "legacy chat spans name the model alias and carry no temperature, response id, or \
             status code",
        ),
        Scenario::new(
            "observability::spans::unified::a_tool_turn_exports_no_span",
            unified_spans,
        ),
        Scenario::new(
            "observability::spans::legacy::a_failing_collector_leaves_the_turn_alone",
            |ctx| failing_collector(ctx, true),
        ),
        Scenario::new(
            "observability::spans::unified::a_failing_collector_leaves_the_turn_alone",
            |ctx| failing_collector(ctx, false),
        ),
    ]);
    scenarios
}

/// `(case, variable, value)`: each selects a level that writes records.
const LEVELS: [(&str, &str, &str); 3] = [
    ("info", "LOG_LEVEL", "INFO"),
    ("debug", "LOG_LEVEL", "DEBUG"),
    ("debug_mode", "DEBUG_MODE", "true"),
];

fn log_path(world: &World) -> std::path::PathBuf {
    world.vibe_home().join("logs").join("vibe.log")
}

/// Runs one answered `vibe -p` with `env`.
fn logged_run(world: &World, env: &[(&str, &str)]) -> Outcome<vibe_e2e::Output> {
    world.platform().reply(Reply::text("logged"));
    let mut command = world
        .command("vibe")?
        .args(["-p", "hello", "--output", "text"]);
    for &(name, value) in env {
        command = command.env(name, value);
    }
    command.run()
}

/// Whether `line` is a record as both implementations format one:
/// `<RFC 3339 UTC timestamp> <ppid> <pid> <LEVEL> <message>`.
fn is_record(line: &str) -> bool {
    let mut parts = line.splitn(5, ' ');
    let (Some(timestamp), Some(ppid), Some(pid), Some(level)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    timestamp.len() >= 20
        && timestamp.as_bytes().get(10) == Some(&b'T')
        && timestamp.ends_with("+00:00")
        && ppid.parse::<u32>().is_ok()
        && pid.parse::<u32>().is_ok()
        && ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"].contains(&level)
}

fn log_file(ctx: &mut Ctx, variable: &str, value: &str) -> Outcome {
    let world = ctx.world()?;
    exits(&logged_run(&world, &[(variable, value)])?, 0)?;
    let path = log_path(&world);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    ensure(!text.is_empty(), || format!("{} is empty", path.display()))?;
    let malformed: Vec<&str> = text.lines().filter(|line| !is_record(line)).collect();
    ensure(malformed.is_empty(), || {
        format!("lines that are not log records: {malformed:?}")
    })?;
    owner_only(&path, 0o600)?;
    owner_only(path.parent().ok_or("the log has no directory")?, 0o700)
}

#[cfg(unix)]
fn owner_only(path: &std::path::Path, expected: u32) -> Outcome {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    ensure(mode == expected, || {
        format!("{} has mode {mode:o}, not {expected:o}", path.display())
    })
}

#[cfg(not(unix))]
fn owner_only(_: &std::path::Path, _: u32) -> Outcome {
    Ok(())
}

/// The reference lets `int()` raise and exits 1 with a traceback; this port
/// reports that logging is unavailable and answers.
fn invalid_ceiling(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let output = logged_run(&world, &[("LOG_MAX_BYTES", "abc"), ("LOG_LEVEL", "DEBUG")])?;
    exits(&output, 0)?;
    ensure(output.stdout == "logged\n", || output.stdout.clone())?;
    ensure(
        output.stderr.contains("LOG_MAX_BYTES") && output.stderr.contains("abc"),
        || format!("the warning does not name the variable: {}", output.stderr),
    )?;
    ensure(!log_path(&world).exists(), || {
        "a log file was written with an invalid ceiling".to_owned()
    })
}

/// The reference wrote 1,042 bytes under a 200-byte ceiling, since a
/// rotation without a backup reopens the same file; this port starts the file
/// over instead.
fn small_ceiling(ctx: &mut Ctx) -> Outcome {
    const CEILING: u64 = 200;
    let world = ctx.world()?;
    let output = logged_run(
        &world,
        &[
            ("LOG_MAX_BYTES", &CEILING.to_string()),
            ("LOG_LEVEL", "DEBUG"),
        ],
    )?;
    exits(&output, 0)?;
    let size = std::fs::metadata(log_path(&world))?.len();
    ensure(size > 0 && size <= CEILING, || {
        format!("vibe.log holds {size} bytes under a {CEILING}-byte ceiling")
    })
}

/// Turns tracing on with the platform as the collector.
fn traced(ctx: &mut Ctx) -> Outcome<World> {
    let world = ctx.world()?;
    let base = world.platform().base().to_owned();
    edit_config(&world, |text| {
        text.replacen(
            "enable_update_checks = false",
            &format!(
                "enable_update_checks = false\nenable_otel = true\notel_endpoint = \"{base}\""
            ),
            1,
        )
    })?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "true"})),
        Reply::text("traced"),
    ]);
    world.platform().exact();
    Ok(world)
}

fn tool_turn(world: &World, legacy: bool) -> Outcome {
    let mut arguments = vec!["-p", "hello", "--output", "text", "--auto-approve"];
    if legacy {
        arguments.push("--legacy-harness");
    }
    let output = vibe(world, &arguments)?;
    exits(&output, 0)?;
    ensure(output.stdout == "traced\n", || output.stdout.clone())
}

/// Every span the trace route received, in export order.
fn spans(world: &World) -> Vec<Value> {
    world
        .platform()
        .journal()
        .into_iter()
        .filter(|entry| entry.queue == "traces")
        .flat_map(|entry| entry.body["spans"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// `(name, attributes)` of the spans a legacy tool turn exports, with the
/// attributes the criterion compares; the model is the configured model's
/// name and the response id the platform's.
fn legacy_expectation(model: &str) -> Vec<(String, Value)> {
    let chat = json!({
        "gen_ai.request.model": model,
        "gen_ai.response.model": model,
        "gen_ai.request.temperature": 0.2,
        "gen_ai.response.id": "cmpl-e2e",
        "http.response.status_code": 200,
    });
    vec![
        (format!("chat {model}"), chat.clone()),
        (
            "execute_tool bash".to_owned(),
            json!({"gen_ai.tool.name": "bash"}),
        ),
        (format!("chat {model}"), chat),
        (
            "invoke_agent mistral-vibe".to_owned(),
            json!({"gen_ai.request.model": model}),
        ),
    ]
}

fn legacy_spans(ctx: &mut Ctx) -> Outcome {
    let world = traced(ctx)?;
    tool_turn(&world, true)?;
    let exported = spans(&world);
    let names: Vec<&str> = exported
        .iter()
        .map(|span| span["name"].as_str().unwrap_or_default())
        .collect();
    let expected = legacy_expectation(world.provider().model_name());
    let expected_names: Vec<&str> = expected.iter().map(|(name, _)| name.as_str()).collect();
    ensure(names == expected_names, || {
        format!("the turn exported the spans {names:?}, the reference exports {expected_names:?}")
    })?;
    for (span, (name, attributes)) in exported.iter().zip(&expected) {
        ensure(span["status"]["code"] == 1, || {
            format!("{name} ended with the status {}", span["status"])
        })?;
        for (key, value) in attributes.as_object().into_iter().flatten() {
            ensure(span["attributes"][key] == *value, || {
                format!(
                    "{name} carries {key} = {}, the reference's is {value}",
                    span["attributes"][key]
                )
            })?;
        }
    }
    Ok(())
}

/// The reference's Unified Harness turn opens no span, so neither
/// implementation exports one.
fn unified_spans(ctx: &mut Ctx) -> Outcome {
    let world = traced(ctx)?;
    tool_turn(&world, false)?;
    let exported = spans(&world);
    ensure(exported.is_empty(), || {
        format!("the turn exported {exported:?}")
    })
}

fn failing_collector(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = traced(ctx)?;
    world.platform().route(
        "traces",
        RouteReply::raw(500, "text/plain", "collector failure"),
    );
    tool_turn(&world, legacy)
}
