//! Built-in slash commands handled by the adapter instead of the model.

pub(crate) mod mcp;
pub(crate) mod proxy;
pub(crate) mod teleport;

use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::TurnDriver;

use crate::agent::AcpAgent;
use crate::history::history_page;
use crate::protocol::{AcpError, AcpPromptResponse, AcpUsage};
use crate::session::{AcpHarness, ActivePhase, now_millis};
use crate::updates::{UpdateSender, queue_update, text_chunk};

pub(crate) struct CommandInfo {
    pub(crate) name: &'static str,
    description: &'static str,
    hint: Option<&'static str>,
}

const COMMANDS: &[CommandInfo] = &[
    CommandInfo {
        name: "compact",
        description: "Compact conversation history by summarizing it",
        hint: Some("Optional instructions for the summary"),
    },
    CommandInfo {
        name: "data-retention",
        description: "Show data-retention information",
        hint: None,
    },
    CommandInfo {
        name: "help",
        description: "Show available commands and keyboard shortcuts",
        hint: None,
    },
    CommandInfo {
        name: "log",
        description: "Show session logging status",
        hint: None,
    },
    CommandInfo {
        name: "mcp",
        description: "Show MCP status, login guidance, or log out a server",
        hint: Some("status | login <alias> | logout <alias>"),
    },
    CommandInfo {
        name: "proxy-setup",
        description: "Configure proxy and TLS certificate settings",
        hint: Some("KEY value to set, KEY to unset, or empty for help"),
    },
    CommandInfo {
        name: "reload",
        description: "Reload configuration, agent instructions, and skills",
        hint: None,
    },
    CommandInfo {
        name: "teleport",
        description: "Teleport session to Vibe Code Web",
        hint: None,
    },
];

/// `teleport` only exists when the cloud service is configured.
pub(crate) fn catalog(vibe_code_enabled: bool) -> impl Iterator<Item = &'static CommandInfo> {
    COMMANDS
        .iter()
        .filter(move |command| vibe_code_enabled || command.name != "teleport")
}

pub(crate) fn advertised(vibe_code_enabled: bool) -> Vec<Value> {
    catalog(vibe_code_enabled)
        .map(|command| {
            let mut encoded = json!({
                "name": command.name,
                "description": command.description,
            });
            if let Some(hint) = command.hint {
                encoded["input"] = json!({"hint": hint});
            }
            encoded
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltinCommand {
    name: String,
    arguments: String,
    raw: String,
}

/// Recognizes a single-block prompt that is exactly one advertised command.
pub(crate) fn builtin_command(
    content: &[Value],
    vibe_code_enabled: bool,
) -> Option<BuiltinCommand> {
    let [block] = content else {
        return None;
    };
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let raw = block.get("text").and_then(Value::as_str)?.trim();
    let mut parts = raw.strip_prefix('/')?.splitn(2, char::is_whitespace);
    let name = parts.next()?.to_ascii_lowercase();
    catalog(vibe_code_enabled).find(|command| command.name == name)?;
    Some(BuiltinCommand {
        name,
        arguments: parts.next().map(str::trim).unwrap_or_default().to_owned(),
        raw: raw.to_owned(),
    })
}

pub(crate) fn command_response() -> AcpPromptResponse {
    AcpPromptResponse {
        stop_reason: "end_turn".to_owned(),
        usage: AcpUsage {
            total_tokens: 0,
            input_tokens: 0,
            output_tokens: 0,
        },
    }
}

/// Streams command output for one session. Locally minted identifiers come
/// from the harness counter so updates emitted in the same millisecond stay
/// distinct.
pub(crate) struct CommandSink<'a, D>
where
    D: TurnDriver,
{
    pub(crate) harness: &'a Arc<AcpHarness<D>>,
    sender: &'a UpdateSender,
}

impl<D> CommandSink<'_, D>
where
    D: TurnDriver,
{
    pub(crate) fn session_id(&self) -> &str {
        &self.harness.session_id
    }

    fn chunk(&self, kind: &str, text: &str) -> Result<(), AcpError> {
        queue_update(
            self.sender,
            self.session_id(),
            text_chunk(kind, &format!("command-{}", now_millis()), text),
        )
    }

    fn echo_user(&self, text: &str) -> Result<(), AcpError> {
        self.chunk("user_message_chunk", text)
    }

    pub(crate) fn reply(&self, text: &str) -> Result<(), AcpError> {
        self.chunk("agent_message_chunk", text)
    }

    pub(crate) fn tool_update(&self, update: Value) -> Result<(), AcpError> {
        queue_update(self.sender, self.session_id(), update)
    }
}

pub(crate) async fn execute<D>(
    agent: &AcpAgent<D>,
    harness: &Arc<AcpHarness<D>>,
    sender: &UpdateSender,
    command: &BuiltinCommand,
) -> Result<AcpPromptResponse, AcpError>
where
    D: TurnDriver,
{
    harness.begin(ActivePhase::Builtin)?;
    harness.clear_cancel();
    let result = run(agent, &CommandSink { harness, sender }, command).await;
    harness.release()?;
    result
}

async fn run<D>(
    agent: &AcpAgent<D>,
    sink: &CommandSink<'_, D>,
    command: &BuiltinCommand,
) -> Result<AcpPromptResponse, AcpError>
where
    D: TurnDriver,
{
    sink.echo_user(&command.raw)?;
    match command.name.as_str() {
        "help" => sink.reply(&help_text(agent.vibe_code_enabled()))?,
        "compact" => compact(sink, &command.arguments).await?,
        "reload" => reload(sink).await?,
        "log" => sink.reply("Session logging paths are not exposed by this configuration.")?,
        "mcp" => {
            let reply = mcp::execute(sink, &command.arguments).await;
            sink.reply(&reply)?;
        }
        "teleport" if agent.vibe_code_enabled() => teleport::execute(agent, sink).await?,
        "proxy-setup" => {
            let reply = proxy::execute(sink, &command.arguments).await;
            sink.reply(&reply)?;
        }
        "data-retention" => sink.reply(
            "Data retention is controlled by the configured model provider and any connected \
             services. Local session transcripts remain under the configured Vibe session \
             directory.",
        )?,
        unknown => {
            return Err(AcpError::InvalidParams(format!(
                "unknown built-in command `{unknown}`"
            )));
        }
    }
    Ok(command_response())
}

fn help_text(vibe_code_enabled: bool) -> String {
    let lines = catalog(vibe_code_enabled)
        .map(|command| format!("- `/{}`: {}", command.name, command.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!("### Available Commands\n\n{lines}")
}

async fn compact<D>(sink: &CommandSink<'_, D>, instructions: &str) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    let mut service = sink.harness.service.lock().await;
    if history_page(&mut service, sink.session_id(), 0, 1)?.is_empty() {
        return sink.reply("No conversation history to compact yet.");
    }
    let call_id = sink.harness.next_local_id("compact");
    sink.tool_update(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": "Compacting conversation context",
        "status": "in_progress",
    }))?;
    if let Err(error) = service.compact(sink.session_id(), instructions).await {
        sink.tool_update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "title": "Compaction failed",
            "status": "failed",
            "rawOutput": error.to_string(),
        }))?;
        return Err(error.into());
    }
    sink.tool_update(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "title": "Conversation context compacted",
        "status": "completed",
    }))
}

async fn reload<D>(sink: &CommandSink<'_, D>) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    let outcome = sink
        .harness
        .service
        .lock()
        .await
        .public_call("config/reload", json!({}));
    match outcome {
        Ok(_) => sink.reply("Configuration reloaded (including agent instructions and skills)."),
        Err(error) => sink.reply(&format!("Failed to reload configuration: {error}")),
    }
}
