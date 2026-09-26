//! Opening, reattaching, forking, listing, and closing sessions.
//!
//! Reference `VibeAcpAgent.new_session`, `load_session`, `resume_session`,
//! `fork_session`, `close_session`, `list_sessions`, `set_session_mode`, and
//! `set_config_option`.

use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::{ClientError, SessionOptions, TurnDriver};
use vibe_protocol::ProtocolErrorCode;

use crate::agent::AcpAgent;
use crate::agent::surface::INITIAL_COMMANDS_DELAY;
use crate::client_tools::{attach_client_tools, serve_client_tools};
use crate::mcp::project_acp_mcp_servers;
use crate::projection::replay_session_updates;
use crate::protocol::{AcpError, AcpForkSession, AcpLoadSession, AcpNewSession};
use crate::router::response_sent;
use crate::session::AcpHarness;

/// The tools a client that cannot answer a question has disabled. Reference
/// `NON_INTERACTIVE_DISABLED_TOOLS`.
const NON_INTERACTIVE_DISABLED_TOOLS: [&str; 2] = ["ask_user_question", "exit_plan_mode"];

/// Which session `create_session` opens.
enum Intent<'a> {
    New,
    Resume(&'a str),
}

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    pub async fn new_session(self: &Arc<Self>, request: AcpNewSession) -> Result<Value, AcpError> {
        let harness = self
            .create_session(
                &request.cwd,
                Intent::New,
                request.additional_directories.unwrap_or_default(),
                request.mcp_servers.as_deref().unwrap_or_default(),
            )
            .await?;
        let (modes, _) = self.mode_state(&harness).await?;
        self.send_usage_update(&harness);
        Ok(json!({
            "sessionId": harness.session_id,
            "modes": modes,
            "configOptions": self.config_options(&harness).await?,
            "_meta": self.trust_meta(&harness, &request.cwd).await?,
        }))
    }

    /// Reference `load_session`: the saved session is opened, its transcript
    /// replayed to the client, and its settings answered.
    pub async fn load_session(
        self: &Arc<Self>,
        request: AcpLoadSession,
    ) -> Result<Value, AcpError> {
        if self
            .lock_state()?
            .sessions
            .contains_key(&request.session_id)
        {
            return Err(AcpError::Configuration(format!(
                "session `{}` is already open",
                request.session_id
            )));
        }
        let harness = self
            .create_session(
                &request.cwd,
                Intent::Resume(&request.session_id),
                request.additional_directories.clone().unwrap_or_default(),
                request.mcp_servers.as_deref().unwrap_or_default(),
            )
            .await?;
        let state = self.session_state(&harness).await?;
        let session = state.get("session").cloned().unwrap_or(Value::Null);
        let title = display_title(&session);
        let updated_at = session
            .get("updatedAt")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let history = history_entries(&state);
        for entry in &history {
            harness.remember_entry(entry);
        }
        for update in replay_session_updates(title.as_deref(), updated_at, &history) {
            self.session_update(&harness.session_id, update);
        }
        self.send_usage_update(&harness);
        let (modes, _) = self.mode_state(&harness).await?;
        Ok(json!({
            "modes": modes,
            "configOptions": self.config_options(&harness).await?,
            "_meta": self.trust_meta(&harness, &request.cwd).await?,
        }))
    }

    /// Reference `resume_session`: a session that is not live is opened
    /// without replaying anything, and either way the answer is empty.
    pub async fn resume_session(
        self: &Arc<Self>,
        request: AcpLoadSession,
    ) -> Result<Value, AcpError> {
        let live = self
            .lock_state()?
            .sessions
            .get(&request.session_id)
            .cloned();
        let harness = match live {
            Some(harness) => harness,
            None => {
                self.create_session(
                    &request.cwd,
                    Intent::Resume(&request.session_id),
                    request.additional_directories.clone().unwrap_or_default(),
                    request.mcp_servers.as_deref().unwrap_or_default(),
                )
                .await?
            }
        };
        self.send_usage_update(&harness);
        Ok(json!({}))
    }

    /// Reference `fork_session`: the live source is forked at the message the
    /// client names, and the fork is opened as a session of its own.
    pub async fn fork_session(
        self: &Arc<Self>,
        request: AcpForkSession,
    ) -> Result<Value, AcpError> {
        let source = self.session_harness(&request.session_id)?;
        let mut params = json!({});
        if let Some(message_id) = &request.message_id {
            params["messageId"] = json!(message_id);
        }
        let fork = self
            .call(&source, "internal/session/fork", params)
            .await
            .map_err(invalid_request)?;
        let fork_id = fork
            .get("metadata")
            .and_then(|metadata| metadata.get("session_id").or_else(|| metadata.get("id")))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| AcpError::InvalidResponse("the fork named no session".to_owned()))?;
        let child = self
            .create_session(
                &request.cwd,
                Intent::Resume(&fork_id),
                request.additional_directories.clone().unwrap_or_default(),
                request.mcp_servers.as_deref().unwrap_or_default(),
            )
            .await?;
        let (modes, _) = self.mode_state(&child).await?;
        self.send_usage_update(&child);
        Ok(json!({
            "sessionId": child.session_id,
            "modes": modes,
            "configOptions": self.config_options(&child).await?,
        }))
    }

    /// Reference `close_session`: only a live session can be closed.
    pub async fn close_session(&self, session_id: &str) -> Result<(), AcpError> {
        let harness = self
            .lock_state()?
            .sessions
            .remove(session_id)
            .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))?;
        stop_session(&harness).await;
        Ok(())
    }

    /// Stops every live session, which is what a closing connection does.
    pub async fn disconnect(&self) -> Result<(), AcpError> {
        let sessions = std::mem::take(&mut self.lock_state()?.sessions);
        for harness in sessions.into_values() {
            stop_session(&harness).await;
        }
        Ok(())
    }

    /// Reference `list_sessions`: every saved session, titled by its title or
    /// else its first message.
    pub async fn list_sessions(&self, cwd: Option<&str>) -> Result<Value, AcpError> {
        let working_directory = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_owned());
        let mut params = json!({});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        let listed = self.with_probe(&working_directory, &[], |probe| {
            Ok(probe.public_call("internal/session/list", params)?)
        })?;
        // The marker root names the vibe home; the transcripts live where its
        // configuration saves them.
        let save_dir = self.session_root.as_deref().map(|root| {
            vibe_app_server::workspace::WorkspaceService::for_runtime_session_root(
                root,
                &working_directory,
            )
            .session_root()
            .to_path_buf()
        });
        let text = |session: &Value, key: &str| {
            session
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        };
        // A session is saved once it holds a message; the store keeps the
        // record of an empty one, which the reference never wrote.
        let sessions = listed
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|session| session.get("messageCount").and_then(Value::as_u64) != Some(0))
            .map(|session| {
                let id = text(session, "id").unwrap_or_default();
                let title = text(session, "title").or_else(|| {
                    save_dir
                        .as_deref()
                        .and_then(|root| vibe_app_server::startup::saved_session_preview(root, &id))
                });
                let mut info = json!({
                    "sessionId": id,
                    "cwd": text(session, "workingDirectory").unwrap_or_default(),
                    "updatedAt": text(session, "endTime").or_else(|| text(session, "startTime")),
                });
                if let Some(title) = title {
                    info["title"] = json!(title);
                }
                info
            })
            .collect::<Vec<_>>();
        Ok(json!({"sessions": sessions}))
    }

    /// Reference `set_session_mode`: a mode the session does not offer is
    /// ignored rather than refused.
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        let harness = self.session_harness(session_id)?;
        if !self.is_primary_mode(&harness, mode_id).await? {
            return Ok(());
        }
        self.call(&harness, "session/agent/update", json!({"name": mode_id}))
            .await?;
        Ok(())
    }

    /// Reference `set_config_option`.
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &Value,
    ) -> Result<Value, AcpError> {
        let harness = self.session_harness(session_id)?;
        let unsupported = || {
            AcpError::InvalidParams(format!("the `{config_id}` option cannot be set to {value}"))
        };
        let text = value.as_str();
        match (config_id, text) {
            ("mode", Some(mode)) if self.is_primary_mode(&harness, mode).await? => {
                self.call(&harness, "session/agent/update", json!({"name": mode}))
                    .await
                    .map_err(invalid_request)?;
            }
            ("model", Some(model)) if self.offers_model(&harness, model).await? => {
                self.call(
                    &harness,
                    "config/patch",
                    json!({
                        "ops": [{"op": "set", "path": "/active_model", "value": model}],
                        "reloadRuntime": true,
                    }),
                )
                .await
                .map_err(invalid_request)?;
            }
            ("thinking", Some(level))
                if ["off", "low", "medium", "high", "max"].contains(&level) =>
            {
                // Reference `model_config_write_ops`: thinking is stored on
                // the active model's entry.
                let config = self.config_view(&harness).await?;
                let alias = config
                    .pointer("/activeModel/alias")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .replace('~', "~0")
                    .replace('/', "~1");
                self.call(
                    &harness,
                    "config/patch",
                    json!({
                        "ops": [{"op": "set", "path": format!("/models/{alias}/thinking"), "value": level}],
                        "reloadRuntime": true,
                    }),
                )
                .await
                .map_err(invalid_request)?;
            }
            ("max_turns" | "max_tokens", Some(raw)) => {
                let parsed = python_int(raw).ok_or_else(|| {
                    AcpError::InvalidParams(format!("`{raw}` is not a whole number"))
                })?;
                let key = if config_id == "max_turns" {
                    "maxTurns"
                } else {
                    "maxTokens"
                };
                self.call(&harness, "session/settings/update", json!({key: parsed}))
                    .await
                    .map_err(invalid_request)?;
            }
            _ => return Err(unsupported()),
        }
        Ok(json!({"configOptions": self.config_options(&harness).await?}))
    }

    async fn offers_model(&self, harness: &AcpHarness<D>, alias: &str) -> Result<bool, AcpError> {
        let config = self.config_view(harness).await?;
        Ok(config
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model.get("alias").and_then(Value::as_str) == Some(alias))
            }))
    }

    /// The public state of the session, history included.
    pub(crate) async fn session_state(&self, harness: &AcpHarness<D>) -> Result<Value, AcpError> {
        Ok(self
            .call(harness, "session/read", json!({}))
            .await?
            .remove("state")
            .unwrap_or(Value::Null))
    }

    /// Reference `_create_session`: one canonical session under the client
    /// this connection describes, registered and warmed up.
    async fn create_session(
        self: &Arc<Self>,
        cwd: &str,
        intent: Intent<'_>,
        workspace_roots: Vec<String>,
        mcp_servers: &[Value],
    ) -> Result<Arc<AcpHarness<D>>, AcpError> {
        let mcp_servers = project_acp_mcp_servers(mcp_servers)?;
        let supports_user_input = self.lock_state()?.capabilities().elicitation_form;
        let disabled_tools = if supports_user_input {
            Vec::new()
        } else {
            NON_INTERACTIVE_DISABLED_TOOLS
                .iter()
                .map(|tool| (*tool).to_owned())
                .collect()
        };
        let resume = match intent {
            Intent::New => None,
            Intent::Resume(session_id) => Some(session_id.to_owned()),
        };
        let mut service = self
            .new_service(cwd, &workspace_roots)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        let options = SessionOptions {
            working_directory: cwd.to_owned(),
            session_id: resume.clone(),
            add_directories: workspace_roots,
            trusted: false,
            agent: None,
            tool_filters: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools,
            mcp_servers,
            model: None,
            max_turns: None,
            max_tokens: None,
            max_price_micros: None,
            mode: None,
            thinking: false,
            reasoning_effort: None,
            auto_approve: false,
            // The reference declares the editor launch headless, as it does
            // the programmatic one (`vibe/acp/entrypoint.py`).
            headless: true,
            resume: resume.clone(),
            continue_session: false,
        };
        let bridge = service.client_tools();
        let frames = self.client.is_some().then(|| attach_client_tools(&bridge));
        let session_id = match service.start_session(&options) {
            Ok(session_id) => session_id,
            Err(error) => {
                let _ = service.shutdown();
                return Err(start_error(error, resume.as_deref()));
            }
        };
        let harness = Arc::new(self.adopt(service, &session_id)?);
        if let Ok(state) = self.session_state(&harness).await {
            if let Some(session) = state.get("session") {
                harness.seed_display_title(session);
            }
            for entry in history_entries(&state) {
                harness.remember_entry(&entry);
            }
        }
        if let (Some(client), Some(frames)) = (self.client.clone(), frames) {
            let pump = serve_client_tools(
                client,
                &bridge,
                frames,
                harness.session_id.clone(),
                harness.barrier.clone(),
            );
            harness.track(pump.abort_handle());
        }
        self.lock_state()?
            .sessions
            .insert(harness.session_id.clone(), Arc::clone(&harness));
        self.warm_up(&harness);
        self.send_initial_commands(&harness);
        Ok(harness)
    }

    /// Reference `_warm_up`: once the session's servers settle, the client is
    /// told which MCP servers failed to connect.
    fn warm_up(self: &Arc<Self>, harness: &Arc<AcpHarness<D>>) {
        let agent = Arc::clone(self);
        let session = Arc::clone(harness);
        // The reference spawns this and speaks only once the servers settle,
        // which is always after the session's own answer.
        let answered = response_sent();
        let task = tokio::spawn(async move {
            let failures = {
                let mut service = session.service.lock().await;
                service
                    .initialize_pending_mcp(&session.canonical_id())
                    .await
                    .unwrap_or_default()
            };
            if let Some(answered) = answered {
                let _ = answered.await;
            }
            if !failures.is_empty() {
                let mut failures = failures;
                failures.sort();
                let mut lines = vec!["These MCP servers could not be reached:".to_owned()];
                lines.extend(failures.iter().map(|failure| format!("- {failure}")));
                agent.command_message(&session, &lines.join("\n"), None);
            }
            // Reference `_notify_mcp_auth`: the servers still waiting on an
            // OAuth sign-in, by name.
            let sources = agent
                .call(&session, "mcp/read", json!({}))
                .await
                .ok()
                .and_then(|mut result| result.remove("mcp"))
                .and_then(|mcp| mcp.get("sources").cloned());
            let mut waiting = sources
                .as_ref()
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|source| {
                    source.get("kind").and_then(Value::as_str) == Some("server")
                        && source.get("status").and_then(Value::as_str) == Some("needs_auth")
                })
                .filter_map(|source| source.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>();
            if !waiting.is_empty() {
                waiting.sort_unstable();
                let message = format!(
                    "These MCP servers need an OAuth sign-in first: {}. Sign in with `/mcp login <alias>` in Vibe.",
                    waiting.join(", ")
                );
                agent.command_message(&session, &message, None);
            }
        });
        harness.track(task.abort_handle());
    }

    /// Reference `_send_initial_commands`: the command catalog, shortly after
    /// the session exists.
    fn send_initial_commands(self: &Arc<Self>, harness: &Arc<AcpHarness<D>>) {
        let agent = Arc::clone(self);
        let session = Arc::clone(harness);
        let task = tokio::spawn(async move {
            tokio::time::sleep(INITIAL_COMMANDS_DELAY).await;
            let _ = agent.send_commands(&session).await;
        });
        harness.track(task.abort_handle());
    }
}

/// The title a session is shown under: its own, or else its first message.
pub(crate) fn display_title(session: &Value) -> Option<String> {
    session
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .or_else(|| {
            session
                .get("preview")
                .and_then(Value::as_str)
                .filter(|preview| !preview.is_empty())
        })
        .map(ToOwned::to_owned)
}

/// The entries of a public session state.
pub(crate) fn history_entries(state: &Value) -> Vec<Value> {
    state
        .get("history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// How the reference reports a session that could not be opened.
fn start_error(error: ClientError, resumed: Option<&str>) -> AcpError {
    match (&error, resumed) {
        (ClientError::Protocol(ProtocolErrorCode::NotFound, _), Some(session_id)) => {
            AcpError::SessionNotFound(session_id.to_owned())
        }
        (ClientError::Protocol(ProtocolErrorCode::Unauthorized, message), _) => {
            AcpError::Unauthenticated(message.clone())
        }
        (ClientError::Protocol(_, message), _) => AcpError::Configuration(message.clone()),
        _ => AcpError::Configuration(error.to_string()),
    }
}

/// An app-server refusal, reported the way the reference reports it: as an
/// invalid request carrying the server's message.
pub(crate) fn invalid_request(error: AcpError) -> AcpError {
    match error {
        AcpError::AppServer(ClientError::Protocol(_, message)) => AcpError::InvalidParams(message),
        other => other,
    }
}

/// Python's `int()` over a string: surrounding whitespace, a sign, and
/// underscores between digits.
fn python_int(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits
            .chars()
            .all(|character| character.is_ascii_digit() || character == '_')
    {
        return None;
    }
    digits
        .replace('_', "")
        .parse::<i64>()
        .ok()
        .map(|value| sign * value)
}

/// Stops a session the agent no longer holds: its tasks, any turn it runs,
/// and its canonical session.
async fn stop_session<D>(harness: &AcpHarness<D>)
where
    D: TurnDriver,
{
    harness.abort_tasks();
    let _ = harness.request_cancel();
    if let Some(experiments) = harness.experiments.as_ref() {
        experiments.close().await;
    }
    let mut service = harness.service.lock().await;
    if let Ok(Some(turn_id)) = harness.running_turn_id() {
        let _ = service.interrupt(&harness.canonical_id(), &turn_id);
    }
    let _ = service.close_session(&harness.canonical_id()).await;
    let _ = service.shutdown();
}
