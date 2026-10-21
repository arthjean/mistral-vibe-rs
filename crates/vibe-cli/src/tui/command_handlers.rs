//! What each slash command does once dispatch has decided it runs.
//!
//! Reference `_handle_command` (`vibe/cli/textual_ui/app.py`) and the handler
//! each registry entry names. Every handler is written once, against
//! [`CommandBackend`], and says what it does as a sequence of [`Effect`]s and
//! backend calls. The terminal client runs them against the live session
//! (`workflow/live_commands.rs`); the command corpus replays them against the
//! fixtures the reference was driven with (`commands_parity_tests/replay.rs`),
//! so what a handler does is compared with the reference rather than asserted
//! about.
//!
//! The operator-facing lines are the command's contract and are reproduced as
//! observed. The exceptions are prose `NOTICE` keeps out of this repository:
//! the help document, which [`super::help`] writes, and the continuation
//! `/retry` submits, which [`retry_prompt`] writes.

use std::collections::BTreeMap;

use vibe_core::storage::short_session_id;
use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::TelemetryCommandKind;
use vibe_core::workspace::WARNING_TAG;

use super::commands::{CommandContext, CommandId, ParsedCommand, command_echo, command_name};
use super::help;
use super::interaction::IntegrationKind;
use super::plugins::{self, PluginCatalog, PluginsPort, ReloadOutcome};

pub(super) mod log_level;
pub(super) mod loops;
pub(super) mod mcp;
mod mcp_arguments;
pub(super) mod status;

pub(in crate::tui) use mcp_arguments::AddArguments as McpAddArguments;

#[cfg(test)]
#[path = "command_handlers/command_handlers_tests.rs"]
mod command_handlers_tests;

/// A bottom panel or screen a command opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Panel {
    Config,
    Model,
    Skills,
    Thinking,
    LogLevel,
    DebugConsole,
    RemoteProjects,
    ProxySetup,
    Sessions,
    Mcp,
    ConnectorAuth,
    Voice,
    Rewind,
    Theme,
    Todos,
}

impl Panel {
    /// The name the command corpus records the panel under.
    #[cfg(test)]
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Model => "model",
            Self::Skills => "skills",
            Self::Thinking => "thinking",
            Self::LogLevel => "logLevel",
            Self::DebugConsole => "debugConsole",
            Self::RemoteProjects => "remoteProjects",
            Self::ProxySetup => "proxySetup",
            Self::Sessions => "sessions",
            Self::Mcp => "mcp",
            Self::ConnectorAuth => "connectorAuth",
            Self::Voice => "voice",
            Self::Rewind => "rewind",
            Self::Theme => "theme",
            Self::Todos => "todos",
        }
    }
}

/// How a transient notification presents. Reference `App.notify` severities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NotifySeverity {
    Information,
    Warning,
}

/// What a handler shows the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Effect {
    /// The command line, mounted above what the command writes.
    Echo(String),
    /// A Markdown message in the transcript. Reference `UserCommandMessage`.
    Message(String),
    /// Reference `ErrorMessage`.
    Error(String),
    /// Reference `WarningMessage`.
    Warning(String),
    /// A progress line that settled. Reference `ReloadConfigMessage` and
    /// `CompactMessage`, and the branch confirmation.
    Status {
        text: String,
        ok: bool,
    },
    /// A transient notification that never enters the transcript.
    Notify {
        severity: NotifySeverity,
        text: String,
    },
    Panel {
        panel: Panel,
        initial: Option<String>,
    },
    ClosePanel,
    /// Takes the command line back out of the transcript.
    RemoveEcho,
    /// Drops every widget the transcript shows.
    ResetTranscript,
    /// Starts a model turn with `text`.
    Submit {
        text: String,
        injected: bool,
    },
    /// Pastes the clipboard image, saying so when there is none.
    ClipboardImage,
    Exit,
    Teleport {
        target: String,
        echo: bool,
    },
    /// The session picker received its list.
    SessionsLoaded(usize),
    /// The session title the header and the terminal show changed.
    Title(String),
}

/// What `/reload` reports about the configuration it reread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Reloaded {
    /// Images earlier turns carried that the new model cannot read.
    pub stripped_images: u64,
    pub model_display_name: String,
}

/// Where the session is written, and whether it has been. Reference
/// `SessionLogView`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionLog {
    pub enabled: bool,
    pub persisted: bool,
    pub path: String,
}

/// The counters `/status` prints. Reference `AgentStatsSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Stats {
    pub steps: u64,
    pub session_prompt_tokens: u64,
    pub session_cached_tokens: u64,
    pub session_completion_tokens: u64,
    pub session_total_llm_tokens: u64,
    pub last_turn_total_tokens: u64,
    pub last_turn_cached_tokens: u64,
    pub session_cost: f64,
}

/// The active model's provider as `/status` shows it. Reference
/// `ProviderAuthView`: already redacted by the server, so it carries no
/// credential.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ProviderAuth {
    pub model_display_name: String,
    pub provider_name: String,
    /// The sanitized API base, or `None` when it cannot be shown safely.
    pub api_base: Option<String>,
}

/// The signed-in caller. Reference `IdentityView`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Identity {
    pub name: Option<String>,
    pub email: Option<String>,
    pub workspace: Option<String>,
    pub organization: Option<String>,
}

/// What `/mcp` reads before it decides. Reference `MCPState`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct McpState {
    pub sources: Vec<(String, IntegrationKind)>,
    pub connector_error: Option<String>,
    pub statuses: BTreeMap<String, String>,
}

/// How far a server login got by the time the backend answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct McpLogin {
    /// The authorization URLs the login published, in order.
    pub urls: Vec<String>,
    /// Whether the login finished. A backend whose login completes later
    /// reports the completion itself.
    pub completed: bool,
}

/// One scheduled loop. Reference `ScheduledLoop`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ScheduledLoop {
    pub id: String,
    pub prompt: String,
    pub interval_seconds: u64,
    pub next_fire_at: f64,
}

/// Everything a handler reads from or asks of the session.
///
/// A call that fails answers the message the reference prints for it, which is
/// the server's own error message.
#[allow(async_fn_in_trait)]
pub(super) trait CommandBackend {
    /// Shows one effect.
    fn emit(&mut self, effect: Effect);
    fn report(&mut self, record: TelemetryRecord);
    fn session_id(&self) -> String;
    /// Whether a model turn is running. Reference `_agent_job_active`.
    fn turn_active(&self) -> bool;
    fn session_log(&self) -> SessionLog;
    async fn reload(&mut self) -> Result<Reloaded, String>;
    /// Starts a new conversation, keeping the current one resumable.
    async fn clear_history(&mut self) -> Result<(), String>;
    fn last_assistant_message(&self) -> Option<String>;
    /// Copies `text`, answering whether the copy was verified.
    fn copy_text(&mut self, text: &str) -> bool;
    fn history_is_empty(&self) -> bool;
    async fn compact(&mut self, instructions: &str) -> Result<(), String>;
    fn stats(&mut self) -> Stats;
    /// Whether the session runs the unified mode. Reference
    /// `runtime.experimental_harness`, which `/status` reads apart from the
    /// registry's context.
    fn experimental_harness(&self) -> bool;
    /// Reference `ProviderAuthResource.read`, `providerAuth/read`.
    async fn provider_auth(&mut self) -> Result<ProviderAuth, String>;
    async fn identity(&mut self) -> Result<Option<Identity>, String>;
    async fn account_plan(&mut self) -> Result<Option<String>, String>;
    async fn open_projects(&mut self) -> Result<(), String>;
    async fn saved_sessions(&mut self) -> usize;
    /// Renames the session, answering the title it now carries.
    async fn rename(&mut self, title: &str) -> Result<String, String>;
    async fn mcp_state(&mut self) -> McpState;
    async fn mcp_login(&mut self, alias: &str) -> Result<McpLogin, String>;
    async fn mcp_logout(&mut self, alias: &str) -> Result<(), String>;
    /// Adds an OAuth server, answering its name and whether it is new.
    async fn mcp_add(&mut self, arguments: &McpAddArguments) -> Result<(String, bool), String>;
    fn open_url(&mut self, url: &str);
    fn has_todos(&self) -> bool;
    async fn agent_names(&mut self) -> Vec<String>;
    async fn set_agent_installed(&mut self, name: &str, installed: bool) -> Result<(), String>;
    /// Copies the session to a new identifier without leaving it, answering
    /// the copy's identifier.
    async fn fork(&mut self) -> Result<String, String>;
    fn retry_offered(&self) -> bool;
    fn now(&self) -> f64;
    async fn loops_list(&mut self) -> Result<Vec<ScheduledLoop>, String>;
    async fn loops_create(&mut self, interval: &str, prompt: &str)
    -> Result<ScheduledLoop, String>;
    async fn loops_delete(&mut self, id: &str) -> Result<ScheduledLoop, String>;
    async fn loops_clear(&mut self) -> Result<u64, String>;
    /// The plugin catalog, or `None` from a backend that resolves no plugins.
    /// Reference `PluginCatalogResource.read`.
    fn plugin_catalog(&mut self) -> Result<Option<PluginCatalog>, String>;
    /// Re-pins the session's plugins. Reference `plugin/reload`.
    fn reload_plugins(&mut self) -> Result<(), String>;
    /// Opens the plugins panel on `catalog`.
    fn open_plugins(&mut self, catalog: PluginCatalog);
}

/// A handler's backend as the two calls the plugin commands make.
struct BackendPlugins<'a, B>(&'a mut B);

impl<B: CommandBackend> PluginsPort for BackendPlugins<'_, B> {
    fn read_catalog(&mut self) -> Result<Option<PluginCatalog>, String> {
        self.0.plugin_catalog()
    }

    fn reload(&mut self) -> Result<(), String> {
        self.0.reload_plugins()
    }
}

/// Reference `_show_plugins`: a backend that resolves no plugins, or one
/// with nothing installed, says so; otherwise the catalog opens.
fn show_plugins<B: CommandBackend>(backend: &mut B) {
    match backend.plugin_catalog() {
        Err(error) => backend.emit(Effect::Error(error)),
        Ok(None) => backend.emit(Effect::Message(plugins::NO_PLUGINS.to_owned())),
        Ok(Some(catalog)) if catalog.is_empty() => {
            backend.emit(Effect::Message(plugins::NOTHING_INSTALLED.to_owned()));
        }
        Ok(Some(catalog)) => {
            backend.emit(Effect::Message(plugins::OPENED.to_owned()));
            backend.open_plugins(catalog);
        }
    }
}

/// Reference `_reload_plugins`: what the re-pin moved, by digest.
fn reload_plugins<B: CommandBackend>(backend: &mut B) {
    let outcome = plugins::reload_plugins(&mut BackendPlugins(&mut *backend));
    backend.emit(match outcome {
        ReloadOutcome::NoPlugins => Effect::Message(plugins::NO_PLUGINS.to_owned()),
        ReloadOutcome::Failed(error) => Effect::Error(plugins::reload_failure(&error)),
        ReloadOutcome::Reloaded { report, .. } => Effect::Message(report),
    });
}

/// Reference `_handle_command`: the event is reported under the registry key,
/// the line is echoed, and the handler runs.
pub(super) async fn run<B: CommandBackend>(
    line: &str,
    parsed: &ParsedCommand<'_>,
    context: &CommandContext,
    backend: &mut B,
) {
    let id = parsed.id;
    backend.report(TelemetryRecord::SlashCommandUsed {
        command: command_name(id).to_owned(),
        kind: TelemetryCommandKind::Builtin,
    });
    backend.emit(Effect::Echo(command_echo(line, id)));
    let arguments = parsed.arguments;
    match id {
        CommandId::Help => backend.emit(Effect::Message(help::document(context))),
        CommandId::Config => open(backend, Panel::Config),
        CommandId::Model => open(backend, Panel::Model),
        CommandId::Skills => {
            backend.emit(Effect::Message("Skills browser opened.".to_owned()));
            open(backend, Panel::Skills);
        }
        CommandId::Thinking => open(backend, Panel::Thinking),
        CommandId::Reload => reload(backend).await,
        CommandId::Clear => clear(arguments, backend).await,
        CommandId::Copy => copy(backend),
        CommandId::PasteImage => backend.emit(Effect::ClipboardImage),
        CommandId::Log => log(backend),
        CommandId::LogLevel => open(backend, Panel::LogLevel),
        CommandId::Debug => open(backend, Panel::DebugConsole),
        CommandId::Compact => compact(arguments, backend).await,
        CommandId::Exit => backend.emit(Effect::Exit),
        CommandId::Status => status::show(backend).await,
        CommandId::Whoami => status::whoami(backend).await,
        // Reference `_teleport_command` ignores its arguments: a teleport with
        // a prompt is typed as `&prompt`, which dispatch routes itself.
        CommandId::Teleport => backend.emit(Effect::Teleport {
            target: String::new(),
            echo: false,
        }),
        CommandId::RemoteProject => match backend.open_projects().await {
            Ok(()) => open(backend, Panel::RemoteProjects),
            Err(error) => backend.emit(Effect::Error(error)),
        },
        CommandId::ProxySetup => open(backend, Panel::ProxySetup),
        CommandId::Resume => resume(backend).await,
        CommandId::Rename => rename(arguments, backend).await,
        CommandId::Mcp => mcp::handle(arguments, backend).await,
        CommandId::Plugins => show_plugins(backend),
        CommandId::ReloadPlugins => reload_plugins(backend),
        CommandId::Todo => {
            if backend.has_todos() {
                open(backend, Panel::Todos);
            } else {
                backend.emit(Effect::Message("No todos yet.".to_owned()));
            }
        }
        CommandId::Voice => open(backend, Panel::Voice),
        CommandId::InstallLean => set_lean(backend, true).await,
        CommandId::UninstallLean => set_lean(backend, false).await,
        CommandId::Rewind => open(backend, Panel::Rewind),
        CommandId::Branch => branch(backend).await,
        CommandId::Retry => {
            if !backend.turn_active() && backend.retry_offered() {
                backend.emit(Effect::Submit {
                    text: retry_prompt(arguments),
                    injected: true,
                });
            }
        }
        CommandId::Loop => loops::handle(arguments, backend).await,
        CommandId::DataRetention => {
            backend.emit(Effect::Message(DATA_RETENTION_MESSAGE.to_owned()));
        }
        CommandId::Theme => open(backend, Panel::Theme),
    }
}

fn open<B: CommandBackend>(backend: &mut B, panel: Panel) {
    backend.emit(Effect::Panel {
        panel,
        initial: None,
    });
}

pub(super) const DATA_RETENTION_MESSAGE: &str = "\
## Your Data Helps Improve Mistral AI

At Mistral AI, we're committed to delivering the best possible experience. When you use Mistral models on our API, your interactions may be collected to improve our models, ensuring they stay cutting-edge, accurate, and helpful.

Manage your data settings [here](https://chat.mistral.ai/work?profile_dialog=privacy)";

const RELOADED: &str = "Configuration reloaded (includes agent instructions and skills).";

/// Reference `_reload_config`.
async fn reload<B: CommandBackend>(backend: &mut B) {
    match backend.reload().await {
        Ok(reloaded) => {
            backend.emit(Effect::Status {
                text: RELOADED.to_owned(),
                ok: true,
            });
            if reloaded.stripped_images > 0 {
                let noun = if reloaded.stripped_images == 1 {
                    "image"
                } else {
                    "images"
                };
                backend.emit(Effect::Warning(format!(
                    "{} {noun} from earlier turns will be omitted when sending to {} (no vision \
                     support).",
                    reloaded.stripped_images, reloaded.model_display_name
                )));
            }
        }
        Err(error) => backend.emit(Effect::Status {
            text: format!("Failed to reload config: {error}"),
            ok: false,
        }),
    }
}

/// Reference `_clear_history`: the conversation continues under a new session,
/// and the old one is named so it can be resumed, when it can be.
async fn clear<B: CommandBackend>(arguments: &str, backend: &mut B) {
    let previous = backend.session_id();
    let log = backend.session_log();
    let resumable = log.enabled && log.persisted;
    let prompt = arguments.trim();
    if let Err(error) = backend.clear_history().await {
        backend.emit(Effect::Error(format!("Failed to clear history: {error}")));
        return;
    }
    backend.emit(Effect::ResetTranscript);
    // The reset took the first echo with it, and the reference mounts the
    // registry key again rather than the alias the operator typed.
    backend.emit(Effect::Echo(command_name(CommandId::Clear).to_owned()));
    let message = if resumable {
        let short = short_session_id(&previous);
        format!(
            "New conversation started.\n\nPrevious session: `{short}`\nTo resume it later, run: \
             `vibe --resume {short}`"
        )
    } else {
        "New conversation started.".to_owned()
    };
    backend.emit(Effect::Message(message));
    if !prompt.is_empty() {
        backend.emit(Effect::Submit {
            text: prompt.to_owned(),
            injected: false,
        });
    }
}

/// Reference `NATIVE_COPY_HINT`, appended when the copy could not be verified.
pub(super) const NATIVE_COPY_HINT: &str = "if paste fails, hold Shift (Option in iTerm2, Fn in Terminal.app) while selecting for native \
     copy";

/// Reference `_copy_last_agent_message`.
fn copy<B: CommandBackend>(backend: &mut B) {
    let Some(content) = backend.last_assistant_message() else {
        backend.emit(Effect::Notify {
            severity: NotifySeverity::Warning,
            text: "No agent message available to copy".to_owned(),
        });
        return;
    };
    let verified = backend.copy_text(&content);
    let success = "Last agent message copied to clipboard";
    backend.emit(Effect::Notify {
        severity: NotifySeverity::Information,
        text: if verified {
            success.to_owned()
        } else {
            format!("{success} · {NATIVE_COPY_HINT}")
        },
    });
    backend.report(TelemetryRecord::UserCopiedText {
        text_length: content.chars().count() as u64,
    });
}

/// Reference `_show_log_path`.
fn log<B: CommandBackend>(backend: &mut B) {
    let log = backend.session_log();
    if !log.enabled {
        backend.emit(Effect::Error(
            "Session logging is disabled in configuration.".to_owned(),
        ));
    } else if !log.persisted {
        backend.emit(Effect::Error(
            "The current session has not been persisted yet.".to_owned(),
        ));
    } else {
        backend.emit(Effect::Message(format!(
            "## Current Log Directory\n\n`{}`\n\nYou can send this directory to share your \
             interaction.",
            log.path
        )));
    }
}

/// Reference `_compact_history` and `_run_compact`.
async fn compact<B: CommandBackend>(arguments: &str, backend: &mut B) {
    if backend.turn_active() {
        backend.emit(Effect::Error(
            "Cannot compact while agent loop is processing. Please wait.".to_owned(),
        ));
        return;
    }
    if backend.history_is_empty() {
        backend.emit(Effect::Error(
            "No conversation history to compact yet.".to_owned(),
        ));
        return;
    }
    let status = match backend.compact(arguments.trim()).await {
        Ok(()) => Effect::Status {
            text: "Compaction completed.".to_owned(),
            ok: true,
        },
        Err(error) => Effect::Status {
            text: format!("Error: {error}"),
            ok: false,
        },
    };
    backend.emit(status);
}

/// Reference `_resume_session`: the command line leaves the transcript, the
/// picker opens while the list loads, and an empty list closes it again.
async fn resume<B: CommandBackend>(backend: &mut B) {
    backend.emit(Effect::RemoveEcho);
    backend.emit(Effect::Panel {
        panel: Panel::Sessions,
        initial: None,
    });
    let count = if backend.session_log().enabled {
        backend.saved_sessions().await
    } else {
        0
    };
    if count > 0 {
        backend.emit(Effect::SessionsLoaded(count));
    } else {
        backend.emit(Effect::ClosePanel);
        backend.emit(Effect::Message(
            "No sessions found for this directory.".to_owned(),
        ));
    }
}

/// Reference `_rename_session`.
async fn rename<B: CommandBackend>(arguments: &str, backend: &mut B) {
    let title = arguments.trim();
    if title.is_empty() {
        backend.emit(Effect::Error("Usage: /rename <title>".to_owned()));
        return;
    }
    match backend.rename(title).await {
        Ok(renamed) => {
            backend.emit(Effect::Title(renamed.clone()));
            backend.emit(Effect::Message(format!(
                "Session renamed to \"{renamed}\"."
            )));
        }
        Err(error) => backend.emit(Effect::Error(format!("Failed to rename session: {error}"))),
    }
}

/// Reference `_install_lean` and `_uninstall_lean`.
async fn set_lean<B: CommandBackend>(backend: &mut B, install: bool) {
    let present = backend
        .agent_names()
        .await
        .iter()
        .any(|name| name == "lean");
    if install && present {
        backend.emit(Effect::Message(
            "Lean agent is already installed.".to_owned(),
        ));
        return;
    }
    if !install && !present {
        backend.emit(Effect::Message("Lean agent is not installed.".to_owned()));
        return;
    }
    if let Err(error) = backend.set_agent_installed("lean", install).await {
        backend.emit(Effect::Error(error));
        return;
    }
    backend.emit(Effect::Message(
        if install {
            "Lean agent installed."
        } else {
            "Lean agent uninstalled."
        }
        .to_owned(),
    ));
}

/// Reference `_branch_session`.
async fn branch<B: CommandBackend>(backend: &mut B) {
    let source = backend.session_id();
    let copy = match backend.fork().await {
        Ok(copy) => copy,
        Err(error) => {
            backend.emit(Effect::Error(format!("Failed to branch session: {error}")));
            return;
        }
    };
    backend.report(TelemetryRecord::SessionBranched {
        source_session_id: source.clone(),
        new_session_id: copy.clone(),
    });
    let (short_source, short_copy) = (short_session_id(&source), short_session_id(&copy));
    backend.emit(Effect::Status {
        text: format!(
            "Branched to a new session.\nsession: {short_source} → {short_copy}\nResume the copy \
             with: vibe --resume {short_copy}"
        ),
        ok: true,
    });
}

/// The continuation `/retry` submits, in this port's own words.
///
/// The reference wraps the same three directives in the same warning tag
/// (`vibe/cli/commands.py:18`): resume where the stream broke, do not restate
/// what was already produced, and answer the pending request from the start when
/// nothing was. `NOTICE` forbids shipping its sentences, so these are original.
pub(super) fn retry_prompt(additional_instructions: &str) -> String {
    let mut message = "The previous model stream stopped before it finished. Pick the response up \
                       where it broke off, without restating anything already written. If nothing \
                       was written yet, answer the pending request from the start."
        .to_owned();
    let instructions = additional_instructions.trim();
    if !instructions.is_empty() {
        message.push_str(&format!(
            "\n\nApply these further instructions from the operator while continuing:\n\
             {instructions}"
        ));
    }
    format!("<{WARNING_TAG}>{message}</{WARNING_TAG}>")
}
