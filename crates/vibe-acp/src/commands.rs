//! The slash commands the adapter answers itself instead of running a turn.
//!
//! Reference `AcpCommandController` and `AcpCommandRegistry`
//! (`vibe/acp/commands/`): the built-in commands are advertised with the
//! user-invocable skills, a prompt whose first word names one is echoed and
//! answered here, and `/retry` hands a turn a prompt the user never typed.

mod teleport;

use std::sync::Arc;

use serde_json::{Map, Value, json};
use vibe_app_server::client::{ClientError, TurnDriver};

use crate::agent::AcpAgent;
use crate::agent::sessions::history_entries;
use crate::agent::turn::uuid;
use crate::protocol::AcpError;
use crate::session::AcpHarness;

/// What a command did with the prompt.
pub(crate) enum CommandOutcome {
    /// The prompt is answered.
    Response(Value),
    /// A turn runs on this text instead of the prompt.
    Injected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Help,
    Compact,
    Reload,
    Log,
    Mcp,
    Teleport,
    ProxySetup,
    Retry,
    Leanstall,
    Unleanstall,
    DataRetention,
}

struct Command {
    name: &'static str,
    description: &'static str,
    hint: Option<&'static str>,
    kind: Kind,
}

/// Reference `_build_commands`, in its declaration order.
const COMMANDS: [Command; 11] = [
    Command {
        name: "help",
        description: "List the commands and skills this session understands",
        hint: None,
        kind: Kind::Help,
    },
    Command {
        name: "compact",
        description: "Summarize the conversation so far to free context, guided by optional instructions",
        hint: Some("What the summary should keep or focus on"),
        kind: Kind::Compact,
    },
    Command {
        name: "reload",
        description: "Read the configuration, agent instructions, and skills from disk again",
        hint: None,
        kind: Kind::Reload,
    },
    Command {
        name: "log",
        description: "Show where this session's log is written",
        hint: None,
        kind: Kind::Log,
    },
    Command {
        name: "mcp",
        description: "Report MCP sign-in status, sign in to an MCP server, or sign out of one",
        hint: Some("status | login <alias> | logout <alias>"),
        kind: Kind::Mcp,
    },
    Command {
        name: "teleport",
        description: "Continue this session in Vibe Code on the web",
        hint: None,
        kind: Kind::Teleport,
    },
    Command {
        name: "proxy-setup",
        description: "Set up HTTP proxies and TLS certificates",
        hint: Some("KEY and a value to set it, KEY alone to clear it, nothing for help"),
        kind: Kind::ProxySetup,
    },
    Command {
        name: "retry",
        description: "Pick up a model response that was cut off, with optional extra guidance",
        hint: Some("Extra guidance for the continuation"),
        kind: Kind::Retry,
    },
    Command {
        name: "leanstall",
        description: "Add the Lean 4 agent (leanstral)",
        hint: None,
        kind: Kind::Leanstall,
    },
    Command {
        name: "unleanstall",
        description: "Remove the Lean 4 agent",
        hint: None,
        kind: Kind::Unleanstall,
    },
    Command {
        name: "data-retention",
        description: "Explain how conversation data is retained",
        hint: None,
        kind: Kind::DataRetention,
    },
];

/// The hint a skill is advertised with.
const SKILL_HINT: &str = "What the skill should do";

/// Reference `DATA_RETENTION_MESSAGE`, in this project's words.
const DATA_RETENTION_MESSAGE: &str = "## Data retention\n\n\
Prompts, responses, and tool output are sent to the model provider you configured, \
which keeps them according to its own retention policy and your plan's terms. \
Review the provider's privacy settings to opt out of training or shorten retention.\n\n\
Session transcripts and logs stay on this machine, under the Vibe home directory, until you delete them.";

fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// Reference `AcpCommandController.execute`: the first word of the prompt,
/// lowercased, names a built-in command or nothing happens here.
pub(crate) async fn execute<D>(
    agent: &Arc<AcpAgent<D>>,
    harness: &Arc<AcpHarness<D>>,
    text: &str,
    message_id: &str,
) -> Result<Option<CommandOutcome>, AcpError>
where
    D: TurnDriver + 'static,
{
    let trimmed = text.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let Some(name) = parts
        .next()
        .and_then(|word| word.strip_prefix('/'))
        .map(str::to_lowercase)
    else {
        return Ok(None);
    };
    let Some(command) = find(&name) else {
        return Ok(None);
    };
    let arguments = parts.next().map(str::trim_start).unwrap_or_default();
    agent.session_update(
        &harness.session_id,
        json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": text},
            "messageId": message_id,
        }),
    );
    agent
        .record(
            harness,
            "vibe.slash_command_used",
            json!({"command": name, "command_type": "builtin"}),
        )
        .await;
    let response = match command.kind {
        Kind::Help => agent.reply(harness, &agent.help_text(harness).await, None),
        Kind::Compact => agent.compact(harness, arguments).await?,
        Kind::Reload => agent.reload(harness).await,
        Kind::Log => agent.log(harness).await,
        Kind::Mcp => agent.mcp(harness, arguments).await,
        Kind::Teleport => agent.teleport(harness).await?,
        Kind::ProxySetup => agent.proxy_setup(harness, arguments).await?,
        Kind::Retry => {
            if agent.has_history(harness).await? {
                return Ok(Some(CommandOutcome::Injected(retry_prompt(arguments))));
            }
            agent.reply(harness, "There is no cut-off response to pick up.", None)
        }
        Kind::Leanstall => agent.set_lean(harness, true).await?,
        Kind::Unleanstall => agent.set_lean(harness, false).await?,
        Kind::DataRetention => agent.reply(harness, DATA_RETENTION_MESSAGE, None),
    };
    Ok(Some(CommandOutcome::Response(response)))
}

/// Reference `build_retry_prompt`: a harness note asking the model to carry
/// on where its response stopped, with the user's guidance appended.
fn retry_prompt(instructions: &str) -> String {
    let mut message = "The last model response stopped before it was finished. Resume it \
from the exact point where it stopped, without restating anything already written. If \
nothing was written yet, answer the waiting user request as usual."
        .to_owned();
    let instructions = instructions.trim();
    if !instructions.is_empty() {
        message.push_str("\n\nWhile continuing, also follow this guidance from the user:\n");
        message.push_str(instructions);
    }
    format!(
        "<{tag}>{message}</{tag}>",
        tag = vibe_core::workspace::WARNING_TAG
    )
}

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    /// Reference `send_commands`: the built-in commands and the skills a user
    /// can invoke, sorted by name.
    pub(crate) async fn send_commands(&self, harness: &AcpHarness<D>) -> Result<(), AcpError> {
        let mut commands = COMMANDS
            .iter()
            .map(|command| {
                let mut advertised = json!({
                    "name": command.name,
                    "description": command.description,
                });
                if let Some(hint) = command.hint {
                    advertised["input"] = json!({"hint": hint});
                }
                advertised
            })
            .collect::<Vec<_>>();
        commands.extend(self.invocable_skills(harness).await.into_iter().map(
            |(name, description)| {
                json!({
                    "name": name,
                    "description": description,
                    "input": {"hint": SKILL_HINT},
                })
            },
        ));
        commands.sort_by(|left, right| {
            let name = |command: &Value| {
                command
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            name(left).cmp(&name(right))
        });
        self.session_update(
            &harness.session_id,
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": commands,
            }),
        );
        Ok(())
    }

    /// Reference `message`: an agent message the command writes, under a
    /// message ID of its own.
    pub(crate) fn command_message(
        &self,
        harness: &AcpHarness<D>,
        text: &str,
        meta: Option<&Value>,
    ) {
        let mut update = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
            "messageId": uuid(),
        });
        if let Some(meta) = meta {
            update["_meta"] = meta.clone();
        }
        self.session_update(&harness.session_id, update);
    }

    /// Reference `_reply`: the message, and the prompt response that ends the
    /// command.
    fn reply(&self, harness: &AcpHarness<D>, text: &str, meta: Option<Value>) -> Value {
        self.command_message(harness, text, meta.as_ref());
        end_turn(meta)
    }

    /// The user-invocable skills that no built-in command shadows, as
    /// `(name, description)` sorted by name.
    async fn invocable_skills(&self, harness: &AcpHarness<D>) -> Vec<(String, String)> {
        let listing = self
            .call(harness, "skills/list", json!({}))
            .await
            .unwrap_or_default();
        let mut skills = listing
            .get("skills")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|skill| skill.get("userInvocable").and_then(Value::as_bool) == Some(true))
            .filter_map(|skill| {
                let name = skill.get("name")?.as_str()?;
                (find(name).is_none()).then(|| {
                    (
                        name.to_owned(),
                        skill
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )
                })
            })
            .collect::<Vec<_>>();
        skills.sort();
        skills
    }

    /// Reference `_record_skill_command`: a prompt that invokes a skill is
    /// recorded as a skill command.
    pub(crate) async fn record_skill_command(&self, harness: &AcpHarness<D>, text: &str) {
        let Some(name) = text
            .trim()
            .split(' ')
            .next()
            .and_then(|word| word.strip_prefix('/'))
            .map(str::to_lowercase)
        else {
            return;
        };
        let skill = self
            .invocable_skills_with_builtins(harness)
            .await
            .into_iter()
            .find(|skill| skill.to_lowercase() == name);
        if let Some(skill) = skill {
            self.record(
                harness,
                "vibe.slash_command_used",
                json!({"command": skill, "command_type": "skill"}),
            )
            .await;
        }
    }

    async fn invocable_skills_with_builtins(&self, harness: &AcpHarness<D>) -> Vec<String> {
        self.call(harness, "skills/list", json!({}))
            .await
            .unwrap_or_default()
            .get("skills")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|skill| skill.get("userInvocable").and_then(Value::as_bool) == Some(true))
            .filter_map(|skill| skill.get("name")?.as_str().map(ToOwned::to_owned))
            .collect()
    }

    /// Reference `_help`.
    async fn help_text(&self, harness: &AcpHarness<D>) -> String {
        let mut commands = COMMANDS.iter().collect::<Vec<_>>();
        commands.sort_by_key(|command| command.name);
        let mut lines = vec!["### Available Commands".to_owned(), String::new()];
        lines.extend(commands.iter().map(|command| {
            let hint = command
                .hint
                .map(|hint| format!(" `<{hint}>`"))
                .unwrap_or_default();
            format!("- `/{}`{hint}: {}", command.name, command.description)
        }));
        let skills = self.invocable_skills(harness).await;
        if !skills.is_empty() {
            lines.extend([
                "".to_owned(),
                "### Available Skills".to_owned(),
                String::new(),
            ]);
            lines.extend(
                skills
                    .iter()
                    .map(|(name, description)| format!("- `/{name}`: {description}")),
            );
        }
        lines.join("\n")
    }

    /// Whether the session's history holds anything.
    async fn has_history(&self, harness: &AcpHarness<D>) -> Result<bool, AcpError> {
        let state = self.session_state(harness).await?;
        Ok(!history_entries(&state).is_empty())
    }

    /// Reference `_compact`.
    async fn compact(
        &self,
        harness: &AcpHarness<D>,
        instructions: &str,
    ) -> Result<Value, AcpError> {
        if !self.has_history(harness).await? {
            return Ok(self.reply(
                harness,
                "The conversation has nothing to compact yet.",
                None,
            ));
        }
        let call_id = uuid();
        self.session_update(
            &harness.session_id,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": "Compacting the conversation history...",
                "kind": "other",
                "status": "in_progress",
                "content": [text_content(
                    "Context housekeeping that needs no approval. It can take a moment..."
                )],
            }),
        );
        let session_id = harness.canonical_id();
        let result = harness
            .service
            .lock()
            .await
            .compact(&session_id, instructions)
            .await;
        if let Err(error) = result {
            let message = error.to_string();
            self.session_update(
                &harness.session_id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "title": "Compaction failed",
                    "status": "failed",
                    "rawOutput": message,
                }),
            );
            return Err(AcpError::Internal(message));
        }
        self.session_update(
            &harness.session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "title": "Compacted the conversation history",
                "status": "completed",
                "content": [text_content("The conversation context was compacted")],
            }),
        );
        Ok(end_turn(None))
    }

    /// Reference `_reload`.
    async fn reload(&self, harness: &AcpHarness<D>) -> Value {
        if let Err(error) = self
            .call(harness, "config/reload", json!({"reloadRuntime": true}))
            .await
        {
            return self.reply(
                harness,
                &format!("The configuration could not be reloaded: {error}"),
                None,
            );
        }
        if let Err(error) = self.send_commands(harness).await {
            return self.reply(
                harness,
                &format!(
                    "The configuration was reloaded, but the new command list could not be sent: {error}"
                ),
                None,
            );
        }
        self.reply(
            harness,
            "Reloaded the configuration, agent instructions, and skills.",
            None,
        )
    }

    /// Reference `_log`.
    async fn log(&self, harness: &AcpHarness<D>) -> Value {
        let runtime = self
            .call(harness, "runtime/read", json!({}))
            .await
            .unwrap_or_default();
        let log = runtime.get("sessionLog").cloned().unwrap_or(Value::Null);
        let path = log
            .get("path")
            .and_then(Value::as_str)
            .filter(|_| log.get("enabled").and_then(Value::as_bool) == Some(true));
        let message = match path {
            Some(path) => format!(
                "## Current Log Directory\n\n`{path}`\n\nShare this directory to pass the interaction along."
            ),
            None => "The configuration turns session logging off.".to_owned(),
        };
        self.reply(harness, &message, None)
    }

    /// Reference `_mcp`.
    async fn mcp(&self, harness: &AcpHarness<D>, arguments: &str) -> Value {
        let mut parts = arguments.trim().splitn(2, char::is_whitespace);
        let subcommand = parts
            .next()
            .filter(|word| !word.is_empty())
            .map_or_else(|| "status".to_owned(), str::to_lowercase);
        let alias = parts.next().map(str::trim).unwrap_or_default();
        match (subcommand.as_str(), alias.is_empty()) {
            ("status", false) => self.reply(harness, "Usage: `/mcp status`", None),
            ("status", true) => self.mcp_status(harness).await,
            ("logout", false) => self.mcp_logout(harness, alias).await,
            ("logout", true) => self.reply(harness, "Usage: `/mcp logout <alias>`", None),
            ("login", false) => self.mcp_login(harness, alias).await,
            ("login", true) => self.reply(harness, "Usage: `/mcp login <alias>`", None),
            _ => self.reply(
                harness,
                "Usage: `/mcp status`, `/mcp login <alias>`, or `/mcp logout <alias>`",
                None,
            ),
        }
    }

    async fn mcp_sources(&self, harness: &AcpHarness<D>) -> Result<Vec<Value>, AcpError> {
        let dispatch = self.call_async(harness, "mcp/read", json!({})).await?;
        Ok(dispatch
            .result
            .get("mcp")
            .and_then(|mcp| mcp.get("sources"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    async fn mcp_status(&self, harness: &AcpHarness<D>) -> Value {
        let mut sources = match self.mcp_sources(harness).await {
            Ok(sources) => sources,
            Err(error) => return self.reply(harness, &app_server_message(&error), None),
        };
        if sources.is_empty() {
            return self.reply(
                harness,
                "There are no MCP servers in the configuration.",
                None,
            );
        }
        let field = |source: &Value, key: &str| {
            source
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        sources.sort_by_key(|source| field(source, "name"));
        let mut lines = vec!["### MCP auth status".to_owned(), String::new()];
        lines.extend(sources.iter().map(|source| {
            format!(
                "- `{}`: `{}`",
                field(source, "name"),
                field(source, "status")
            )
        }));
        self.reply(harness, &lines.join("\n"), None)
    }

    async fn mcp_login(&self, harness: &AcpHarness<D>, alias: &str) -> Value {
        let known = match self.mcp_sources(harness).await {
            Ok(sources) => sources
                .iter()
                .any(|source| source.get("name").and_then(Value::as_str) == Some(alias)),
            Err(error) => return self.reply(harness, &app_server_message(&error), None),
        };
        if !known {
            return self.reply(harness, &format!("No MCP server is named `{alias}`"), None);
        }
        match self
            .call_async(harness, "mcp/login", json!({"name": alias}))
            .await
        {
            Ok(dispatch) => {
                for notification in &dispatch.notifications {
                    if notification.method != "mcp/authUrl" {
                        continue;
                    }
                    if let Some(url) = notification.params.get("url").and_then(Value::as_str) {
                        self.command_message(
                            harness,
                            &format!("Sign in to MCP server `{alias}` at {url}"),
                            None,
                        );
                    }
                }
                self.reply(
                    harness,
                    &format!("Signed in to MCP server `{alias}`."),
                    None,
                )
            }
            Err(error) => self.reply(harness, &app_server_message(&error), None),
        }
    }

    async fn mcp_logout(&self, harness: &AcpHarness<D>, alias: &str) -> Value {
        match self
            .call_async(harness, "mcp/logout", json!({"name": alias}))
            .await
        {
            Ok(_) => self.reply(
                harness,
                &format!("Signed out of MCP server `{alias}`."),
                None,
            ),
            Err(error) => self.reply(harness, &app_server_message(&error), None),
        }
    }

    /// Reference `_proxy_setup`.
    async fn proxy_setup(
        &self,
        harness: &AcpHarness<D>,
        arguments: &str,
    ) -> Result<Value, AcpError> {
        let settings = self
            .call(harness, "config/proxy/read", json!({}))
            .await?
            .remove("settings")
            .unwrap_or(Value::Null);
        if arguments.is_empty() {
            return Ok(self.reply(harness, &proxy_help(&settings), None));
        }
        let mut parts = arguments.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or_default().to_uppercase();
        let value = parts.next().unwrap_or_default().trim();
        let known = settings
            .get("values")
            .and_then(Value::as_object)
            .is_some_and(|values| values.contains_key(&key));
        if !known {
            return Ok(self.reply(
                harness,
                &format!("Error: `{key}` is not a proxy variable this command manages"),
                None,
            ));
        }
        let resolved = (!value.is_empty()).then_some(value);
        let mut changes = Map::new();
        changes.insert(
            key.clone(),
            resolved.map_or(Value::Null, |value| json!(value)),
        );
        if let Err(error) = self
            .call(harness, "config/proxy/write", json!({"changes": changes}))
            .await
        {
            return Ok(self.reply(
                harness,
                &format!("Error: {}", app_server_message(&error)),
                None,
            ));
        }
        let message = match resolved {
            None => format!(
                "Removed `{key}` from ~/.vibe/.env\n\nStart a new chat to apply the change."
            ),
            Some(value) => format!(
                "Wrote `{key}={value}` to ~/.vibe/.env\n\nStart a new chat to apply the change."
            ),
        };
        Ok(self.reply(harness, &message, None))
    }

    /// Reference `_set_lean`.
    async fn set_lean(&self, harness: &AcpHarness<D>, installed: bool) -> Result<Value, AcpError> {
        let method = if installed {
            "agents/install"
        } else {
            "agents/uninstall"
        };
        self.call(harness, method, json!({"agentName": "lean"}))
            .await
            .map_err(|error| AcpError::InvalidParams(app_server_message(&error)))?;
        self.send_config_options(harness).await?;
        let message = if installed {
            "The Lean agent is installed."
        } else {
            "The Lean agent is uninstalled."
        };
        Ok(self.reply(harness, message, None))
    }
}

/// Reference `get_proxy_help_text`.
fn proxy_help(settings: &Value) -> String {
    let mut lines = vec![
        "## Proxy Configuration".to_owned(),
        String::new(),
        "Proxy and TLS settings apply to every HTTP request Vibe makes.".to_owned(),
        String::new(),
        "### Usage:".to_owned(),
        "- `/proxy-setup`: show this help and the current values".to_owned(),
        "- `/proxy-setup KEY value`: set a variable".to_owned(),
        "- `/proxy-setup KEY`: clear a variable".to_owned(),
        String::new(),
        "### Supported Variables:".to_owned(),
    ];
    if let Some(descriptions) = settings.get("descriptions").and_then(Value::as_object) {
        lines.extend(descriptions.iter().map(|(key, description)| {
            format!("- `{key}`: {}", description.as_str().unwrap_or_default())
        }));
    }
    lines.push(String::new());
    lines.push("### Current Settings:".to_owned());
    let configured = settings
        .get("values")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(|value| format!("- `{key}={value}`"))
        })
        .collect::<Vec<_>>();
    if configured.is_empty() {
        lines.push("- (nothing set)".to_owned());
    } else {
        lines.extend(configured);
    }
    lines.join("\n")
}

/// The message an app-server error carries, which is what the reference
/// reads off `AppServerResponseError.error.message`.
fn app_server_message(error: &AcpError) -> String {
    match error {
        AcpError::AppServer(ClientError::Protocol(_, message)) => message.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

/// A command's prompt response: no usage, since no model ran.
fn end_turn(meta: Option<Value>) -> Value {
    let mut response = json!({"stopReason": "end_turn"});
    if let Some(meta) = meta {
        response["_meta"] = meta;
    }
    response
}
