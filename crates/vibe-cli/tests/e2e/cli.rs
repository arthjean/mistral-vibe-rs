//! What the programmatic scenarios share: running `vibe -p`, reading the
//! journal, and the configuration snippets a world adds.

use serde_json::Value;
use vibe_e2e::{Entry, Outcome, Output, World, ensure};

/// Runs the subject's `vibe` with `arguments` in the world's work tree.
pub fn vibe(world: &World, arguments: &[&str]) -> Outcome<Output> {
    world.command("vibe")?.args(arguments.iter().copied()).run()
}

/// Fails unless `output` exited with `code`, quoting both streams.
pub fn exits(output: &Output, code: i32) -> Outcome {
    ensure(output.code() == Some(code), || {
        format!(
            "expected exit code {code}, got {:?}\nstdout:\n{}\nstderr:\n{}",
            output.code(),
            output.stdout,
            output.stderr
        )
    })
}

/// The chat-completions requests the world's platform received.
pub fn chats(world: &World) -> Vec<Entry> {
    world.platform().chat_requests()
}

/// Fails unless the platform received exactly `count` chat requests.
pub fn chat_count(world: &World, count: usize) -> Outcome<Vec<Entry>> {
    let requests = chats(world);
    ensure(requests.len() == count, || {
        format!(
            "expected {count} chat request(s), the platform received {}",
            requests.len()
        )
    })?;
    Ok(requests)
}

/// A string field of a request body.
pub fn body_str<'a>(entry: &'a Entry, field: &str) -> Option<&'a str> {
    entry.body.get(field).and_then(Value::as_str)
}

/// The text of a message, whether its content is a string or a list of parts.
pub fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(other @ Value::Array(_)) => other.to_string(),
        _ => String::new(),
    }
}

/// Every system message of a request, joined.
pub fn system_text(entry: &Entry) -> String {
    entry
        .messages()
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .map(content_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The messages of a request with `role`.
pub fn with_role<'a>(entry: &'a Entry, role: &str) -> Vec<&'a Value> {
    entry
        .messages()
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some(role))
        .collect()
}

/// The last user message of a request.
pub fn last_user(entry: &Entry) -> String {
    with_role(entry, "user")
        .last()
        .map(|m| content_text(m))
        .unwrap_or_default()
}

/// The entries of an NDJSON `--output streaming` run.
pub fn stream_entries(stdout: &str) -> Outcome<Vec<Value>> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| format!("a streaming line is not JSON ({error}): {line}").into())
        })
        .collect()
}

/// The entries an interrupted `--output streaming` run printed before its
/// farewell: every line up to the first that is not JSON must parse, and no
/// JSON may follow it.
pub fn interrupted_entries(stdout: &str) -> Outcome<Vec<Value>> {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    let parsed: Vec<Value> = lines
        .iter()
        .map_while(|line| serde_json::from_str(line).ok())
        .collect();
    let rest = &lines[parsed.len()..];
    ensure(
        rest.iter()
            .all(|line| serde_json::from_str::<Value>(line).is_err()),
        || format!("JSON follows a line that is not JSON: {rest:?}"),
    )?;
    Ok(parsed)
}

/// The session id a `--output json` run printed.
pub fn session_id(stdout: &str) -> Outcome<String> {
    let history: Value = serde_json::from_str(stdout)
        .map_err(|error| format!("the json output does not parse ({error}): {stdout}"))?;
    history
        .pointer("/0/sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("the json output names no session: {stdout}").into())
}

/// A second model on the world's generic provider, selectable by `alias`.
pub fn extra_model(alias: &str, name: &str) -> String {
    format!(
        "[[models]]\nname = \"{name}\"\nprovider = \"e2e-provider\"\nalias = \"{alias}\"\n\
         input_price = 0.0\noutput_price = 0.0\n"
    )
}

/// Rewrites the world's `config.toml` through `edit`.
pub fn edit_config(world: &World, edit: impl FnOnce(String) -> String) -> Outcome {
    let path = world.vibe_home().join("config.toml");
    let text = std::fs::read_to_string(&path)?;
    std::fs::write(&path, edit(text))?;
    Ok(())
}
