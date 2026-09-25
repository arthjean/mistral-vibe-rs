//! The slash-command handlers, run against the live session.
//!
//! [`super::super::command_handlers`] decides what each command does; this
//! module is how each decision reaches the session and the screen. What a
//! handler asks for that only the caller can do, starting a turn, pasting an
//! image or leaving, comes back as a [`FollowUp`].

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::PublicDispatch;
use vibe_core::events::{PublicHistoryEntry, PublicNoticeLevel};
use vibe_core::identity::{HttpIdentityGateway, IdentityCache};
use vibe_core::observability::{LogLevel, LogLevelChain, log_level_chain, set_session_log_level};
use vibe_core::telemetry::TelemetryRecord;

use super::super::chat_input::ChatInputState;
use super::super::clipboard::copy_text_verified;
use super::super::command_handlers::log_level::{self, Badge, LogLevelBackend, Picker};
use super::super::command_handlers::{
    CommandBackend, Effect, Identity, McpAddArguments, McpLogin, McpSourceKind, McpState, Panel,
    Reloaded, ScheduledLoop, SessionLog, Stats, session_cost,
};
use super::super::commands::{CommandContext, parse_command_in};
use super::super::controls::ControlState;
use super::super::interaction::{IntegrationKind, Overlay, OverlayItem, OverlayKind};
use super::super::pickers::{mcp_overlay, sessions_overlay, theme_overlay, thinking_overlay};
use super::super::remote_project_workflow::{handle_teleport_command, open_project_picker};
use super::super::setup::ResolvedTheme;
use super::super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use super::super::{
    Arguments, InteractiveRuntime, adopt_hydrated_session, metadata_session_id,
    parse_runtime_skills, push_command_echo, push_local_document, refresh_server_banner_metrics,
    sync_runtime_intent, unix_seconds,
};
use super::config::{apply_render_preferences, persisted_theme};
use super::mcp::{
    McpEffect, McpLoginOrigin, SystemUrlOpener, connector_sources, execute_mcp_effect,
    schedule_mcp_login, server_sources,
};
use super::{show_config, show_debug, show_model, show_proxy, show_rewind, show_voice};

/// What a command asks of the caller once it has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) enum FollowUp {
    Submit { text: String, injected: bool },
    ClipboardImage,
    Exit,
}

/// How long `/whoami` waits on the identity service before answering that
/// there is no identity.
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);

const SETUP_REQUIRED: &str = "Setup is required before using this command";

/// The session and the screen a command runs against.
pub(in crate::tui) struct LiveBackend<'a> {
    pub arguments: &'a Arguments,
    pub working_directory: &'a Path,
    pub runtime: &'a mut Option<InteractiveRuntime>,
    pub state: &'a mut TuiState,
    pub controls: &'a mut ControlState,
    pub composer: &'a mut ChatInputState,
    pub theme: &'a mut ResolvedTheme,
    pub turn_active: bool,
    pub follow_ups: Vec<FollowUp>,
    /// The transcript entry the command line was echoed as.
    echo: Option<String>,
    /// The picker the saved-session list was read into.
    sessions: Option<Overlay>,
    /// What `mcp/read` answered, which the MCP browser opens on.
    mcp: Option<Value>,
}

impl<'a> LiveBackend<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::tui) fn new(
        arguments: &'a Arguments,
        working_directory: &'a Path,
        runtime: &'a mut Option<InteractiveRuntime>,
        state: &'a mut TuiState,
        controls: &'a mut ControlState,
        composer: &'a mut ChatInputState,
        theme: &'a mut ResolvedTheme,
        turn_active: bool,
    ) -> Self {
        Self {
            arguments,
            working_directory,
            runtime,
            state,
            controls,
            composer,
            theme,
            turn_active,
            follow_ups: Vec::new(),
            echo: None,
            sessions: None,
            mcp: None,
        }
    }

    fn runtime(&mut self) -> Result<&mut InteractiveRuntime, String> {
        self.runtime
            .as_mut()
            .ok_or_else(|| SETUP_REQUIRED.to_owned())
    }

    /// One call whose answer the caller needs, reported as the message the
    /// server gave rather than pushed as a diagnostic.
    fn call(&mut self, method: &str, mut params: Value) -> Result<Value, String> {
        let runtime = self.runtime()?;
        if let Some(fields) = params.as_object_mut() {
            fields
                .entry("sessionId")
                .or_insert_with(|| json!(runtime.session_id));
        }
        runtime
            .service
            .public_call(method, params)
            .map(|result| Value::Object(result.into_iter().collect()))
            .map_err(|error| error.to_string())
    }

    /// A call whose work the server defers to a backend, awaited in place.
    async fn call_deferred(
        &mut self,
        method: &str,
        mut params: Value,
    ) -> Result<PublicDispatch, String> {
        let runtime = self.runtime()?;
        if let Some(fields) = params.as_object_mut() {
            fields
                .entry("sessionId")
                .or_insert_with(|| json!(runtime.session_id));
        }
        let pending = runtime
            .service
            .begin_public_call(method, params)
            .map_err(|error| error.to_string())?;
        pending.complete().await.map_err(|error| error.to_string())
    }

    fn notice(&mut self, text: String, status: EntryStatus, level: PublicNoticeLevel) {
        self.state.append_local(TranscriptEntry {
            id: String::new(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text,
            status,
            source: EntrySource::notice(level),
        });
    }

    fn open_panel(&mut self, panel: Panel, initial: Option<String>) {
        let Some(runtime) = self.runtime.as_mut() else {
            self.state.push_diagnostic(SETUP_REQUIRED);
            return;
        };
        match panel {
            Panel::Config => show_config(runtime, self.state),
            Panel::Model => show_model(runtime, self.state),
            Panel::Thinking => self.state.overlay = Some(thinking_overlay(&runtime.thinking)),
            Panel::Theme => {
                self.state.overlay = Some(theme_overlay(&persisted_theme(runtime)));
            }
            Panel::Voice => show_voice(runtime, self.state),
            Panel::ProxySetup => show_proxy(runtime, self.state),
            Panel::DebugConsole => show_debug(runtime, self.state),
            // Reference `action_rewind_prev`: nothing to rewind while a turn
            // runs, and nothing opens when no message was sent.
            Panel::Rewind => {
                if !self.turn_active {
                    show_rewind(runtime, self.state);
                }
            }
            Panel::Skills => self.state.overlay = Some(skills_overlay(runtime)),
            Panel::LogLevel => open_log_level_picker(self.state),
            Panel::Todos => self.state.overlay = Some(todos_overlay(self.state)),
            // The picker opened when the project list was requested, and the
            // saved sessions open once they are read.
            Panel::RemoteProjects | Panel::Sessions => {}
            Panel::Mcp => {
                let Some(value) = self.mcp.as_ref() else {
                    return;
                };
                let mut overlay = mcp_overlay(&server_sources(value), &connector_sources(value));
                if let Some(initial) = initial.filter(|initial| !initial.is_empty()) {
                    overlay.set_query(initial);
                }
                self.state.overlay = Some(overlay);
            }
            Panel::ConnectorAuth => execute_mcp_effect(
                McpEffect::BeginAuth {
                    kind: IntegrationKind::Connector,
                    source: initial.unwrap_or_default(),
                    enable_on_complete: false,
                },
                runtime,
                self.state,
                &SystemUrlOpener,
            ),
        }
    }

    /// Reference `_reload_config`, with everything this client derives from
    /// the configuration read again.
    async fn reload_configuration(&mut self) -> Result<Reloaded, String> {
        let result = self.call("config/reload", json!({}))?;
        let runtime = self.runtime()?;
        if let Ok(skills) = runtime
            .service
            .public_call("skills/list", json!({"sessionId": runtime.session_id}))
        {
            runtime.skills = parse_runtime_skills(skills.get("skills"));
        }
        let skills = runtime
            .skills
            .values()
            .map(|skill| (skill.name.clone(), skill.description.clone()))
            .collect::<Vec<_>>();
        self.composer.set_user_skills(
            skills
                .iter()
                .map(|(name, description)| (name.as_str(), description.as_str())),
        );
        let Some(runtime) = self.runtime.as_mut() else {
            return Err(SETUP_REQUIRED.to_owned());
        };
        let session_id = runtime.session_id.clone();
        refresh_server_banner_metrics(&mut runtime.service, &session_id, &mut runtime.banner).await;
        // Reference `_reset_ui_state` drops a pending retry offer.
        self.state.retry_offered = false;
        apply_render_preferences(runtime, self.state);
        // Reference `_apply_config_to_ui` re-applies the configured theme.
        crate::tui::preview_theme(&persisted_theme(runtime), self.theme);
        super::sync_voice_preference(runtime, self.composer);
        Ok(Reloaded {
            stripped_images: result
                .get("strippedHistoryImages")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            model_display_name: runtime
                .workspace
                .layered_config()
                .load()
                .ok()
                .and_then(|snapshot| snapshot.active_model_display_name())
                .unwrap_or_else(|| "None".to_owned()),
        })
    }
}

impl CommandBackend for LiveBackend<'_> {
    fn emit(&mut self, effect: Effect) {
        match effect {
            Effect::Echo(text) => self.echo = Some(push_command_echo(self.state, text)),
            Effect::Message(text) => {
                push_local_document(self.state, text);
            }
            Effect::Error(text) => self.notice(text, EntryStatus::Failed, PublicNoticeLevel::Error),
            Effect::Warning(text) => {
                self.notice(text, EntryStatus::Completed, PublicNoticeLevel::Warning);
            }
            Effect::Status { text, ok: true } => {
                self.notice(text, EntryStatus::Completed, PublicNoticeLevel::Info);
            }
            Effect::Status { text, ok: false } => {
                self.notice(text, EntryStatus::Failed, PublicNoticeLevel::Error);
            }
            // This client has no transient toast; the diagnostics line is what
            // shows a message without writing it into the transcript.
            Effect::Notify { text, .. } => self.state.push_diagnostic(text),
            Effect::Panel { panel, initial } => self.open_panel(panel, initial),
            Effect::ClosePanel => {
                self.state.overlay = None;
                self.state.log_level_picker = None;
            }
            Effect::RemoveEcho => {
                if let Some(echo) = self.echo.take() {
                    self.state.remove_local(&echo);
                }
            }
            // Adopting the session the clear continued under already replaced
            // the transcript.
            Effect::ResetTranscript => {}
            Effect::Submit { text, injected } => {
                self.follow_ups.push(FollowUp::Submit { text, injected });
            }
            Effect::ClipboardImage => self.follow_ups.push(FollowUp::ClipboardImage),
            Effect::Exit => self.follow_ups.push(FollowUp::Exit),
            Effect::Teleport { target, .. } => {
                if let Some(runtime) = self.runtime.as_mut() {
                    handle_teleport_command(
                        Some(&target),
                        self.working_directory,
                        runtime,
                        self.state,
                    );
                }
            }
            Effect::SessionsLoaded(_) => {
                if let Some(overlay) = self.sessions.take() {
                    self.state.overlay = Some(overlay);
                }
            }
            // The server holds the title, and this client draws none.
            Effect::Title(_) => {}
        }
    }

    fn report(&mut self, record: TelemetryRecord) {
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.report(&record);
        }
    }

    fn session_id(&self) -> String {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.session_id.clone())
            .unwrap_or_default()
    }

    fn turn_active(&self) -> bool {
        self.turn_active
    }

    fn session_log(&self) -> SessionLog {
        let Some(runtime) = self.runtime.as_ref() else {
            return SessionLog {
                enabled: false,
                persisted: false,
                path: String::new(),
            };
        };
        let directory = runtime
            .workspace
            .session_store()
            .session_directory(&runtime.session_id)
            .ok()
            .filter(|directory| directory.is_dir());
        SessionLog {
            enabled: runtime.workspace.session_logging_enabled(),
            persisted: directory.is_some(),
            path: directory
                .map(|directory| directory.display().to_string())
                .unwrap_or_default(),
        }
    }

    async fn reload(&mut self) -> Result<Reloaded, String> {
        self.reload_configuration().await
    }

    async fn clear_history(&mut self) -> Result<(), String> {
        let result = self.call("session/history/clear", json!({}))?;
        let result = result
            .as_object()
            .map(|fields| fields.clone().into_iter().collect())
            .unwrap_or_default();
        let session_id = metadata_session_id(&result)
            .ok_or_else(|| "the cleared session published no identifier".to_owned())?;
        let Some(runtime) = self.runtime.as_mut() else {
            return Err(SETUP_REQUIRED.to_owned());
        };
        if adopt_hydrated_session(runtime, self.state, self.controls, session_id) {
            Ok(())
        } else {
            Err("the new conversation could not be opened".to_owned())
        }
    }

    fn last_assistant_message(&self) -> Option<String> {
        self.state
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.kind == TranscriptKind::AssistantMessage)
            .map(|entry| entry.text.trim())
            .find(|text| !text.is_empty())
            .map(ToOwned::to_owned)
    }

    fn copy_text(&mut self, text: &str) -> bool {
        copy_text_verified(text)
    }

    fn history_is_empty(&self) -> bool {
        !self.state.entries.iter().any(|entry| {
            matches!(
                entry.kind,
                TranscriptKind::UserMessage | TranscriptKind::AssistantMessage
            )
        })
    }

    async fn compact(&mut self, instructions: &str) -> Result<(), String> {
        let runtime = self.runtime()?;
        let session_id = runtime.session_id.clone();
        let result = runtime
            .service
            .compact(&session_id, instructions)
            .await
            .map_err(|error| error.to_string())?;
        let compacted = result
            .get("state")
            .and_then(|state| state.pointer("/session/id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| "the compaction published no session identifier".to_owned())?;
        let Some(runtime) = self.runtime.as_mut() else {
            return Err(SETUP_REQUIRED.to_owned());
        };
        if adopt_hydrated_session(runtime, self.state, self.controls, compacted) {
            Ok(())
        } else {
            Err("the compacted session could not be opened".to_owned())
        }
    }

    fn stats(&mut self) -> Stats {
        let stats = self
            .call("stats/read", json!({}))
            .ok()
            .and_then(|result| result.get("stats").cloned())
            .unwrap_or(Value::Null);
        let count = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or_default();
        let price = |key: &str| stats.get(key).and_then(Value::as_f64);
        let (prompt, completion, cached) = (
            count("sessionPromptTokens"),
            count("sessionCompletionTokens"),
            count("sessionCachedTokens"),
        );
        Stats {
            steps: count("steps"),
            session_prompt_tokens: prompt,
            session_cached_tokens: cached,
            session_completion_tokens: completion,
            session_total_llm_tokens: prompt.saturating_add(completion),
            last_turn_total_tokens: count("lastTurnPromptTokens")
                .saturating_add(count("lastTurnCompletionTokens")),
            last_turn_cached_tokens: count("lastTurnCachedTokens"),
            session_cost: session_cost(
                prompt,
                completion,
                cached,
                price("inputPricePerMillion").unwrap_or_default(),
                price("outputPricePerMillion").unwrap_or_default(),
                price("cachedInputPricePerMillion"),
            ),
        }
    }

    /// Reference `IdentityController.read`: only a Mistral model has an
    /// identity, read with the credential its provider names.
    async fn identity(&mut self) -> Result<Option<Identity>, String> {
        static CACHE: OnceLock<IdentityCache> = OnceLock::new();
        let runtime = self.runtime()?;
        let Some(provider) = runtime
            .workspace
            .layered_config()
            .load()
            .ok()
            .and_then(|snapshot| snapshot.active_provider())
            .filter(|provider| {
                provider.get("backend").and_then(toml::Value::as_str) == Some("mistral")
            })
        else {
            return Ok(None);
        };
        let text = |key: &str| {
            provider
                .get(key)
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let (base_url, variable) = (text("api_base"), text("api_key_env_var"));
        let Some(api_key) = (!variable.is_empty())
            .then(|| crate::cli_credentials(self.arguments)(&variable))
            .flatten()
            .filter(|key| !key.is_empty())
        else {
            return Ok(None);
        };
        let Some(gateway) = HttpIdentityGateway::production() else {
            return Ok(None);
        };
        let resolved = CACHE
            .get_or_init(IdentityCache::new)
            .resolve(&gateway, &base_url, &api_key, Some(IDENTITY_TIMEOUT))
            .await;
        Ok(resolved.map(|identity| {
            let name = match (
                identity.first_name.as_deref(),
                identity.last_name.as_deref(),
            ) {
                (Some(first), Some(last)) if !first.is_empty() && !last.is_empty() => {
                    Some(format!("{first} {last}"))
                }
                (Some(first), _) if !first.is_empty() => Some(first.to_owned()),
                _ => identity.email.clone(),
            };
            Identity {
                name,
                email: identity.email.clone(),
                workspace: identity
                    .workspace
                    .as_ref()
                    .map(|entity| entity.name.clone()),
                organization: identity
                    .organization
                    .as_ref()
                    .map(|entity| entity.name.clone()),
            }
        }))
    }

    async fn account_plan(&mut self) -> Result<Option<String>, String> {
        let result = Value::Object(
            self.call_deferred("account/read", json!({}))
                .await?
                .result
                .into_iter()
                .collect(),
        );
        Ok(match result.pointer("/account/plan") {
            Some(Value::String(plan)) => Some(plan.clone()),
            Some(plan) => plan
                .get("title")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            None => None,
        })
    }

    async fn open_projects(&mut self) -> Result<(), String> {
        let Some(runtime) = self.runtime.as_mut() else {
            return Err(SETUP_REQUIRED.to_owned());
        };
        open_project_picker(self.working_directory, runtime, self.state);
        Ok(())
    }

    async fn saved_sessions(&mut self) -> usize {
        let cwd = self.working_directory.to_string_lossy().into_owned();
        let Ok(result) = self.call(
            "session/list",
            json!({"cwd": cwd, "offset": 0, "limit": 100}),
        ) else {
            return 0;
        };
        let current = self.session_id();
        let overlay = sessions_overlay(&result, &current);
        let count = overlay.items.len();
        self.sessions = Some(overlay);
        count
    }

    async fn rename(&mut self, title: &str) -> Result<String, String> {
        self.call("session/title/update", json!({"title": title}))?;
        Ok(title.to_owned())
    }

    async fn mcp_state(&mut self) -> McpState {
        let value = self
            .call_deferred("mcp/read", json!({}))
            .await
            .map(|dispatch| Value::Object(dispatch.result.into_iter().collect()))
            .unwrap_or(Value::Null);
        let sources = value
            .pointer("/mcp/sources")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let state = McpState {
            sources: sources
                .iter()
                .filter_map(|source| {
                    let name = source.get("name").and_then(Value::as_str)?;
                    let kind = match source.get("kind").and_then(Value::as_str) {
                        Some("connector") => McpSourceKind::Connector,
                        _ => McpSourceKind::Server,
                    };
                    Some((name.to_owned(), kind))
                })
                .collect(),
            connector_error: value
                .pointer("/mcp/connectorError")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            statuses: sources
                .iter()
                .filter(|source| source.get("kind").and_then(Value::as_str) != Some("connector"))
                .filter_map(|source| {
                    Some((
                        source.get("name").and_then(Value::as_str)?.to_owned(),
                        source.get("status").and_then(Value::as_str)?.to_owned(),
                    ))
                })
                .collect(),
        };
        self.mcp = Some(value);
        state
    }

    /// Starts the OAuth login beside the interactive slot. It answers once
    /// the browser comes back, and reports its URL and its outcome itself.
    async fn mcp_login(&mut self, alias: &str) -> Result<McpLogin, String> {
        let Some(runtime) = self.runtime.as_mut() else {
            return Err(SETUP_REQUIRED.to_owned());
        };
        schedule_mcp_login(
            runtime,
            alias.to_owned(),
            McpLoginOrigin::Command,
            self.state,
        );
        Ok(McpLogin {
            urls: Vec::new(),
            completed: false,
        })
    }

    async fn mcp_logout(&mut self, alias: &str) -> Result<(), String> {
        self.call_deferred("mcp/logout", json!({"name": alias}))
            .await
            .map(drop)
    }

    async fn mcp_add(&mut self, arguments: &McpAddArguments) -> Result<(String, bool), String> {
        let mut params = json!({
            "url": arguments.url,
            "scopes": arguments.scopes,
            "transport": arguments.transport,
            "allowInsecureHttp": arguments.allow_insecure_http,
        });
        if let (Some(name), Some(fields)) = (&arguments.name, params.as_object_mut()) {
            fields.insert("name".to_owned(), json!(name));
        }
        let dispatch = self.call_deferred("mcp/add", params).await?;
        let name = dispatch
            .result
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "the MCP server was added under no name".to_owned())?
            .to_owned();
        let created = dispatch
            .result
            .get("created")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok((name, created))
    }

    fn open_url(&mut self, url: &str) {
        let url = url.to_owned();
        // Reference `webbrowser.open` is fire and forget: a browser that will
        // not open leaves the URL printed above for the operator to follow.
        tokio::spawn(async move {
            drop(super::mcp::open_auth_url(url).await);
        });
    }

    fn has_todos(&self) -> bool {
        !current_todos(self.state).is_empty()
    }

    async fn agent_names(&mut self) -> Vec<String> {
        self.call("agents/list", json!({}))
            .ok()
            .and_then(|result| result.get("agents").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|agent| agent.get("name").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect()
    }

    async fn set_agent_installed(&mut self, name: &str, installed: bool) -> Result<(), String> {
        let method = if installed {
            "agents/install"
        } else {
            "agents/uninstall"
        };
        let result = self.call(method, json!({"agentName": name}))?;
        // The catalog answer names the agent the session runs now, which is the
        // default when the one it was running has just been uninstalled.
        if let Some(agent) = result.pointer("/active/name").and_then(Value::as_str)
            && let Some(runtime) = self.runtime.as_mut()
        {
            sync_runtime_intent(runtime, Some(agent));
        }
        Ok(())
    }

    async fn fork(&mut self) -> Result<String, String> {
        let source = self.session_id();
        let result = self.call(
            "session/fork",
            json!({"newSessionId": vibe_core::session_id::rotate_session_id(&source)}),
        )?;
        let result = result
            .as_object()
            .map(|fields| fields.clone().into_iter().collect())
            .unwrap_or_default();
        metadata_session_id(&result).ok_or_else(|| "the copy published no identifier".to_owned())
    }

    fn retry_offered(&self) -> bool {
        self.state.retry_offered
    }

    fn now(&self) -> f64 {
        unix_seconds() as f64
    }

    async fn loops_list(&mut self) -> Result<Vec<ScheduledLoop>, String> {
        let result = self.call("loops/list", json!({}))?;
        Ok(result
            .get("loops")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(scheduled_loop)
            .collect())
    }

    async fn loops_create(
        &mut self,
        interval: &str,
        prompt: &str,
    ) -> Result<ScheduledLoop, String> {
        let result = self.call(
            "loops/create",
            json!({"interval": interval, "prompt": prompt}),
        )?;
        result
            .get("loop")
            .and_then(scheduled_loop)
            .ok_or_else(|| "the scheduled loop was created without its fields".to_owned())
    }

    async fn loops_delete(&mut self, id: &str) -> Result<ScheduledLoop, String> {
        let result = self.call("loops/delete", json!({"loopId": id}))?;
        result
            .get("loop")
            .and_then(scheduled_loop)
            .ok_or_else(|| "the scheduled loop was cancelled without its fields".to_owned())
    }

    async fn loops_clear(&mut self) -> Result<u64, String> {
        let result = self.call("loops/clear", json!({}))?;
        Ok(result
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or_default())
    }
}

pub(in crate::tui) fn scheduled_loop(value: &Value) -> Option<ScheduledLoop> {
    Some(ScheduledLoop {
        id: value.get("id")?.as_str()?.to_owned(),
        prompt: value.get("prompt")?.as_str()?.to_owned(),
        interval_seconds: value.get("intervalSeconds")?.as_u64()?,
        next_fire_at: value
            .get("nextFireAt")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
    })
}

/// Runs one submitted command line against the live session, answering what
/// the caller still has to do, or [`None`] when the line names no command.
pub(in crate::tui) async fn run_command(
    line: &str,
    context: &CommandContext,
    backend: &mut LiveBackend<'_>,
) -> Option<Vec<FollowUp>> {
    let parsed = parse_command_in(line, context)?;
    super::super::command_handlers::run(line, &parsed, context, backend).await;
    Some(std::mem::take(&mut backend.follow_ups))
}

// --------------------------------------------------------------------------
// Todos and skills
// --------------------------------------------------------------------------

/// The todo list the agent last wrote. Reference `TodoTracker.todos`, which
/// follows the `todo` tool's results and empties when the conversation does.
fn current_todos(state: &TuiState) -> Vec<(String, String)> {
    state
        .entries
        .iter()
        .rev()
        .find_map(|entry| match entry.source.server() {
            Some(PublicHistoryEntry::Effect {
                detail,
                state: vibe_core::events::PublicEffectState::Completed { output, .. },
                ..
            }) if detail.kind == vibe_core::events::ToolEffectKind::Todo => Some(output.clone()),
            _ => None,
        })
        .and_then(|output| output.get("todos").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|todo| {
            Some((
                todo.get("status")?.as_str()?.to_owned(),
                todo.get("content")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// Reference `TodoOverlayScreen`: the list grouped by status, in the order the
/// transcript renders it.
fn todos_overlay(state: &TuiState) -> Overlay {
    let todos = current_todos(state);
    let mut items = Vec::new();
    for status in ["in_progress", "pending", "completed", "cancelled"] {
        for (index, (_, content)) in todos
            .iter()
            .enumerate()
            .filter(|(_, (todo_status, _))| todo_status == status)
        {
            let icon = match status {
                "completed" => "☑",
                "cancelled" => "☒",
                _ => "☐",
            };
            items.push(OverlayItem::new(
                format!("todo-{index}"),
                format!("{icon} {content}"),
                "",
                false,
            ));
        }
    }
    Overlay::new(OverlayKind::Todos, "Todos", items)
}

/// The skills this session loaded, which is what the browser lists.
fn skills_overlay(runtime: &InteractiveRuntime) -> Overlay {
    let items = runtime
        .skills
        .values()
        .map(|skill| {
            OverlayItem::new(
                skill.name.clone(),
                format!("/{}", skill.name),
                skill.description.clone(),
                false,
            )
        })
        .collect();
    Overlay::new(OverlayKind::Skills, "Skills", items)
}

// --------------------------------------------------------------------------
// The log-level picker
// --------------------------------------------------------------------------

fn open_log_level_picker(state: &mut TuiState) {
    let picker = Picker::new(log_level_chain());
    state.overlay = Some(log_level_overlay(&picker));
    state.log_level_picker = Some(picker);
}

/// Reference `_build_row`, as a list: the effective level is marked, and each
/// row names the badges it carries, the highlighted row naming the one that
/// `Enter` toggles.
pub(in crate::tui) fn log_level_overlay(picker: &Picker) -> Overlay {
    let effective = picker.effective();
    let items = LogLevel::ALL
        .into_iter()
        .map(|level| {
            let marker = if level == effective { "› " } else { "  " };
            let mut badges = Vec::new();
            for (badge, name, set) in [
                (Badge::Session, "session", picker.session() == Some(level)),
                (Badge::Config, "config", picker.config() == Some(level)),
            ] {
                let focused = level == picker.highlighted() && badge == picker.focused();
                match (focused, set) {
                    (true, _) => badges.push(format!("[{name}]")),
                    (false, true) => badges.push(name.to_owned()),
                    (false, false) => {}
                }
            }
            OverlayItem::new(
                level.as_str(),
                format!("{marker}{:<10}", level.as_str()),
                badges.join("  "),
                false,
            )
        })
        .collect();
    let mut overlay = Overlay::new(OverlayKind::LogLevel, "Log Level", items);
    overlay.select_id(picker.highlighted().as_str());
    overlay.notice = Some(format!(
        "{}\n↑↓/jk Navigate  ←/→ Switch badge  Enter Toggle  Esc Close",
        picker.subtitle()
    ));
    overlay
}

/// What the picker's keys do, and applying it when it closes.
pub(in crate::tui) fn handle_log_level_key(
    key: crossterm::event::KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) {
    use crossterm::event::KeyCode;

    let Some(mut picker) = state.log_level_picker.take() else {
        state.overlay = None;
        return;
    };
    let step = |picker: &Picker, delta: isize| {
        let index = LogLevel::ALL
            .iter()
            .position(|level| *level == picker.highlighted())
            .unwrap_or_default();
        let next = index
            .saturating_add_signed(delta)
            .min(LogLevel::ALL.len() - 1);
        LogLevel::ALL[next]
    };
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => picker.highlight(step(&picker, -1)),
        KeyCode::Down | KeyCode::Char('j') => picker.highlight(step(&picker, 1)),
        KeyCode::Left | KeyCode::Char('h') => picker.focus(Badge::Session),
        KeyCode::Right | KeyCode::Char('l') => picker.focus(Badge::Config),
        KeyCode::Enter => picker.toggle(),
        KeyCode::Esc => {
            let mut backend = LiveLogLevel { runtime, state };
            log_level::apply(picker.applied(), &mut backend);
            return;
        }
        _ => {}
    }
    state.overlay = Some(log_level_overlay(&picker));
    state.log_level_picker = Some(picker);
}

struct LiveLogLevel<'a> {
    runtime: &'a mut Option<InteractiveRuntime>,
    state: &'a mut TuiState,
}

impl LogLevelBackend for LiveLogLevel<'_> {
    fn chain(&self) -> LogLevelChain {
        log_level_chain()
    }

    fn set_session_override(&mut self, level: Option<LogLevel>) {
        set_session_log_level(level);
    }

    fn persist(&mut self, level: Option<LogLevel>) -> Result<(), String> {
        let runtime = self
            .runtime
            .as_mut()
            .ok_or_else(|| SETUP_REQUIRED.to_owned())?;
        let mutation = match level {
            Some(level) => json!({"path": ["log_level"], "value": level.as_str()}),
            None => json!({"path": ["log_level"], "remove": true}),
        };
        runtime
            .service
            .public_call(
                "config/batchWrite",
                json!({
                    "sessionId": runtime.session_id,
                    "writes": [{"target": "user", "mutations": [mutation]}],
                }),
            )
            .map_err(|error| error.to_string())?;
        // Reference `_on_config_changed`: the written level is the configured
        // tier from now on.
        vibe_core::observability::set_config_log_level(level);
        Ok(())
    }

    fn emit(&mut self, effect: Effect) {
        match effect {
            Effect::ClosePanel => {
                self.state.overlay = None;
                self.state.log_level_picker = None;
            }
            Effect::Message(text) => {
                push_local_document(self.state, text);
            }
            Effect::Error(text) => {
                self.state.append_local(TranscriptEntry {
                    id: String::new(),
                    revision: 1,
                    kind: TranscriptKind::Notice,
                    text,
                    status: EntryStatus::Failed,
                    source: EntrySource::notice(PublicNoticeLevel::Error),
                });
            }
            _ => {}
        }
    }
}
