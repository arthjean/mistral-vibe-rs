//! US-020, US-024, US-025: the app-server protocol over stdio, as its clients
//! drive it, against `vibe-app-server-e2e` (composed by the function `vibe`
//! builds its own server with) or the reference's `vibe-app-server`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_e2e::{Ctx, Entry, Outcome, Reply, Rpc, Scenario, Subject, World, ensure};

use crate::cli::{content_text, exits, session_id, vibe, with_role};

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![Scenario::new(
        "app_server::self_test::initialize_round_trip",
        initialize_round_trip,
    )];
    for (harness, flags) in HARNESSES {
        scenarios.push(Scenario::new(
            format!("app_server::lifecycle::{harness}::two_turns_survive_a_restart"),
            move |ctx| lifecycle(ctx, flags),
        ));
        scenarios.push(
            Scenario::new(
                format!("app_server::root::{harness}::a_resumed_session_runs_the_next_turn"),
                move |ctx| resumed_root(ctx, flags),
            )
            .known_defect(
                "E2E-D27",
                "resuming another session on a connection with a root keeps the old root",
            ),
        );
    }
    scenarios.extend([
        Scenario::new(
            "app_server::root::unified::an_attached_fork_runs_the_next_turn",
            attached_fork,
        )
        .known_defect(
            "E2E-D27",
            "an attached `session/fork` keeps the old root on the default harness",
        ),
        Scenario::new(
            "app_server::cli::a_vibe_p_session_is_listed_and_continued",
            cli_session_in_server,
        ),
        Scenario::new(
            "app_server::cli::vibe_p_resume_continues_a_server_session",
            server_session_in_cli,
        ),
        Scenario::new(
            "app_server::store::every_json_document_ends_with_a_newline",
            trailing_newlines,
        )
        .known_defect(
            "E2E-D28",
            "the Unified Harness store writes JSON documents without a trailing newline",
        ),
        Scenario::new(
            "app_server::store::the_reference_lists_and_loads_a_port_session",
            reference_reads_port_session,
        )
        .needs_reference()
        .port_only("reads with the reference a session the port wrote")
        .known_defect(
            "E2E-D28",
            "the reference's reader rejects a port session whose JSON lacks a trailing newline",
        ),
        Scenario::new(
            "app_server::crash::a_killed_turn_keeps_its_user_message",
            killed_turn,
        )
        .known_defect(
            "E2E-D43",
            "a server killed during a turn loses the turn's user message and its failed state",
        ),
    ]);
    scenarios
}

/// The harnesses a scenario runs on, and the flag that selects each.
const HARNESSES: [(&str, &[&str]); 2] = [("unified", &[]), ("legacy", &["--legacy-harness"])];

/// How long a turn gets to reach its terminal notification.
pub const TURN: Duration = Duration::from_secs(30);

/// Starts the subject's app server with `flags` and completes the handshake.
pub fn server(world: &World, flags: &[&str]) -> Outcome<Rpc> {
    server_as(world, world.subject(), flags)
}

/// [`server`] as `subject` ships it, whatever this run's subject.
pub fn server_as(world: &World, subject: Subject, flags: &[&str]) -> Outcome<Rpc> {
    let child = world
        .command_for(subject, "vibe-app-server")?
        .args(flags.iter().copied())
        .stdin_piped()
        .spawn()?;
    let mut rpc = Rpc::new(child);
    rpc.call(
        "initialize",
        json!({"clientInfo": {"name": "vibe-e2e", "version": "0"}}),
    )?;
    rpc.notify("initialized", json!({}))?;
    Ok(rpc)
}

/// Closes the client's end and waits for the server to exit.
pub fn stop(rpc: Rpc) -> Outcome {
    rpc.hang_up().map(drop)
}

/// Opens a session in the world's work tree and returns its id.
pub fn start(rpc: &mut Rpc, world: &World) -> Outcome<String> {
    let result = rpc.call(
        "session/start",
        json!({"agentConfig": {"cwd": world.work(), "agent": "auto-approve"}}),
    )?;
    state_id(&result)
}

/// The session id of a response carrying a session state.
pub fn state_id(result: &Value) -> Outcome<String> {
    result["state"]["session"]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("no session state in {result}").into())
}

/// Starts a turn and returns its id without waiting for it.
pub fn start_turn(rpc: &mut Rpc, session: &str, text: &str) -> Outcome<String> {
    let result = rpc.call(
        "turn/start",
        json!({"sessionId": session, "message": [{"type": "text", "text": text}]}),
    )?;
    result["turn"]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("turn/start answered {result}").into())
}

/// The terminal notification of the turn `id`.
pub fn turn_end(rpc: &mut Rpc, id: &str) -> Outcome<Value> {
    rpc.await_notification(
        |m| {
            matches!(
                m["method"].as_str(),
                Some("turn/completed" | "turn/failed" | "turn/interrupted")
            ) && m["params"]["turn"]["id"] == id
        },
        TURN,
    )
}

/// Runs a turn and fails unless it completes.
pub fn turn(rpc: &mut Rpc, session: &str, text: &str) -> Outcome {
    let id = start_turn(rpc, session, text)?;
    let end = turn_end(rpc, &id)?;
    ensure(end["method"] == "turn/completed", || {
        format!("the turn {text:?} ended with {end}")
    })
}

/// The history entries `session/read` returns.
pub fn history(rpc: &mut Rpc, session: &str) -> Outcome<Vec<Value>> {
    let result = rpc.call("session/read", json!({"sessionId": session}))?;
    result["state"]["history"]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("session/read answered {result}").into())
}

/// `role:text` for each message entry, in order.
pub fn transcript(history: &[Value]) -> Vec<String> {
    history
        .iter()
        .filter(|entry| entry["type"] == "message")
        .map(|entry| {
            let text: String = entry["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect();
            format!("{}:{text}", entry["role"].as_str().unwrap_or("?"))
        })
        .collect()
}

/// The ids of every history entry, in order.
fn entry_ids(history: &[Value]) -> Vec<String> {
    history
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect()
}

/// The ids `session/list` returns.
pub fn listed(rpc: &mut Rpc) -> Outcome<Vec<String>> {
    let result = rpc.call("session/list", json!({}))?;
    Ok(result["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect())
}

/// The user texts of the last turn request the platform received.
fn last_request_users(world: &World) -> Outcome<Vec<String>> {
    let last: Entry = world
        .platform()
        .chat_requests()
        .into_iter()
        .rfind(|entry| entry.queue == "turn")
        .ok_or("the platform received no turn request")?;
    Ok(with_role(&last, "user")
        .into_iter()
        .map(content_text)
        .collect())
}

/// Fails unless the last turn request carried every text in `present` and
/// none in `absent`.
fn request_carries(world: &World, present: &[&str], absent: &[&str]) -> Outcome {
    let users = last_request_users(world)?;
    let joined = users.join("\n");
    for text in present {
        ensure(joined.contains(text), || {
            format!("the last turn request does not carry {text:?}: {users:?}")
        })?;
    }
    for text in absent {
        ensure(!joined.contains(text), || {
            format!("the last turn request carries {text:?}: {users:?}")
        })?;
    }
    Ok(())
}

fn initialize_round_trip(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    let mut rpc = Rpc::new(world.command("vibe-app-server")?.stdin_piped().spawn()?);
    let result = rpc.call(
        "initialize",
        json!({"clientInfo": {"name": "vibe-e2e", "version": "0"}}),
    )?;
    ensure(result["serverInfo"]["name"].is_string(), || {
        format!("initialize answered {result}")
    })?;
    stop(rpc)
}

fn lifecycle(ctx: &mut Ctx, flags: &[&str]) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::text("answer one"),
        Reply::text("answer two"),
        Reply::text("answer three"),
    ]);
    world.platform().exact();
    let mut rpc = server(&world, flags)?;
    let session = start(&mut rpc, &world)?;
    turn(&mut rpc, &session, "prompt one")?;
    turn(&mut rpc, &session, "prompt two")?;
    let before = listed(&mut rpc)?;
    ensure(before == [session.clone()], || {
        format!("session/list returned {before:?}, expected only {session}")
    })?;
    let first = history(&mut rpc, &session)?;
    let expected = [
        "user:prompt one",
        "assistant:answer one",
        "user:prompt two",
        "assistant:answer two",
    ];
    ensure(transcript(&first) == expected, || {
        format!("the history reads {:?}", transcript(&first))
    })?;
    stop(rpc)?;

    let mut rpc = server(&world, flags)?;
    let after = listed(&mut rpc)?;
    ensure(after == [session.clone()], || {
        format!("after a restart session/list returned {after:?}, expected {session}")
    })?;
    let resumed = rpc.call("session/resume", json!({"sessionId": session}))?;
    ensure(state_id(&resumed)? == session, || {
        format!("session/resume opened {resumed}")
    })?;
    turn(&mut rpc, &session, "prompt three")?;
    let last = history(&mut rpc, &session)?;
    let ids = entry_ids(&last);
    ensure(ids.starts_with(&entry_ids(&first)), || {
        format!(
            "the entry ids changed across the restart: {:?} then {ids:?}",
            entry_ids(&first)
        )
    })?;
    let mut expected = expected.to_vec();
    expected.extend(["user:prompt three", "assistant:answer three"]);
    ensure(transcript(&last) == expected, || {
        format!(
            "after the third turn the history reads {:?}",
            transcript(&last)
        )
    })?;
    request_carries(&world, &["prompt one", "prompt two", "prompt three"], &[])?;
    stop(rpc)
}

fn resumed_root(ctx: &mut Ctx, flags: &[&str]) -> Outcome {
    let world = ctx.world()?;
    world.platform().replies([
        Reply::text("alpha answer"),
        Reply::text("beta answer"),
        Reply::text("alpha again answer"),
    ]);
    world.platform().exact();
    let mut rpc = server(&world, flags)?;
    let alpha = start(&mut rpc, &world)?;
    turn(&mut rpc, &alpha, "alpha prompt")?;
    stop(rpc)?;
    let mut rpc = server(&world, flags)?;
    let beta = start(&mut rpc, &world)?;
    turn(&mut rpc, &beta, "beta prompt")?;
    let resumed = rpc.call("session/resume", json!({"sessionId": alpha}))?;
    ensure(state_id(&resumed)? == alpha, || {
        format!("session/resume opened {resumed}")
    })?;
    turn(&mut rpc, &alpha, "alpha again")?;
    request_carries(&world, &["alpha prompt", "alpha again"], &["beta prompt"])?;
    stop(rpc)
}

fn attached_fork(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("source answer"), Reply::text("fork answer")]);
    world.platform().exact();
    let mut rpc = server(&world, &[])?;
    let source = start(&mut rpc, &world)?;
    turn(&mut rpc, &source, "source prompt")?;
    let forked = rpc.call("session/fork", json!({"sourceSessionId": source}))?;
    let fork = state_id(&forked)?;
    ensure(fork != source, || format!("session/fork answered {forked}"))?;
    turn(&mut rpc, &fork, "fork prompt")?;
    request_carries(&world, &["source prompt", "fork prompt"], &[])?;
    stop(rpc)
}

fn cli_session_in_server(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("cli answer"), Reply::text("server answer")]);
    world.platform().exact();
    let output = vibe(&world, &["-p", "from the cli", "--output", "json"])?;
    exits(&output, 0)?;
    let session = session_id(&output.stdout)?;
    let mut rpc = server(&world, &[])?;
    let ids = listed(&mut rpc)?;
    ensure(ids.contains(&session), || {
        format!("session/list returned {ids:?}, missing the vibe -p session {session}")
    })?;
    rpc.call("session/resume", json!({"sessionId": session}))?;
    turn(&mut rpc, &session, "from the server")?;
    request_carries(&world, &["from the cli", "from the server"], &[])?;
    stop(rpc)
}

fn server_session_in_cli(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world
        .platform()
        .replies([Reply::text("server answer"), Reply::text("cli answer")]);
    world.platform().exact();
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    turn(&mut rpc, &session, "from the server")?;
    stop(rpc)?;
    let output = vibe(
        &world,
        &[
            "-p",
            "from the cli",
            "--resume",
            &session,
            "--output",
            "text",
        ],
    )?;
    exits(&output, 0)?;
    ensure(output.stdout == "cli answer\n", || {
        format!("vibe -p --resume printed {:?}", output.stdout)
    })?;
    request_carries(&world, &["from the server", "from the cli"], &[])
}

/// Every file under `directory` whose name ends with `.json`.
fn json_documents(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(json_documents(&path));
        } else if path.extension().is_some_and(|e| e == "json") {
            found.push(path);
        }
    }
    found
}

/// The directory the default harness stores its sessions in.
fn unified_store(world: &World) -> PathBuf {
    world.vibe_home().join("logs/session/unified")
}

fn trailing_newlines(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("stored"));
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    turn(&mut rpc, &session, "store this")?;
    stop(rpc)?;
    let documents = json_documents(&unified_store(&world));
    ensure(!documents.is_empty(), || {
        format!("no JSON document under {}", unified_store(&world).display())
    })?;
    // The reference ends every document with a newline but `meta.json`
    // (measured 2026-10-29), which its reader accepts either way.
    let unterminated: Vec<String> = documents
        .iter()
        .filter(|path| path.file_name().is_none_or(|name| name != "meta.json"))
        .filter(|path| !std::fs::read(path).is_ok_and(|bytes| bytes.ends_with(b"\n")))
        .map(|path| {
            path.strip_prefix(world.vibe_home())
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect();
    ensure(unterminated.is_empty(), || {
        format!("JSON documents without a trailing newline: {unterminated:?}")
    })
}

fn reference_reads_port_session(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("written by the port"));
    let mut rpc = server_as(&world, Subject::Port, &[])?;
    let session = start(&mut rpc, &world)?;
    turn(&mut rpc, &session, "port prompt")?;
    stop(rpc)?;
    let mut rpc = server_as(&world, Subject::Reference, &[])?;
    let ids = listed(&mut rpc)?;
    ensure(ids.contains(&session), || {
        format!("the reference listed {ids:?}, missing the port session {session}")
    })?;
    rpc.call("session/resume", json!({"sessionId": session}))?;
    let read = history(&mut rpc, &session)?;
    ensure(
        transcript(&read) == ["user:port prompt", "assistant:written by the port"],
        || format!("the reference loaded {:?}", transcript(&read)),
    )?;
    stop(rpc)
}

fn killed_turn(ctx: &mut Ctx) -> Outcome {
    let world = ctx.world()?;
    world.platform().reply(Reply::text("never delivered"));
    let gate = world.platform().gate(1);
    let mut rpc = server(&world, &[])?;
    let session = start(&mut rpc, &world)?;
    start_turn(&mut rpc, &session, "survive the crash")?;
    world.platform().await_request(1, TURN)?;
    rpc.child().crash();
    drop(rpc);
    gate.release();
    // The killed server leaves its lease behind, and a fresh server takes the
    // session over regardless.
    let lease = [
        ".registry".to_owned(),
        format!("{session}.lock"),
        format!("{session}.lock.json"),
    ];
    let locks = lock_files(&world);
    ensure(locks == lease, || {
        format!("after the crash the lease directory holds {locks:?}, expected {lease:?}")
    })?;
    let mut rpc = server(&world, &[])?;
    let ids = listed(&mut rpc)?;
    ensure(ids == [session.clone()], || {
        format!("after the crash session/list returned {ids:?}")
    })?;
    let resumed = rpc.call("session/resume", json!({"sessionId": session}))?;
    let state = &resumed["state"];
    ensure(
        transcript(state["history"].as_array().map_or(&[], Vec::as_slice))
            == ["user:survive the crash"],
        || format!("the resumed history reads {}", state["history"]),
    )?;
    ensure(
        state["session"]["status"]["type"] == "failed" && state["turns"][0]["status"] == "failed",
        || {
            format!(
                "the killed turn resumed as session status {} and turns {}",
                state["session"]["status"], state["turns"]
            )
        },
    )?;
    stop(rpc)
}

/// The names under the lease directory, sorted.
fn lock_files(world: &World) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(world.vibe_home().join("logs/session/active"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
