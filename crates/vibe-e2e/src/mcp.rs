//! A scripted MCP server, over stdio or streamable HTTP.
//!
//! A scenario declares the tools an [`Mcp`] server publishes and the reaction
//! each successive call of a tool gets: a result, a JSON-RPC error, a delay,
//! or a crash. Over stdio the server is the e2e test binary itself, started
//! again with [`SERVER_ARGUMENT`] first ([`serve_if_asked`]), so no fixture
//! binary has to be built for it; it appends every message it reads, after
//! the working directory and environment it started with, to a log. The
//! argument rather than a variable selects the mode, so a launcher that
//! rebuilds the environment can never start the binary as a test run. Over HTTP
//! the [`crate::Platform`] serves it at `/mcp/<name>` and journals each request
//! with its headers.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Failure, Outcome, World};

/// The first argument that starts the binary as a stdio server, followed by
/// the script it answers from and the JSON-lines log it appends to.
pub const SERVER_ARGUMENT: &str = "--vibe-e2e-mcp-server";

/// The tools a server publishes and how each call is answered.
#[derive(Debug, Clone, Default)]
pub struct Mcp {
    tools: Vec<Value>,
    reactions: BTreeMap<String, Vec<Value>>,
    record: Vec<String>,
}

impl Mcp {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes a tool: `name`, its `inputSchema`, and an optional
    /// `outputSchema` (a `description` is added when absent).
    #[must_use]
    pub fn tool(mut self, mut declaration: Value) -> Self {
        if declaration.get("description").is_none() {
            declaration["description"] = json!("An e2e tool.");
        }
        self.tools.push(declaration);
        self
    }

    /// Queues the reaction to the next call of `tool`; the last one repeats.
    /// A reaction holds `result` (a `tools/call` result) or `error` (a
    /// JSON-RPC error object), optionally `delayMs`, or `crash: true`, which
    /// ends a stdio server and drops an HTTP connection without answering.
    #[must_use]
    pub fn react(mut self, tool: &str, reaction: Value) -> Self {
        self.reactions
            .entry(tool.to_owned())
            .or_default()
            .push(reaction);
        self
    }

    /// Records the value of `variable` when a stdio server starts.
    #[must_use]
    pub fn record_env(mut self, variable: &str) -> Self {
        self.record.push(variable.to_owned());
        self
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({"tools": self.tools, "reactions": self.reactions, "record": self.record})
    }
}

/// A text-only `tools/call` result.
#[must_use]
pub fn text_result(text: &str) -> Value {
    json!({"result": {"content": [{"type": "text", "text": text}]}})
}

/// Call counters per tool, so successive calls take successive reactions.
#[derive(Debug, Default)]
pub(crate) struct Calls(BTreeMap<String, usize>);

/// What the server does with one message.
#[derive(Debug, PartialEq)]
pub(crate) enum Answer {
    /// Sends this response after waiting.
    Respond(Value, Duration),
    /// A notification or a response: nothing to send.
    Nothing,
    /// Ends without answering.
    Crash,
}

pub(crate) fn answer(script: &Value, calls: &mut Calls, message: &Value) -> Answer {
    let Some(id) = message
        .get("id")
        .filter(|_| message.get("method").is_some())
    else {
        return Answer::Nothing;
    };
    let respond = |body: Value| {
        let mut response = json!({"jsonrpc": "2.0", "id": id});
        if let Some(error) = body.get("error") {
            response["error"] = error.clone();
        } else {
            response["result"] = body.get("result").cloned().unwrap_or_else(|| json!({}));
        }
        response
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    match message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => Answer::Respond(
            respond(json!({"result": {
                "protocolVersion": params.get("protocolVersion").cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "vibe-e2e-mcp", "version": "1.0.0"},
            }})),
            Duration::ZERO,
        ),
        "ping" => Answer::Respond(respond(json!({"result": {}})), Duration::ZERO),
        "tools/list" => Answer::Respond(
            respond(json!({"result": {"tools": script["tools"]}})),
            Duration::ZERO,
        ),
        "tools/call" => {
            let tool = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let count = calls.0.entry(tool.to_owned()).or_default();
            let reactions = script["reactions"][tool]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let reaction = reactions
                .get(*count)
                .or_else(|| reactions.last())
                .cloned()
                .unwrap_or_else(|| text_result("ok"));
            *count += 1;
            if reaction.get("crash").and_then(Value::as_bool) == Some(true) {
                return Answer::Crash;
            }
            let delay =
                Duration::from_millis(reaction.get("delayMs").and_then(Value::as_u64).unwrap_or(0));
            Answer::Respond(respond(reaction), delay)
        }
        _ => Answer::Respond(
            respond(json!({"error": {"code": -32601, "message": "Method not found"}})),
            Duration::ZERO,
        ),
    }
}

/// Serves MCP over stdio when the first argument is [`SERVER_ARGUMENT`],
/// which is how a world starts this binary as a server; [`None`] otherwise.
pub(crate) fn serve_if_asked() -> Option<ExitCode> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next()? != SERVER_ARGUMENT {
        return None;
    }
    let (Some(script), Some(log)) = (arguments.next(), arguments.next()) else {
        eprintln!("e2e mcp server: {SERVER_ARGUMENT} takes a script and a log");
        return Some(ExitCode::from(2));
    };
    Some(match serve_stdio(Path::new(&script), Path::new(&log)) {
        Ok(code) => code,
        Err(Failure(message)) => {
            eprintln!("e2e mcp server: {message}");
            ExitCode::from(2)
        }
    })
}

fn serve_stdio(script: &Path, log: &Path) -> Outcome<ExitCode> {
    let script: Value = serde_json::from_slice(&std::fs::read(script)?)?;
    let append = |record: &Value| -> Outcome {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        writeln!(file, "{record}")?;
        Ok(())
    };
    let env: BTreeMap<String, Option<String>> = script["record"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| (name.to_owned(), std::env::var(name).ok()))
        .collect();
    let cwd = std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned())?;
    append(&json!({"start": {"cwd": cwd, "env": env}}))?;
    let mut calls = Calls::default();
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line)?;
        append(&json!({"message": message}))?;
        match answer(&script, &mut calls, &message) {
            Answer::Respond(response, delay) => {
                std::thread::sleep(delay);
                let mut out = stdout.lock();
                writeln!(out, "{response}")?;
                out.flush()?;
            }
            Answer::Nothing => {}
            Answer::Crash => return Ok(ExitCode::from(3)),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A stdio server a world declared: the command that starts it and its log.
#[derive(Debug, Clone)]
pub struct StdioMcp {
    name: String,
    command: String,
    args: Vec<String>,
    log: PathBuf,
}

impl StdioMcp {
    /// The `[[mcp_servers]]` entry that starts this server, with `extra` TOML
    /// lines added to it.
    pub fn toml(&self, extra: &str) -> String {
        self.toml_with_env(extra, &[])
    }

    /// [`Self::toml`] with an `env` table holding `env`.
    pub fn toml_with_env(&self, extra: &str, env: &[(&str, &str)]) -> String {
        let quote = |text: &str| Value::String(text.to_owned()).to_string();
        let mut entry = format!(
            "[[mcp_servers]]\nname = {}\ntransport = \"stdio\"\ncommand = {}\nargs = {}\n{extra}\n",
            quote(&self.name),
            quote(&self.command),
            Value::from(self.args.clone())
        );
        if !env.is_empty() {
            let env: Vec<String> = env
                .iter()
                .map(|(name, value)| format!("{name} = {}", quote(value)))
                .collect();
            entry.push_str(&format!("[mcp_servers.env]\n{}\n", env.join("\n")));
        }
        entry.push('\n');
        entry
    }

    /// The program that starts the server.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The arguments the program takes to serve this script.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Every record the server appended: one `start` per process, then one
    /// `message` per message it read.
    pub fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The messages the server read, across every process.
    pub fn messages(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter_map(|record| record.get("message").cloned())
            .collect()
    }

    /// The `start` record of each process the server ran as.
    pub fn starts(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter_map(|record| record.get("start").cloned())
            .collect()
    }
}

impl World {
    /// Declares a stdio MCP server named `name` that answers from `script`:
    /// the e2e binary running this trial, started again as a server.
    pub fn mcp_stdio(&self, name: &str, script: &Mcp) -> Outcome<StdioMcp> {
        let directory = format!("mcp/{name}");
        self.write(
            format!("{directory}/script.json"),
            script.to_json().to_string(),
        )?;
        let script_path = self.path(format!("{directory}/script.json"))?;
        let log = self.path(format!("{directory}/log.jsonl"))?;
        let command = std::env::current_exe()
            .map_err(|error| Failure(format!("cannot locate the e2e binary: {error}")))?;
        Ok(StdioMcp {
            name: name.to_owned(),
            command: command.to_string_lossy().into_owned(),
            args: vec![
                SERVER_ARGUMENT.to_owned(),
                script_path.to_string_lossy().into_owned(),
                log.to_string_lossy().into_owned(),
            ],
            log,
        })
    }
}

#[cfg(test)]
mod mcp_tests;
