//! US-013: the `vibe` surface outside the TUI: worktrees, signals, argument
//! errors, budgets, cross-harness resume, and the vibe home a run leaves.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use vibe_e2e::{Ctx, Outcome, Reply, RouteReply, Scenario, World, ensure};

use crate::cli::{
    chat_count, exits, interrupted_entries, last_user, session_id, stream_entries, vibe, with_role,
};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        Scenario::new(
            "surface::worktree::a_named_worktree_is_created_then_reused",
            named_worktree,
        ),
        Scenario::new(
            "surface::worktree::an_unnamed_worktree_is_named_by_the_model",
            unnamed_worktree,
        )
        .known_defect(
            "E2E-D32",
            "an unnamed worktree is named from the prompt locally, without a utility request",
        ),
        Scenario::new(
            "surface::worktree::outside_a_repository_fails",
            worktree_outside,
        ),
        Scenario::new(
            "surface::update::check_upgrade_with_no_newer_release",
            |ctx| update_check(ctx, "--check-upgrade", None),
        )
        .port_only(UPDATE_HOST)
        .tests_seam("VIBE_UPDATE_BASE_URL"),
        Scenario::new(
            "surface::update::update_command_with_no_newer_release",
            |ctx| update_check(ctx, "update", None),
        )
        .port_only(UPDATE_HOST)
        .tests_seam("VIBE_UPDATE_BASE_URL"),
        Scenario::new(
            "surface::update::check_upgrade_with_a_newer_release",
            |ctx| update_check(ctx, "--check-upgrade", Some("99.0.0")),
        )
        .port_only(UPDATE_HOST)
        .tests_seam("VIBE_UPDATE_BASE_URL"),
        Scenario::new(
            "surface::update::update_command_with_a_newer_release",
            |ctx| update_check(ctx, "update", Some("99.0.0")),
        )
        .port_only(UPDATE_HOST)
        .tests_seam("VIBE_UPDATE_BASE_URL"),
        Scenario::new(
            "surface::signals::sigint_with_the_default_disposition",
            |ctx| interrupted(ctx, false),
        ),
        Scenario::new("surface::signals::sigint_with_an_inherited_ignore", |ctx| {
            interrupted(ctx, true)
        })
        .known_defect(
            "E2E-D35",
            "`vibe -p` stops on SIGINT even when it was started with SIGINT ignored",
        ),
        Scenario::new("surface::signals::stdout_closed_early", stdout_closed),
        Scenario::new(
            "surface::arguments::the_usage_line_lists_the_options",
            usage_line,
        )
        .known_defect(
            "E2E-D33",
            "an argument error prints a one-line `usage: vibe [OPTIONS] [PROMPT]` synopsis \
             instead of the option list",
        ),
    ];
    for (name, arguments, error) in ARGUMENT_ERRORS {
        scenarios.push(Scenario::new(
            format!("surface::arguments::{name}"),
            move |ctx| argument_error(ctx, arguments, error),
        ));
    }
    scenarios.extend([
        Scenario::new("surface::budgets::max_turns::unified", |ctx| {
            max_turns(ctx, false)
        }),
        Scenario::new("surface::budgets::max_turns::legacy", |ctx| {
            max_turns(ctx, true)
        }),
        Scenario::new("surface::budgets::max_tokens_is_not_enforced", |ctx| {
            unenforced_budget(ctx, &["--max-tokens", "50"])
        }),
        Scenario::new("surface::budgets::max_price_is_not_enforced", |ctx| {
            unenforced_budget(ctx, &["--max-price", "0.000001"])
        }),
    ]);
    for (from, legacy_first) in [("legacy", true), ("unified", false)] {
        for (how, by_id) in [("continue", false), ("resume", true)] {
            let scenario = Scenario::new(
                format!("surface::cross_harness::{how}_a_{from}_session"),
                move |ctx| cross_harness(ctx, legacy_first, by_id),
            );
            scenarios.push(if by_id && !legacy_first {
                scenario.known_defect(
                    "E2E-D36",
                    "`--legacy-harness --resume <id>` cannot find a Unified Harness session",
                )
            } else {
                scenario
            });
        }
    }
    scenarios.push(
        Scenario::new(
            "surface::home::a_run_leaves_the_reference_inventory",
            inventory,
        )
        .known_defect(
            "E2E-D34",
            "the vibe home a run leaves diverges: `.config.lock` and `builtin-plugins/` added, \
             no plugin `blobs/` or `manifests/`, and wider directory modes",
        ),
    );
    scenarios
}

fn git(world: &World, arguments: &[&str]) -> Outcome<String> {
    let output = world.program("git").args(arguments.iter().copied()).run()?;
    exits(&output, 0)?;
    Ok(output.stdout)
}

/// The worktree directories under `$VIBE_HOME/worktrees/<repository>/`.
fn worktrees(world: &World) -> Vec<String> {
    let root = world.vibe_home().join("worktrees");
    let mut found = Vec::new();
    for repository in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let name = repository.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        for tree in std::fs::read_dir(repository.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            found.push(tree.file_name().to_string_lossy().into_owned());
        }
    }
    found.sort();
    found
}

fn named_worktree(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("first"), Reply::text("second")]);
    world.platform().exact();
    for prompt in ["hello", "again"] {
        let output = vibe(
            &world,
            &["-p", prompt, "--worktree", "feat", "--output", "text"],
        )?;
        exits(&output, 0)?;
    }
    ensure(worktrees(&world) == ["feat"], || {
        format!("the worktrees are {:?}", worktrees(&world))
    })?;
    let branches = git(&world, &["branch", "--list", "feat"])?;
    ensure(branches.contains("feat"), || {
        "no branch named feat was created".to_owned()
    })?;
    chat_count(&world, 2).map(drop)
}

fn unnamed_worktree(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("login-fix"), Reply::text("fixed")]);
    world.platform().exact();
    let output = vibe(
        &world,
        &["-p", "fix the login bug", "--worktree", "--output", "text"],
    )?;
    exits(&output, 0)?;
    ensure(output.stdout == "fixed\n", || output.stdout.clone())?;
    ensure(worktrees(&world) == ["login-fix"], || {
        format!("the worktrees are {:?}", worktrees(&world))
    })?;
    let branches = git(&world, &["branch", "--list", "vibe/login-fix"])?;
    ensure(branches.contains("vibe/login-fix"), || {
        "no vibe/login-fix branch was created".to_owned()
    })?;
    chat_count(&world, 2).map(drop)
}

fn worktree_outside(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("ran"));
    let output = world
        .command("vibe")?
        .args(["-p", "hello", "--worktree", "feat", "--output", "text"])
        .current_dir(world.home())
        .run()?;
    exits(&output, 1)?;
    chat_count(&world, 0)?;
    ensure(worktrees(&world).is_empty(), || {
        format!("worktrees were created: {:?}", worktrees(&world))
    })
}

/// Why the update scenarios cannot run against the reference: its release
/// index is hardcoded (`vibe/cli/update_notifier/adapters/pypi_update_gateway.py`),
/// so a jailed check reaches the sentinel. The TTY prompt waits for US-026.
const UPDATE_HOST: &str = "the reference's update host, pypi.org, is hardcoded; only the port's \
                           release index is redirectable, through VIBE_UPDATE_BASE_URL";

/// `--check-upgrade`, or `update` in first position, against the platform's
/// release route, without a TTY: the index is asked once and the cache records
/// the latest version.
fn update_check(ctx: &mut Ctx, command: &str, newer: Option<&str>) -> Outcome {
    let world = ctx.world()?;
    if let Some(version) = newer {
        world.platform().route(
            "release_github",
            RouteReply::json(
                200,
                &json!([{"tag_name": format!("v{version}"), "published_at": "2026-01-01T00:00:00Z",
                         "prerelease": false, "draft": false}]),
            ),
        );
    }
    let output = world
        .command("vibe")?
        .arg(command)
        .env("VIBE_UPDATE_BASE_URL", world.platform().base())
        .run()?;
    let checks = world
        .platform()
        .journal()
        .iter()
        .filter(|entry| entry.queue == "release_github")
        .count();
    ensure(checks == 1, || {
        format!("the release index was asked {checks} time(s)")
    })?;
    chat_count(&world, 0)?;
    let current = env!("CARGO_PKG_VERSION");
    let latest = newer.unwrap_or(current);
    let cache = std::fs::read_to_string(world.vibe_home().join("cache.toml")).unwrap_or_default();
    ensure(
        cache.contains(&format!("latest_version = \"{latest}\"")),
        || format!("the update cache does not record {latest}: {cache}"),
    )?;
    ensure(!output.stdout.contains('✗'), || {
        format!("the check reported a failure: {}", output.stdout)
    })?;
    if newer.is_some() {
        // The newer release opens the update prompt, which needs a terminal:
        // the port refuses without one. The reference's answer could not be
        // measured jailed, so the exit code is left to the PTY scenario of
        // US-026.
        return Ok(());
    }
    exits(&output, 0)?;
    ensure(output.stdout.contains(current), || {
        format!(
            "the output does not name the installed version: {}",
            output.stdout
        )
    })
}

/// A paced streaming run, interrupted after three pieces. With the signal
/// ignored before `exec`, the run streams to its end.
fn interrupted(ctx: &mut Ctx, ignored: bool) -> Outcome {
    let world = ctx.world()?;
    let pieces: Vec<String> = (1..=10).map(|n| format!("piece {n} ")).collect();
    world.platform().reply(Reply::chunks(pieces).paced(200));
    let vibe = world.binary("vibe")?;
    let script = format!(
        "{}exec \"$0\" -p go --output streaming",
        if ignored { "trap '' INT; " } else { "" }
    );
    let child = world
        .program("sh")
        .arg("-c")
        .arg(script)
        .arg(vibe)
        .spawn()?;
    world
        .platform()
        .await_events(1, 4, Duration::from_secs(30))?;
    child.interrupt()?;
    let output = child.wait_within(Duration::from_secs(10))?;
    exits(&output, 0)?;
    if ignored {
        stream_entries(&output.stdout)?;
        ensure(output.stdout.contains("piece 10"), || {
            format!("the ignored interrupt cut the answer: {}", output.stdout)
        })
    } else {
        let entries = interrupted_entries(&output.stdout)?;
        ensure(
            !entries.is_empty() && !output.stdout.contains("piece 10"),
            || format!("the interrupt did not stop the stream: {}", output.stdout),
        )
    }
}

/// `head` closes the pipe after the first line, and `vibe` fails on the next
/// write. The exit code differs: the port exits 1, the reference dies in an
/// uncaught `BrokenPipeError` and exits 120, which is left to a decision
/// rather than encoded here.
fn stdout_closed(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "echo one"})),
        Reply::text("done"),
    ]);
    let vibe = world.binary("vibe")?;
    let output = world
        .program("bash")
        .arg("-c")
        .arg("\"$0\" -p go --output streaming --auto-approve | head -n 1; echo \"rc=${PIPESTATUS[0]}\" >&2")
        .arg(vibe)
        .run()?;
    exits(&output, 0)?;
    let entries = stream_entries(&output.stdout)?;
    ensure(entries.len() == 1, || {
        format!("head kept {} lines", entries.len())
    })?;
    let code = output
        .stderr
        .lines()
        .find_map(|line| line.strip_prefix("rc="))
        .unwrap_or_default();
    ensure(!code.is_empty() && code != "0", || {
        format!("vibe met the closed pipe with: {}", output.stderr)
    })
}

/// Argument errors and the error line argparse words for each.
const ARGUMENT_ERRORS: [(&str, &str, &str); 8] = [
    (
        "an_unknown_flag",
        "--bogus",
        "vibe: error: unrecognized arguments: --bogus",
    ),
    (
        "a_missing_output_value",
        "-p hi --output",
        "vibe: error: argument --output: expected one argument",
    ),
    (
        "an_invalid_output_choice",
        "-p hi --output xml",
        "vibe: error: argument --output: invalid choice: 'xml' (choose from text, json, streaming)",
    ),
    (
        "continue_with_resume",
        "-p hi -c --resume abc",
        "vibe: error: argument --resume: not allowed with argument -c/--continue",
    ),
    (
        "a_non_integer_max_turns",
        "-p hi --max-turns abc",
        "vibe: error: argument --max-turns: invalid int value: 'abc'",
    ),
    (
        "a_non_numeric_max_price",
        "-p hi --max-price x",
        "vibe: error: argument --max-price: invalid float value: 'x'",
    ),
    (
        "both_harness_flags",
        "-p hi --legacy-harness --experimental-harness",
        "vibe: error: argument --experimental-harness: not allowed with argument --legacy-harness",
    ),
    (
        "a_missing_agent_value",
        "-p hi --agent",
        "vibe: error: argument --agent: expected one argument",
    ),
];

fn argument_error(ctx: &mut Ctx, arguments: &str, error: &str) -> Outcome {
    let world = ctx.world()?;
    let output = vibe(&world, &arguments.split(' ').collect::<Vec<_>>())?;
    exits(&output, 2)?;
    chat_count(&world, 0)?;
    let last = output.stderr.trim_end().lines().last().unwrap_or_default();
    ensure(last == error, || {
        format!("the error line is {last:?}, not {error:?}")
    })
}

fn usage_line(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let output = vibe(&world, &["--bogus"])?;
    exits(&output, 2)?;
    let first = output.stderr.lines().next().unwrap_or_default();
    ensure(
        first.starts_with("usage: vibe [-h] [-v] [-p [TEXT]]"),
        || format!("the usage line is {first:?}"),
    )
}

fn looping(world: &World) {
    world.platform().replies(
        (0..5)
            .map(|n| {
                Reply::tool_call("bash", json!({"command": format!("echo {n}")})).usage(1000, 100)
            })
            .chain([Reply::text("done")]),
    );
}

/// The Unified Harness ends the run quietly at the limit; the legacy loop
/// reports it and exits 1.
fn max_turns(ctx: &mut Ctx, legacy: bool) -> Outcome {
    let world = ctx.world()?;
    looping(&world);
    let mut arguments = vec![
        "-p",
        "go",
        "--output",
        "text",
        "--auto-approve",
        "--max-turns",
        "2",
    ];
    if legacy {
        arguments.push("--legacy-harness");
    }
    let output = vibe(&world, &arguments)?;
    exits(&output, i32::from(legacy))?;
    ensure(output.stdout.is_empty(), || output.stdout.clone())?;
    chat_count(&world, 2).map(drop)
}

/// shared_with_reference: neither implementation enforces `--max-tokens` or
/// `--max-price` on the Unified Harness (PRD shared-defect list).
fn unenforced_budget(ctx: &mut Ctx, budget: &[&str]) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies(
        (0..3)
            .map(|n| {
                Reply::tool_call("bash", json!({"command": format!("echo {n}")})).usage(1000, 100)
            })
            .chain([Reply::text("done")]),
    );
    world.platform().exact();
    let mut arguments = vec!["-p", "go", "--output", "text", "--auto-approve"];
    arguments.extend_from_slice(budget);
    let output = vibe(&world, &arguments)?;
    exits(&output, 0)?;
    ensure(output.stdout == "done\n", || output.stdout.clone())?;
    chat_count(&world, 4).map(drop)
}

/// `-c` only sees sessions of the harness it runs on; `--resume <id>` finds a
/// session whichever harness saved it.
fn cross_harness(ctx: &mut Ctx, legacy_first: bool, by_id: bool) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("first"), Reply::text("second")]);
    let mut first = vec!["-p", "one", "--output", "json"];
    if legacy_first {
        first.push("--legacy-harness");
    }
    let created = vibe(&world, &first)?;
    exits(&created, 0)?;
    let id = session_id(&created.stdout)?;
    let mut second = vec!["-p", "two", "--output", "text"];
    if by_id {
        second.extend(["--resume", &id]);
    } else {
        second.push("-c");
    }
    if !legacy_first {
        second.push("--legacy-harness");
    }
    let output = vibe(&world, &second)?;
    if !by_id {
        exits(&output, 1)?;
        return chat_count(&world, 1).map(drop);
    }
    exits(&output, 0)?;
    ensure(output.stdout == "second\n", || output.stdout.clone())?;
    let requests = chat_count(&world, 2)?;
    let users: Vec<String> = with_role(&requests[1], "user")
        .into_iter()
        .map(crate::cli::content_text)
        .collect();
    ensure(
        users == ["one", "two"] && last_user(&requests[1]) == "two",
        || format!("the resumed turn carried the user messages {users:?}"),
    )
}

/// One normalized line per entry of `root`: mode, kind, and path, with
/// content hashes, their shard directories, ids, and session stamps
/// replaced, and the files inside a stored plugin package left out.
fn listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, into: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let relative = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            into.push(format!(
                "{:o} {} {}",
                mode(&metadata),
                if metadata.is_dir() { 'd' } else { 'f' },
                normalize(&relative)
            ));
            let packaged =
                relative.contains("plugins/packages/") && relative.split('/').count() >= 6;
            if metadata.is_dir() && !packaged {
                walk(root, &path, into);
            }
        }
    }
    let mut lines = Vec::new();
    walk(root, root, &mut lines);
    lines.sort_by(|a, b| a.split(' ').nth(2).cmp(&b.split(' ').nth(2)));
    lines.dedup();
    lines
}

#[cfg(unix)]
fn mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_metadata: &std::fs::Metadata) -> u32 {
    0
}

fn normalize(path: &str) -> String {
    let hex = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    };
    path.split('/')
        .map(|part| {
            let (stem, extension) = part.split_once('.').map_or((part, ""), |(s, e)| (s, e));
            let stem = if stem.len() == 64 && hex(stem) {
                "HASH"
            } else if stem.len() == 2 && hex(stem) {
                "XX"
            } else if stem.len() == 16 && stem.chars().all(|c| c.is_ascii_digit()) {
                "N"
            } else if stem.len() == 36 && stem.matches('-').count() == 4 {
                "ID"
            } else {
                stem
            };
            if extension.is_empty() {
                stem.to_owned()
            } else {
                format!("{stem}.{extension}")
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The normalized vibe home the reference leaves after one `vibe -p` run with
/// one tool call, measured on 2026-10-28.
const REFERENCE_HOME: &[&str] = &[
    "644 f config.toml",
    "700 d logs",
    "755 d logs/session",
    "700 d logs/session/active",
    "600 f logs/session/active/.registry",
    "755 d logs/session/plugins",
    "755 d logs/session/plugins/blobs",
    "700 d logs/session/plugins/blobs/XX",
    "400 f logs/session/plugins/blobs/XX/HASH",
    "755 d logs/session/plugins/data",
    "755 d logs/session/plugins/data/ID",
    "755 d logs/session/plugins/data/ID/vibe",
    "755 d logs/session/plugins/manifests",
    "700 d logs/session/plugins/manifests/XX",
    "400 f logs/session/plugins/manifests/XX/HASH",
    "755 d logs/session/plugins/packages",
    "700 d logs/session/plugins/packages/XX",
    "500 d logs/session/plugins/packages/XX/HASH",
    "700 d logs/session/plugins/tmp",
    "755 d logs/session/unified",
    "700 d logs/session/unified/ID",
    "600 f logs/session/unified/ID/CURRENT",
    "700 d logs/session/unified/ID/chunks",
    "600 f logs/session/unified/ID/chunks/HASH.json",
    "700 d logs/session/unified/ID/generations",
    "700 d logs/session/unified/ID/generations/N",
    "600 f logs/session/unified/ID/generations/N/checkpoint.json",
    "600 f logs/session/unified/ID/generations/N/manifest.json",
    "600 f logs/session/unified/ID/generations/N/projection-state.json",
    "600 f logs/session/unified/ID/generations/N/runtime-state.json",
    "700 d logs/session/unified/ID/journal",
    "600 f logs/session/unified/ID/journal/N.jsonl",
    "600 f logs/session/unified/ID/meta.json",
    "600 f logs/session/unified/ID/scheduled-loops.json",
    "600 f logs/vibe.log",
    "600 f trusted_folders.toml",
    "600 f vibehistory",
];

fn inventory(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::tool_call("bash", json!({"command": "echo hi"})),
        Reply::text("done"),
    ]);
    let output = vibe(&world, &["-p", "go", "--output", "text", "--auto-approve"])?;
    exits(&output, 0)?;
    let found = listing(&world.vibe_home());
    ensure(found == REFERENCE_HOME, || {
        format!("the vibe home holds:\n{}", found.join("\n"))
    })
}
