#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use vibe_app_server::client::{
    ClientError, HeadlessService, ProgrammaticTeleportEvent, ProgrammaticTurn, ProgrammaticUpdate,
    PublicCallbackState, PublicContentBlock, PublicEffectState, PublicHistoryEntry,
    PublicMessageRole, SessionOptions, TurnDriver, TurnRequest,
};
use vibe_app_server::release3::Release3Service;
use vibe_app_server::release4::{Release4Service, VibeCodeCloudConfig};
use vibe_app_server::server::{
    AppServer, OwnedToolHandlerFuture, SessionToolFactory, ToolAvailability, ToolError,
    ToolExecutionOutput, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry,
    ToolSource, ToolSpec,
};
use vibe_protocol::{
    CallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, ClientToolCapability,
    TerminalEmulator,
};

pub const ADAPTER_NAME: &str = "vibe-acp";
pub const ACP_PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_CLIENT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EMBEDDED_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROMPT_BLOCKS: usize = 256;
const MAX_ADDITIONAL_DIRECTORIES: usize = 128;
const MAX_MCP_SERVERS: usize = 64;
const MAX_SESSION_LIST_SCAN: usize = 100_000;
const MAX_HISTORY_ENTRIES: usize = 100_000;
const MAX_HISTORY_PAGE_SIZE: usize = 500;
const SESSION_LIST_PAGE_SIZE: usize = 50;
const SESSION_CURSOR_PREFIX: &str = "v1:";
pub const MAX_ACP_UPDATE_QUEUE: usize = 1_024;

#[must_use]
pub fn available_commands() -> Vec<Value> {
    command_catalog(false)
}

fn command_catalog(vibe_code_enabled: bool) -> Vec<Value> {
    let mut commands = vec![
        (
            "compact",
            "Compact conversation history by summarizing it",
            Some("Optional instructions for the summary"),
        ),
        ("data-retention", "Show data-retention information", None),
        (
            "help",
            "Show available commands and keyboard shortcuts",
            None,
        ),
        ("log", "Show session logging status", None),
        (
            "mcp",
            "Show MCP status, login guidance, or log out a server",
            Some("status | login <alias> | logout <alias>"),
        ),
        (
            "proxy-setup",
            "Configure proxy and TLS certificate settings",
            Some("KEY value to set, KEY to unset, or empty for help"),
        ),
        (
            "reload",
            "Reload configuration, agent instructions, and skills",
            None,
        ),
    ];
    if vibe_code_enabled {
        commands.push(("teleport", "Teleport session to Vibe Code Web", None));
    }
    commands
        .into_iter()
        .map(|(name, description, hint)| {
            let mut command = json!({
                "name": name,
                "description": description,
            });
            if let Some(hint) = hint {
                command["input"] = json!({"hint": hint});
            }
            command
        })
        .collect()
}

pub type AcpClientFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

pub trait AcpClientPort: Send + Sync {
    fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a>;
}

struct AcpClientToolFactory {
    client: Option<Arc<dyn AcpClientPort>>,
    capabilities: AcpClientCapabilities,
    timeout: Duration,
}

impl SessionToolFactory for AcpClientToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        for (tool_name, method, enabled, input_schema) in acp_client_tool_specs(&self.capabilities)
        {
            if !enabled {
                continue;
            }
            let client = self.client.clone();
            let timeout = self.timeout;
            let session_id = session_id.to_owned();
            let method = method.to_owned();
            let description = format!("Execute `{method}` through the connected ACP editor client");
            let handler = Arc::new(
                move |invocation: &ToolInvocation,
                      _output: ToolOutputSink|
                      -> OwnedToolHandlerFuture {
                    let client = client.clone();
                    let method = method.clone();
                    let session_id = session_id.clone();
                    let mut params = invocation.arguments.clone();
                    Box::pin(async move {
                        let Some(client) = client else {
                            return Err(ToolError::Unavailable(
                                "ACP client transport is unavailable".to_owned(),
                            ));
                        };
                        let object = params.as_object_mut().ok_or_else(|| {
                            ToolError::Execution(
                                "ACP client tool input must be an object".to_owned(),
                            )
                        })?;
                        object.insert("sessionId".to_owned(), json!(session_id));
                        let result = tokio::time::timeout(timeout, client.request(&method, params))
                            .await
                            .map_err(|_| {
                                ToolError::Execution(format!(
                                    "ACP client tool `{method}` timed out"
                                ))
                            })?
                            .map_err(ToolError::Execution)?;
                        let model_text = serde_json::to_string(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?;
                        Ok(ToolExecutionOutput {
                            typed_result: result,
                            model_text,
                            display: json!({"kind": "client_tool", "method": method}),
                            chunks: Vec::new(),
                        })
                    })
                },
            );
            tools
                .register(
                    ToolSpec {
                        name: tool_name.to_owned(),
                        description,
                        input_schema,
                        output_schema: None,
                        config: Value::Null,
                        state: Value::Null,
                        availability: ToolAvailability::Available,
                        presentation: ToolPresentationKind::Generic,
                        source: ToolSource::BuiltIn,
                        selection_priority: 30,
                    },
                    handler,
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn acp_client_tool_specs(
    capabilities: &AcpClientCapabilities,
) -> Vec<(&'static str, &'static str, bool, Value)> {
    let object = |properties: Value, required: Value| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": true,
        })
    };
    vec![
        (
            "acp_read_text_file",
            "fs/read_text_file",
            capabilities.fs.read_text_file,
            object(json!({"path": {"type": "string"}}), json!(["path"])),
        ),
        (
            "acp_write_text_file",
            "fs/write_text_file",
            capabilities.fs.write_text_file,
            object(
                json!({
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }),
                json!(["path", "content"]),
            ),
        ),
        (
            "acp_terminal_create",
            "terminal/create",
            capabilities.terminal,
            object(json!({"command": {"type": "string"}}), json!(["command"])),
        ),
        (
            "acp_terminal_output",
            "terminal/output",
            capabilities.terminal,
            object(
                json!({"terminalId": {"type": "string"}}),
                json!(["terminalId"]),
            ),
        ),
        (
            "acp_terminal_wait_for_exit",
            "terminal/wait_for_exit",
            capabilities.terminal,
            object(
                json!({"terminalId": {"type": "string"}}),
                json!(["terminalId"]),
            ),
        ),
        (
            "acp_terminal_kill",
            "terminal/kill",
            capabilities.terminal,
            object(
                json!({"terminalId": {"type": "string"}}),
                json!(["terminalId"]),
            ),
        ),
        (
            "acp_terminal_release",
            "terminal/release",
            capabilities.terminal,
            object(
                json!({"terminalId": {"type": "string"}}),
                json!(["terminalId"]),
            ),
        ),
    ]
}

struct AcpHarness<D>
where
    D: TurnDriver,
{
    service: tokio::sync::Mutex<HeadlessService<D>>,
    active_turn: Mutex<Option<String>>,
    cancel_requested: AtomicBool,
    cancel_notify: tokio::sync::Notify,
    mode_id: Mutex<String>,
    config_options: Mutex<BTreeMap<String, String>>,
    user_message_anchors: Mutex<BTreeMap<String, usize>>,
    next_user_message: AtomicU64,
    cwd: String,
}

impl<D> AcpHarness<D>
where
    D: TurnDriver,
{
    fn settings(&self) -> Result<SessionSettings, AcpError> {
        let mode_id = self
            .mode_id
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .clone();
        let thinking = self
            .config_options
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .get("thinking")
            .cloned()
            .unwrap_or_else(|| "medium".to_owned());
        Ok(SessionSettings { mode_id, thinking })
    }

    fn next_ephemeral_user_message_id(&self, session_id: &str) -> String {
        let sequence = self.next_user_message.fetch_add(1, Ordering::Relaxed);
        format!("{session_id}:user:{sequence}")
    }

    fn record_user_message_anchor(
        &self,
        message_id: String,
        history_before: usize,
        history_after: &[Value],
        prompt: &str,
    ) -> Result<(), AcpError> {
        let anchor = history_after
            .iter()
            .enumerate()
            .skip(history_before)
            .rev()
            .find(|(_, entry)| {
                entry.get("role").and_then(Value::as_str) == Some("user")
                    && entry.get("content").and_then(Value::as_str) == Some(prompt)
            })
            .map(|(index, _)| index);
        if let Some(anchor) = anchor {
            self.user_message_anchors
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?
                .insert(message_id, anchor);
        }
        Ok(())
    }

    fn user_message_anchor(&self, message_id: &str) -> Result<Option<usize>, AcpError> {
        Ok(self
            .user_message_anchors
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .get(message_id)
            .copied())
    }
}

struct AgentState<D>
where
    D: TurnDriver,
{
    initialized: bool,
    authenticated: bool,
    client_capabilities: AcpClientCapabilities,
    client_info: Option<AcpClientInfo>,
    sessions: BTreeMap<String, Arc<AcpHarness<D>>>,
    loading_sessions: BTreeSet<String>,
    closing_sessions: BTreeSet<String>,
    closed_sessions: BTreeSet<String>,
    next_session: u64,
}

pub struct AcpAgent<D>
where
    D: TurnDriver,
{
    driver: Arc<D>,
    state: Mutex<AgentState<D>>,
    client: Option<Arc<dyn AcpClientPort>>,
    client_tool_timeout: Duration,
    session_root: Option<PathBuf>,
    credential_environment: String,
    production_cloud: bool,
    release4: Mutex<Option<Release4Service>>,
    next_teleport: AtomicU64,
}

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    pub fn new(driver: D) -> Result<Self, AcpError> {
        Ok(Self {
            driver: Arc::new(driver),
            state: Mutex::new(AgentState {
                initialized: false,
                authenticated: false,
                client_capabilities: AcpClientCapabilities::default(),
                client_info: None,
                sessions: BTreeMap::new(),
                loading_sessions: BTreeSet::new(),
                closing_sessions: BTreeSet::new(),
                closed_sessions: BTreeSet::new(),
                next_session: 1,
            }),
            client: None,
            client_tool_timeout: DEFAULT_CLIENT_TOOL_TIMEOUT,
            session_root: None,
            credential_environment: "MISTRAL_API_KEY".to_owned(),
            production_cloud: false,
            release4: Mutex::new(None),
            next_teleport: AtomicU64::new(1),
        })
    }

    #[must_use]
    pub fn with_client_port(mut self, client: Arc<dyn AcpClientPort>, timeout: Duration) -> Self {
        self.client = Some(client);
        self.client_tool_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_session_root(mut self, session_root: impl Into<PathBuf>) -> Self {
        self.session_root = Some(session_root.into());
        self
    }

    #[must_use]
    pub fn with_credential_environment(
        mut self,
        credential_environment: impl Into<String>,
    ) -> Self {
        self.credential_environment = credential_environment.into();
        self
    }

    #[must_use]
    pub fn with_production_cloud(mut self) -> Self {
        self.production_cloud = true;
        self
    }

    #[must_use]
    pub fn with_release4_service(mut self, service: Release4Service) -> Self {
        self.production_cloud = true;
        self.release4 = Mutex::new(Some(service));
        self
    }

    fn new_service(
        &self,
        working_directory: &str,
        additional_directories: &[String],
    ) -> Result<HeadlessService<D>, AcpError> {
        let (capabilities, client_info) = {
            let state = self.lock_state()?;
            (state.client_capabilities.clone(), state.client_info.clone())
        };
        let tools = Arc::new(AcpClientToolFactory {
            client: self.client.clone(),
            capabilities: capabilities.clone(),
            timeout: self.client_tool_timeout,
        });
        let mut client_tools = Vec::new();
        if capabilities.fs.read_text_file {
            client_tools.push(ClientToolCapability::FilesystemRead);
        }
        if capabilities.fs.write_text_file {
            client_tools.push(ClientToolCapability::FilesystemWrite);
        }
        if capabilities.terminal {
            client_tools.push(ClientToolCapability::Terminal);
        }
        let mut server = AppServer::default().using_session_tool_factory(tools);
        if let Some(release4) = self.production_release4()? {
            server = server.using_release4_service(release4);
        }
        if let Some(session_root) = &self.session_root {
            let release3 = Release3Service::for_runtime_session_root(
                session_root,
                Path::new(working_directory),
            )
            .with_allowed_roots(additional_directories.iter().map(PathBuf::from).collect());
            server = server.using_release3_service(release3);
        }
        Ok(
            HeadlessService::new_interactive_shared_with_server_and_client(
                self.driver.clone(),
                server,
                ClientInfo {
                    name: client_info
                        .as_ref()
                        .map_or_else(|| "vibe_acp_client".to_owned(), |info| info.name.clone()),
                    version: client_info
                        .as_ref()
                        .map_or_else(|| "unknown".to_owned(), |info| info.version.clone()),
                    title: client_info.and_then(|info| info.title),
                    entrypoint: ClientEntrypoint::Acp,
                    terminal_emulator: TerminalEmulator::Unknown,
                },
                ClientCapabilities {
                    callback_kinds: self
                        .client
                        .is_some()
                        .then_some(CallbackKind::Approval)
                        .into_iter()
                        .collect(),
                    client_tools,
                    disabled_notifications: Vec::new(),
                },
            )?,
        )
    }

    fn production_release4(&self) -> Result<Option<Release4Service>, AcpError> {
        if !self.production_cloud {
            return Ok(None);
        }
        let mut cached = self.release4.lock().map_err(|_| AcpError::StatePoisoned)?;
        if let Some(service) = cached.as_ref() {
            return Ok(Some(service.clone()));
        }
        let Some(api_key) = std::env::var(&self.credential_environment)
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let config = VibeCodeCloudConfig::from_credential(api_key)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        let service = Release4Service::production(config)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        *cached = Some(service.clone());
        Ok(Some(service))
    }

    pub fn initialize(&self) -> Result<AcpInitializeResponse, AcpError> {
        self.initialize_with(AcpInitializeRequest::default())
    }

    #[must_use]
    pub fn advertised_commands(&self) -> Vec<Value> {
        command_catalog(self.production_cloud)
    }

    pub fn initialize_with(
        &self,
        request: AcpInitializeRequest,
    ) -> Result<AcpInitializeResponse, AcpError> {
        if request.protocol_version != ACP_PROTOCOL_VERSION {
            return Err(AcpError::UnsupportedProtocol(request.protocol_version));
        }
        let mut state = self.lock_state()?;
        if state.initialized {
            return Err(AcpError::AlreadyInitialized);
        }
        state.initialized = true;
        state.client_capabilities = request.client_capabilities;
        state.client_info = request.client_info;
        Ok(AcpInitializeResponse {
            protocol_version: ACP_PROTOCOL_VERSION,
            agent_capabilities: AcpAgentCapabilities {
                load_session: true,
                prompt_capabilities: AcpPromptCapabilities {
                    audio: false,
                    embedded_context: true,
                    image: true,
                },
                session_capabilities: json!({
                    "list": {},
                    "fork": {},
                    "close": {},
                }),
            },
            auth_methods: vec![json!({
                "type": "env_var",
                "id": "environment",
                "name": "Mistral API key",
                "description": "Provide the configured provider credential through the environment.",
                "vars": [{
                    "name": self.credential_environment,
                    "label": "API key",
                    "secret": true,
                    "optional": false,
                }],
            })],
            agent_info: AcpImplementation {
                name: "@mistralai/mistral-vibe".to_owned(),
                title: "Mistral Vibe".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        })
    }

    pub fn authenticate(&self, method_id: &str) -> Result<(), AcpError> {
        self.require_initialized()?;
        if method_id != "environment" {
            return Err(AcpError::UnsupportedAuthentication(method_id.to_owned()));
        }
        self.lock_state()?.authenticated = true;
        Ok(())
    }

    pub fn new_session(&self, request: AcpNewSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(&request.cwd, request.additional_directories.as_deref())?;
        let additional_directories = request.additional_directories.unwrap_or_default();
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let session_id = {
            let mut state = self.lock_state()?;
            let id = format!(
                "acp-session-{:x}-{:x}-{:x}",
                now_millis(),
                std::process::id(),
                state.next_session
            );
            state.next_session = state.next_session.saturating_add(1);
            id
        };
        let settings = SessionSettings::default();
        let mut service = self.new_service(&request.cwd, &additional_directories)?;
        let app_session_id = service.start_session(&session_options(
            &request.cwd,
            Some(session_id.clone()),
            additional_directories,
            mcp_servers,
            None,
            &settings,
        ))?;
        let harness = harness_from_service(service, &app_session_id)?;
        let settings = harness.settings()?;
        let harness = Arc::new(harness);
        let mut state = self.lock_state()?;
        state.closed_sessions.remove(&app_session_id);
        state.sessions.insert(app_session_id.clone(), harness);
        Ok(session_response(app_session_id, &settings))
    }

    pub fn load_session(&self, request: AcpLoadSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(&request.cwd, Some(&request.additional_directories))?;
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let requested_session_id = request.session_id.clone();
        self.reserve_session_load(&requested_session_id)?;
        let loaded = (|| {
            let mut probe = self.new_service(&request.cwd, &request.additional_directories)?;
            let result =
                probe.public_call("session/resume", json!({"sessionId": request.session_id}))?;
            let session_id = metadata_session_id(&result)?;
            if session_id != requested_session_id {
                return Err(AcpError::InvalidResponse(format!(
                    "requested session `{requested_session_id}` resumed as `{session_id}`"
                )));
            }
            let persisted_cwd = probe.session(&session_id)?.working_directory;
            ensure_matching_cwd(&request.cwd, &persisted_cwd, "load")?;
            let settings = SessionSettings::from_release3_result(&result);
            probe.shutdown()?;

            let mut service = self.new_service(&request.cwd, &request.additional_directories)?;
            let attached_id = service.start_session(&session_options(
                &request.cwd,
                None,
                request.additional_directories,
                mcp_servers,
                Some(session_id.clone()),
                &settings,
            ))?;
            if attached_id != session_id {
                return Err(AcpError::InvalidResponse(format!(
                    "loaded session `{session_id}` was attached as `{attached_id}`"
                )));
            }
            let harness = harness_from_service(service, &session_id)?;
            let settings = harness.settings()?;
            Ok((session_id, settings, Arc::new(harness)))
        })();

        let (session_id, settings, harness) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.lock_state()?
                    .loading_sessions
                    .remove(&requested_session_id);
                return Err(error);
            }
        };
        let mut state = self.lock_state()?;
        if !state.loading_sessions.remove(&requested_session_id) || !state.initialized {
            return Err(AcpError::NotInitialized);
        }
        state.closed_sessions.remove(&session_id);
        state.sessions.insert(session_id.clone(), harness);
        Ok(session_response(session_id, &settings))
    }

    pub fn list_sessions(
        &self,
        cwd: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<AcpSessionList, AcpError> {
        self.require_initialized()?;
        if let Some(cwd) = cwd
            && !Path::new(cwd).is_absolute()
        {
            return Err(AcpError::InvalidParams(
                "cwd must be an absolute path".to_owned(),
            ));
        }
        let offset = decode_session_cursor(cursor)?;
        let working_directory = cwd.map_or_else(
            || {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .to_string_lossy()
                    .into_owned()
            },
            ToOwned::to_owned,
        );
        let mut service = self.new_service(&working_directory, &[])?;
        let mut saved = Vec::new();
        let mut app_offset = 0_usize;
        loop {
            let mut params = json!({
                "offset": app_offset,
                "limit": 500,
            });
            if let Some(cwd) = cwd {
                params["cwd"] = json!(cwd);
            }
            let result = service.public_call("session/list", params)?;
            saved.extend(
                result
                    .get("sessions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            if saved.len() > MAX_SESSION_LIST_SCAN {
                return Err(AcpError::InvalidResponse(format!(
                    "session/list exceeded the {MAX_SESSION_LIST_SCAN}-session scan limit"
                )));
            }
            let Some(next_offset) = result.get("nextOffset").and_then(Value::as_u64) else {
                break;
            };
            let next_offset = usize::try_from(next_offset).map_err(|_| {
                AcpError::InvalidResponse("session/list nextOffset overflowed".to_owned())
            })?;
            if next_offset <= app_offset {
                return Err(AcpError::InvalidResponse(
                    "session/list nextOffset did not advance".to_owned(),
                ));
            }
            app_offset = next_offset;
        }
        service.shutdown()?;

        let mut sessions = saved
            .iter()
            .map(acp_session_info)
            .collect::<Result<Vec<_>, _>>()?;
        let active = self.lock_state()?;
        for (session_id, harness) in &active.sessions {
            if cwd.is_some_and(|filter| !same_path(filter, &harness.cwd)) {
                continue;
            }
            if let Some(session) = sessions
                .iter_mut()
                .find(|session| session.session_id == *session_id)
            {
                session.meta.insert("active".to_owned(), json!(true));
            } else {
                sessions.push(AcpSessionInfo {
                    session_id: session_id.clone(),
                    cwd: harness.cwd.clone(),
                    title: None,
                    updated_at: None,
                    additional_directories: None,
                    meta: BTreeMap::from([("active".to_owned(), json!(true))]),
                });
            }
        }
        drop(active);

        let start = offset.min(sessions.len());
        let end = start
            .saturating_add(SESSION_LIST_PAGE_SIZE)
            .min(sessions.len());
        let page = sessions
            .get(start..end)
            .map_or_else(Vec::new, <[AcpSessionInfo]>::to_vec);
        Ok(AcpSessionList {
            sessions: page,
            next_cursor: (end < sessions.len()).then(|| encode_session_cursor(end)),
        })
    }

    pub fn fork_session(&self, request: AcpForkSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(&request.cwd, Some(&request.additional_directories))?;
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let mut probe = self.new_service(&request.cwd, &request.additional_directories)?;
        let source =
            probe.public_call("session/resume", json!({"sessionId": request.session_id}))?;
        let source_id = metadata_session_id(&source)?;
        let source_cwd = probe.session(&source_id)?.working_directory;
        ensure_matching_cwd(&request.cwd, &source_cwd, "fork")?;
        let source_history = all_history_entries(&mut probe, &source_id)?;
        let source_keep_messages = request
            .message_id
            .as_deref()
            .map(|message_id| {
                let anchor = self.resolve_user_message_anchor(&request.session_id, message_id)?;
                fork_source_keep_messages(&source_history, anchor, message_id)
            })
            .transpose()?;
        let settings = self
            .live_session_settings(&request.session_id)?
            .unwrap_or_else(|| SessionSettings::from_release3_result(&source));
        let mut fork_params = json!({
            "sessionId": source_id,
            "newSessionId": request.new_session_id,
            "config": settings.as_config(),
        });
        if let Some(source_keep_messages) = source_keep_messages {
            fork_params["keepMessages"] = json!(source_keep_messages);
        }
        let result = probe.public_call("session/fork", fork_params)?;
        let session_id = metadata_session_id(&result)?;
        probe.shutdown()?;

        let mut service = self.new_service(&request.cwd, &request.additional_directories)?;
        let attached_id = service.start_session(&session_options(
            &request.cwd,
            None,
            request.additional_directories,
            mcp_servers,
            Some(session_id.clone()),
            &settings,
        ))?;
        if attached_id != session_id {
            return Err(AcpError::InvalidResponse(format!(
                "forked session `{session_id}` was attached as `{attached_id}`"
            )));
        }
        let harness = harness_from_service(service, &session_id)?;
        let settings = harness.settings()?;
        let harness = Arc::new(harness);
        let mut state = self.lock_state()?;
        state.closed_sessions.remove(&session_id);
        state.sessions.insert(session_id.clone(), harness);
        Ok(session_response(session_id, &settings))
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        if !matches!(mode_id, "code" | "plan") {
            return Err(AcpError::InvalidParams(format!(
                "unknown session mode `{mode_id}`"
            )));
        }
        let harness = self.session_harness(session_id)?;
        {
            let mut service = harness.service.lock().await;
            service.public_call(
                "session/settings/update",
                json!({"sessionId": session_id, "mode": mode_id}),
            )?;
        }
        *harness
            .mode_id
            .lock()
            .map_err(|_| AcpError::StatePoisoned)? = mode_id.to_owned();
        Ok(())
    }

    pub async fn set_config_option(
        &self,
        session_id: &str,
        option_id: &str,
        value: &str,
    ) -> Result<Vec<Value>, AcpError> {
        if option_id != "thinking" {
            return Err(AcpError::InvalidParams(
                "unknown config option; expected `thinking`".to_owned(),
            ));
        }
        if !matches!(value, "off" | "low" | "medium" | "high" | "max") {
            return Err(AcpError::InvalidParams(
                "thinking must be off, low, medium, high, or max".to_owned(),
            ));
        }
        let harness = self.session_harness(session_id)?;
        {
            let mut service = harness.service.lock().await;
            let mut params = json!({
                "sessionId": session_id,
                "thinking": value != "off",
            });
            if value != "off" {
                params["reasoningEffort"] = json!(value);
            }
            service.public_call("session/settings/update", params)?;
        }
        harness
            .config_options
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .insert(option_id.to_owned(), value.to_owned());
        Ok(thinking_config_options(value))
    }

    pub async fn history(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<AcpHistoryPage, AcpError> {
        if !(1..=MAX_HISTORY_PAGE_SIZE).contains(&limit) {
            return Err(AcpError::InvalidParams(format!(
                "history limit must be between 1 and {MAX_HISTORY_PAGE_SIZE}"
            )));
        }
        let harness = self.session_harness(session_id)?;
        let mut service = harness.service.lock().await;
        let result = service.public_call(
            "history/list",
            json!({"sessionId": session_id, "offset": offset, "limit": limit}),
        )?;
        let entries = result
            .get("history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(AcpHistoryPage {
            next_offset: (entries.len() == limit).then_some(offset.saturating_add(entries.len())),
            entries,
        })
    }

    pub async fn prompt(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> Result<Vec<AcpSessionUpdate>, AcpError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);
        let prompt = self.prompt_streaming(session_id, prompt, sender);
        tokio::pin!(prompt);
        let mut updates = Vec::new();
        loop {
            tokio::select! {
                response = &mut prompt => {
                    let _response = response?;
                    break;
                }
                update = receiver.recv() => {
                    let Some(update) = update else {
                        return Err(AcpError::Disconnected);
                    };
                    updates.push(update);
                }
            }
        }
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        Ok(updates)
    }

    pub async fn prompt_streaming(
        &self,
        session_id: &str,
        prompt: &str,
        sender: tokio::sync::mpsc::Sender<AcpSessionUpdate>,
    ) -> Result<AcpPromptResponse, AcpError> {
        self.prompt_content_streaming(
            session_id,
            vec![json!({"type": "text", "text": prompt})],
            sender,
        )
        .await
    }

    pub async fn prompt_content_streaming(
        &self,
        session_id: &str,
        content: Vec<Value>,
        sender: tokio::sync::mpsc::Sender<AcpSessionUpdate>,
    ) -> Result<AcpPromptResponse, AcpError> {
        self.require_initialized()?;
        let harness = self.session_harness(session_id)?;
        if let Some(command) = builtin_command(&content, &self.advertised_commands()) {
            return self
                .execute_builtin_command(session_id, &command, &sender, &harness)
                .await;
        }
        let mut turn_request = turn_request(content)?;
        {
            let mut active = harness
                .active_turn
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?;
            if active.is_some() {
                return Err(AcpError::SessionConflict(format!(
                    "session `{session_id}` already has active work"
                )));
            }
            *active = Some("reserving".to_owned());
        }
        let reserved: Result<_, AcpError> = {
            let mut service = harness.service.lock().await;
            let history_before = match all_history_entries(&mut service, session_id) {
                Ok(history) => Ok(Some(history.len())),
                Err(AcpError::Client(ClientError::Protocol(
                    vibe_protocol::ProtocolErrorCode::NotFound,
                    _,
                ))) => Ok(None),
                Err(error) => Err(error),
            };
            match history_before {
                Ok(history_before) => {
                    let user_message_id = history_before.map_or_else(
                        || harness.next_ephemeral_user_message_id(session_id),
                        |index| format!("history:{index}:user"),
                    );
                    turn_request.client_user_message_id = Some(user_message_id.clone());
                    let reservation = service
                        .reserve_prompt(session_id, &turn_request)
                        .await
                        .map_err(AcpError::from);
                    let driver = service.driver();
                    match reservation {
                        Ok(reservation) => {
                            match service.interactive_update_channel_after(
                                &reservation.session_id,
                                &reservation.turn_id,
                                0,
                            ) {
                                Ok((observer, updates)) => Ok((
                                    reservation,
                                    driver,
                                    observer,
                                    updates,
                                    history_before,
                                    user_message_id,
                                )),
                                Err(error) => {
                                    let _ = service.fail_reserved(&reservation, &error.to_string());
                                    Err(error.into())
                                }
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        };
        let (reservation, driver, observer, mut updates, history_before, user_message_id) =
            match reserved {
                Ok(reserved) => reserved,
                Err(error) => {
                    clear_active(&harness)?;
                    return Err(error);
                }
            };
        {
            let mut active = harness
                .active_turn
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?;
            *active = Some(reservation.turn_id.clone());
        }
        if harness.cancel_requested.swap(false, Ordering::AcqRel) {
            if let Err(error) = driver.interrupt(session_id, &reservation.turn_id) {
                let error = AcpError::Driver(error.to_string());
                let mut service = harness.service.lock().await;
                let _ = service.fail_reserved(&reservation, &error.to_string());
                clear_active(&harness)?;
                return Err(error);
            }
        }
        let run = driver.run_observed(&reservation, observer);
        tokio::pin!(run);
        let mut update_projection =
            AcpUpdateProjection::with_user_message_id(user_message_id.clone());
        let mut callback_poll = tokio::time::interval(Duration::from_millis(5));
        callback_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let outcome = loop {
            tokio::select! {
                biased;
                outcome = &mut run => break outcome,
                update = updates.recv() => {
                    let Some(update) = update else {
                        let error = AcpError::Disconnected;
                        let _ = driver.interrupt(session_id, &reservation.turn_id);
                        let mut service = harness.service.lock().await;
                        let _ = service.fail_reserved(&reservation, &error.to_string());
                        clear_active(&harness)?;
                        return Err(error);
                    };
                    if let Err(error) = send_acp_updates(
                            session_id,
                            update,
                            &mut update_projection,
                            &sender,
                        ) {
                        let _ = driver.interrupt(session_id, &reservation.turn_id);
                        let mut service = harness.service.lock().await;
                        let _ = service.fail_reserved(&reservation, &error.to_string());
                        clear_active(&harness)?;
                        return Err(error);
                    }
                }
                _ = callback_poll.tick() => {
                    if let Err(error) = self.route_pending_callbacks(session_id, &harness).await {
                        let _ = driver.interrupt(session_id, &reservation.turn_id);
                        let mut service = harness.service.lock().await;
                        let _ = service.fail_reserved(&reservation, &error.to_string());
                        clear_active(&harness)?;
                        return Err(error);
                    }
                }
            }
        };
        while let Ok(update) = updates.try_recv() {
            if let Err(error) =
                send_acp_updates(session_id, update, &mut update_projection, &sender)
            {
                let _ = driver.interrupt(session_id, &reservation.turn_id);
                let mut service = harness.service.lock().await;
                let _ = service.fail_reserved(&reservation, &error.to_string());
                clear_active(&harness)?;
                return Err(error);
            }
        }
        let turn = {
            let mut service = harness.service.lock().await;
            match outcome {
                Ok(outcome) => {
                    let turn = match service.finish_reserved(&reservation, outcome) {
                        Ok(turn) => turn,
                        Err(error) => {
                            let _ = service.fail_reserved(&reservation, &error.to_string());
                            clear_active(&harness)?;
                            return Err(error.into());
                        }
                    };
                    if let (Some(history_before), Ok(history)) = (
                        history_before,
                        all_history_entries(&mut service, session_id),
                    ) && let Err(error) = harness.record_user_message_anchor(
                        user_message_id,
                        history_before,
                        &history,
                        &turn_request.prompt,
                    ) {
                        clear_active(&harness)?;
                        return Err(error);
                    }
                    turn
                }
                Err(error) => {
                    if let Err(fail_error) = service.fail_reserved(&reservation, &error.to_string())
                    {
                        clear_active(&harness)?;
                        return Err(fail_error.into());
                    }
                    clear_active(&harness)?;
                    return Err(AcpError::Driver(error.to_string()));
                }
            }
        };
        clear_active(&harness)?;
        send_usage(session_id, &turn, &sender)?;
        Ok(AcpPromptResponse {
            stop_reason: acp_stop_reason(&turn).to_owned(),
            usage: AcpUsage {
                total_tokens: turn
                    .usage
                    .input_tokens
                    .saturating_add(turn.usage.output_tokens),
                input_tokens: turn.usage.input_tokens,
                output_tokens: turn.usage.output_tokens,
            },
        })
    }

    async fn route_pending_callbacks(
        &self,
        session_id: &str,
        harness: &Arc<AcpHarness<D>>,
    ) -> Result<(), AcpError> {
        let callbacks = {
            let mut service = harness.service.lock().await;
            service.drain_callbacks()?
        };
        for callback in callbacks {
            let PublicHistoryEntry::Callback {
                metadata,
                callback_id,
                detail,
                state: PublicCallbackState::Open,
                title,
            } = callback
            else {
                return Err(AcpError::InvalidResponse(
                    "interactive callback drain returned a non-open callback".to_owned(),
                ));
            };
            if metadata.session_id != session_id {
                return Err(AcpError::InvalidResponse(format!(
                    "callback `{callback_id}` crossed from session `{}` into `{session_id}`",
                    metadata.session_id
                )));
            }
            let output = match detail.get("kind").and_then(Value::as_str) {
                Some("approval") => {
                    self.resolve_approval_callback(
                        session_id,
                        &callback_id,
                        &title,
                        &detail,
                        harness,
                    )
                    .await
                }
                Some("user_input") => json!({
                    "type": "user_input",
                    "result": {
                        "answers": [],
                        "cancelled": true,
                    },
                }),
                Some(kind) => {
                    return Err(AcpError::InvalidResponse(format!(
                        "unsupported canonical callback kind `{kind}`"
                    )));
                }
                None => {
                    return Err(AcpError::InvalidResponse(format!(
                        "callback `{callback_id}` omitted its kind"
                    )));
                }
            };
            let mut service = harness.service.lock().await;
            service.respond_callback(json!({
                "sessionId": session_id,
                "callbackId": callback_id,
                "output": output,
            }))?;
        }
        Ok(())
    }

    async fn resolve_approval_callback(
        &self,
        session_id: &str,
        callback_id: &str,
        title: &str,
        detail: &Value,
        harness: &Arc<AcpHarness<D>>,
    ) -> Value {
        let options = acp_approval_options(detail);
        if options.is_empty() {
            return cancelled_approval_output();
        }
        let permission = self.request_permission(
            session_id,
            json!({
                "toolCallId": callback_id,
                "title": title,
                "kind": "execute",
                "rawInput": detail,
                "_meta": {
                    "callbackId": callback_id,
                    "toolName": detail.pointer("/effect/toolName"),
                },
            }),
            options.clone(),
        );
        tokio::pin!(permission);
        let response = tokio::select! {
            response = &mut permission => response.ok(),
            () = wait_for_cancel(harness) => None,
        };
        approval_output_from_acp(response.as_ref(), &options)
    }

    async fn execute_builtin_command(
        &self,
        session_id: &str,
        command: &BuiltinCommand,
        sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
        harness: &Arc<AcpHarness<D>>,
    ) -> Result<AcpPromptResponse, AcpError> {
        {
            let mut active = harness
                .active_turn
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?;
            if active.is_some() {
                return Err(AcpError::SessionConflict(format!(
                    "session `{session_id}` already has active work"
                )));
            }
            *active = Some("builtin-command".to_owned());
        }
        harness.cancel_requested.store(false, Ordering::Release);
        let result = self
            .execute_builtin_command_inner(session_id, command, sender, harness)
            .await;
        harness.cancel_requested.store(false, Ordering::Release);
        clear_active(harness)?;
        result
    }

    async fn execute_builtin_command_inner(
        &self,
        session_id: &str,
        command: &BuiltinCommand,
        sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
        harness: &Arc<AcpHarness<D>>,
    ) -> Result<AcpPromptResponse, AcpError> {
        send_command_chunk(session_id, "user_message_chunk", &command.raw, sender)?;
        match command.name.as_str() {
            "help" => {
                let lines = self
                    .advertised_commands()
                    .into_iter()
                    .filter_map(|command| {
                        Some(format!(
                            "- `/{}`: {}",
                            command.get("name")?.as_str()?,
                            command.get("description")?.as_str()?
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                send_command_reply(
                    session_id,
                    &format!("### Available Commands\n\n{lines}"),
                    sender,
                )?;
            }
            "compact" => {
                let mut service = harness.service.lock().await;
                let history = service.public_call(
                    "history/list",
                    json!({"sessionId": session_id, "offset": 0, "limit": 1}),
                )?;
                if history
                    .get("history")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty)
                {
                    send_command_reply(
                        session_id,
                        "No conversation history to compact yet.",
                        sender,
                    )?;
                    return Ok(command_response());
                }
                let call_id = format!("compact-{}", now_millis());
                send_command_tool_update(
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call_id,
                        "title": "Compacting conversation context",
                        "status": "in_progress",
                    }),
                    sender,
                )?;
                if let Err(error) = service.compact(session_id, &command.arguments).await {
                    send_command_tool_update(
                        session_id,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": call_id,
                            "title": "Compaction failed",
                            "status": "failed",
                            "rawOutput": error.to_string(),
                        }),
                        sender,
                    )?;
                    return Err(error.into());
                }
                send_command_tool_update(
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": call_id,
                        "title": "Conversation context compacted",
                        "status": "completed",
                    }),
                    sender,
                )?;
            }
            "reload" => {
                let mut service = harness.service.lock().await;
                match service.public_call("config/reload", json!({})) {
                    Ok(_) => send_command_reply(
                        session_id,
                        "Configuration reloaded (including agent instructions and skills).",
                        sender,
                    )?,
                    Err(error) => send_command_reply(
                        session_id,
                        &format!("Failed to reload configuration: {error}"),
                        sender,
                    )?,
                }
            }
            "log" => {
                send_command_reply(
                    session_id,
                    "Session logging paths are not exposed by this configuration.",
                    sender,
                )?;
            }
            "mcp" => {
                let reply = self
                    .execute_mcp_command(session_id, &command.arguments, harness)
                    .await;
                send_command_reply(session_id, &reply, sender)?;
            }
            "teleport" if self.production_cloud => {
                self.execute_teleport_command(session_id, sender, harness)
                    .await?;
            }
            "proxy-setup" => {
                let reply = execute_proxy_command(&command.arguments, harness).await;
                send_command_reply(session_id, &reply, sender)?;
            }
            "data-retention" => {
                send_command_reply(
                    session_id,
                    "Data retention is controlled by the configured model provider and any connected services. Local session transcripts remain under the configured Vibe session directory.",
                    sender,
                )?;
            }
            _ => {
                return Err(AcpError::InvalidParams(format!(
                    "unknown built-in command `{}`",
                    command.name
                )));
            }
        }
        Ok(command_response())
    }

    async fn execute_teleport_command(
        &self,
        session_id: &str,
        sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
        harness: &Arc<AcpHarness<D>>,
    ) -> Result<(), AcpError> {
        let sequence = self.next_teleport.fetch_add(1, Ordering::Relaxed);
        let timestamp = now_millis();
        let call_id = format!("teleport-{timestamp}-{}-{sequence}", std::process::id());
        let mut service = harness.service.lock().await;
        let history = match all_history_entries(&mut service, session_id) {
            Ok(history) if !history.is_empty() => history,
            Ok(_) => {
                send_command_reply(session_id, "No conversation history to teleport.", sender)?;
                return Ok(());
            }
            Err(error) => {
                send_command_reply(
                    session_id,
                    &format!("Teleport could not read conversation history: {error}"),
                    sender,
                )?;
                return Ok(());
            }
        };
        let summary = teleport_summary(&history);
        send_command_tool_update(
            session_id,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": "Teleporting session to Vibe Code",
                "kind": "other",
                "status": "in_progress",
                "_meta": {"tool_name": "teleport", "teleport": {"status": "starting"}},
            }),
            sender,
        )?;

        let opened = tokio::select! {
            opened = service.public_call_async(
                "vibeCode/projects/open",
                json!({
                    "sessionId": session_id,
                    "workingDirectory": harness.cwd,
                    "purpose": "teleport",
                }),
            ) => opened,
            () = wait_for_cancel(harness) => {
                send_teleport_cancelled(session_id, &call_id, sender)?;
                return Ok(());
            }
        };
        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                send_teleport_failed(session_id, &call_id, &error.to_string(), sender)?;
                return Ok(());
            }
        };
        let Some(picker_id) = opened.result.get("pickerId").and_then(Value::as_str) else {
            send_teleport_failed(
                session_id,
                &call_id,
                "Vibe Code project discovery omitted its picker ID",
                sender,
            )?;
            return Ok(());
        };
        let Some(project_id) = opened
            .result
            .get("resolvedProjectId")
            .and_then(Value::as_str)
        else {
            send_teleport_failed(
                session_id,
                &call_id,
                "No Vibe Code project is linked to this repository.",
                sender,
            )?;
            return Ok(());
        };
        let picker_id = picker_id.to_owned();
        let project_id = project_id.to_owned();
        let operation_id = format!("teleport-acp-{timestamp}-{}-{sequence}", std::process::id());
        let started = tokio::select! {
            started = service.public_call_async(
                "vibeCode/teleport/start",
                json!({
                    "sessionId": session_id,
                    "pickerId": picker_id,
                    "projectId": project_id,
                    "operationId": operation_id,
                    "workingDirectory": harness.cwd,
                    "prompt": summary,
                }),
            ) => started,
            () = wait_for_cancel(harness) => {
                let _ = service.public_call(
                    "vibeCode/teleport/cancel",
                    json!({"sessionId": session_id, "operationId": operation_id}),
                );
                send_teleport_cancelled(session_id, &call_id, sender)?;
                return Ok(());
            }
        };
        let started = match started {
            Ok(started) => started,
            Err(error) => {
                send_teleport_failed(session_id, &call_id, &error.to_string(), sender)?;
                return Ok(());
            }
        };
        let events = teleport_events_from_notifications(&started.notifications)?;
        emit_teleport_events(session_id, &call_id, &events, sender)?;
        if events
            .iter()
            .any(|event| matches!(event, ProgrammaticTeleportEvent::PushRequired { .. }))
        {
            drop(service);
            let permission = tokio::select! {
                permission = self.request_permission(
                    session_id,
                    json!({
                        "toolCallId": call_id,
                        "title": "Push local commits before Teleport?",
                        "kind": "execute",
                        "rawInput": {"operationId": operation_id},
                        "_meta": {"tool_name": "teleport", "teleport": {"status": "push_required"}},
                    }),
                    vec![
                        json!({
                            "optionId": "teleport_push_and_continue",
                            "name": "Push and continue",
                            "kind": "allow_once",
                        }),
                        json!({
                            "optionId": "teleport_cancel",
                            "name": "Cancel Teleport",
                            "kind": "reject_once",
                        }),
                    ],
                ) => permission,
                () = wait_for_cancel(harness) => {
                    let mut service = harness.service.lock().await;
                    let _ = service.public_call(
                        "vibeCode/teleport/cancel",
                        json!({"sessionId": session_id, "operationId": operation_id}),
                    );
                    send_teleport_cancelled(session_id, &call_id, sender)?;
                    return Ok(());
                }
            };
            let approved = permission
                .ok()
                .and_then(|response| {
                    (response.pointer("/outcome/outcome").and_then(Value::as_str)
                        == Some("selected"))
                    .then(|| {
                        response
                            .pointer("/outcome/optionId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
                })
                .as_deref()
                == Some("teleport_push_and_continue");
            let mut service = harness.service.lock().await;
            let responded = tokio::select! {
                responded = service.public_call_async(
                    "vibeCode/teleport/push/respond",
                    json!({
                        "sessionId": session_id,
                        "operationId": operation_id,
                        "approved": approved,
                    }),
                ) => responded,
                () = wait_for_cancel(harness) => {
                    let _ = service.public_call(
                        "vibeCode/teleport/cancel",
                        json!({"sessionId": session_id, "operationId": operation_id}),
                    );
                    send_teleport_cancelled(session_id, &call_id, sender)?;
                    return Ok(());
                }
            };
            match responded {
                Ok(responded) => {
                    let continued = teleport_events_from_notifications(&responded.notifications)?;
                    emit_teleport_events(session_id, &call_id, &continued, sender)?;
                }
                Err(error) => {
                    send_teleport_failed(session_id, &call_id, &error.to_string(), sender)?;
                }
            }
        }
        Ok(())
    }

    async fn execute_mcp_command(
        &self,
        session_id: &str,
        arguments: &str,
        harness: &Arc<AcpHarness<D>>,
    ) -> String {
        let parts = arguments.split_whitespace().collect::<Vec<_>>();
        let (method, alias) = match parts.as_slice() {
            [] | ["status"] => ("mcp/read", None),
            ["login", alias] => ("mcp/login", Some(*alias)),
            ["logout", alias] => ("mcp/logout", Some(*alias)),
            ["status", ..] => return "Usage: `/mcp status`".to_owned(),
            ["login"] => return "Usage: `/mcp login <alias>`".to_owned(),
            ["logout"] => return "Usage: `/mcp logout <alias>`".to_owned(),
            _ => {
                return "Usage: `/mcp status`, `/mcp login <alias>`, or `/mcp logout <alias>`"
                    .to_owned();
            }
        };
        let mut params = json!({"sessionId": session_id});
        if let Some(alias) = alias {
            params["alias"] = json!(alias);
        }
        let mut service = harness.service.lock().await;
        match service.public_call_async(method, params).await {
            Ok(dispatch) if method == "mcp/read" => {
                let state = dispatch.result.get("mcp").cloned().unwrap_or(Value::Null);
                format!(
                    "### MCP status\n\n{}",
                    serde_json::to_string_pretty(&state)
                        .unwrap_or_else(|_| "unavailable".to_owned())
                )
            }
            Ok(_) => format!("MCP {} completed.", method.trim_start_matches("mcp/")),
            Err(error) => format!("MCP command failed: {error}"),
        }
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let harness = self.session_harness(session_id)?;
        let turn_id = harness
            .active_turn
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .clone();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        if matches!(turn_id.as_str(), "reserving" | "builtin-command") {
            harness.cancel_requested.store(true, Ordering::Release);
            harness.cancel_notify.notify_one();
            return Ok(());
        }
        harness.cancel_requested.store(true, Ordering::Release);
        harness.cancel_notify.notify_one();
        let mut service = harness.service.lock().await;
        service.interrupt(session_id, &turn_id)?;
        Ok(())
    }

    pub async fn close_session(&self, session_id: &str) -> Result<(), AcpError> {
        self.require_initialized()?;
        let harness = {
            let mut state = self.lock_state()?;
            if state.closing_sessions.contains(session_id)
                || state.closed_sessions.contains(session_id)
            {
                return Ok(());
            }
            if state.loading_sessions.contains(session_id) {
                return Err(AcpError::SessionConflict(session_id.to_owned()));
            }
            match state.sessions.get(session_id).cloned() {
                Some(harness) => {
                    state.closing_sessions.insert(session_id.to_owned());
                    harness
                }
                None => return Err(AcpError::SessionNotFound(session_id.to_owned())),
            }
        };
        harness.cancel_requested.store(true, Ordering::Release);
        harness.cancel_notify.notify_one();
        let mut service = harness.service.lock().await;
        {
            let mut state = self.lock_state()?;
            if state.closed_sessions.contains(session_id) {
                state.closing_sessions.remove(session_id);
                return Ok(());
            }
        }
        let result: Result<(), AcpError> = async {
            if let Some(turn_id) = harness
                .active_turn
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?
                .clone()
                && !matches!(turn_id.as_str(), "reserving" | "builtin-command")
            {
                service.interrupt(session_id, &turn_id)?;
            }
            service.close_session(session_id).await?;
            service.shutdown()?;
            Ok(())
        }
        .await;
        let mut state = self.lock_state()?;
        state.closing_sessions.remove(session_id);
        if result.is_ok() {
            state.sessions.remove(session_id);
            state.closed_sessions.insert(session_id.to_owned());
        }
        drop(state);
        drop(service);
        result
    }

    pub async fn request_permission(
        &self,
        session_id: &str,
        tool_call: Value,
        options: Vec<Value>,
    ) -> Result<Value, AcpError> {
        self.session_harness(session_id)?;
        self.call_client(
            "session/request_permission",
            json!({
                "sessionId": session_id,
                "toolCall": tool_call,
                "options": options,
            }),
        )
        .await
    }

    pub fn deny_unsupported_user_input(&self) -> Result<(), AcpError> {
        Err(AcpError::UnsupportedClientFlow(
            "interactive user-input callbacks are not part of ACP v1".to_owned(),
        ))
    }

    pub async fn client_tool(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        const CLIENT_TOOL_METHODS: &[&str] = &[
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
        ];
        if !CLIENT_TOOL_METHODS.contains(&method) {
            return Err(AcpError::UnsupportedClientFlow(method.to_owned()));
        }
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::InvalidParams("sessionId is required".to_owned()))?;
        self.session_harness(session_id)?;
        let capabilities = self.lock_state()?.client_capabilities.clone();
        let supported = match method {
            "fs/read_text_file" => capabilities.fs.read_text_file,
            "fs/write_text_file" => capabilities.fs.write_text_file,
            method if method.starts_with("terminal/") => capabilities.terminal,
            _ => false,
        };
        if !supported {
            return Err(AcpError::UnsupportedClientFlow(format!(
                "client did not advertise `{method}`"
            )));
        }
        self.call_client(method, params).await
    }

    pub async fn disconnect(&self) -> Result<(), AcpError> {
        let sessions = {
            let mut state = self.lock_state()?;
            state.initialized = false;
            state.authenticated = false;
            state.loading_sessions.clear();
            std::mem::take(&mut state.sessions)
        };
        let mut first_error = None;
        for (session_id, harness) in sessions {
            harness.cancel_requested.store(true, Ordering::Release);
            harness.cancel_notify.notify_one();
            let mut service = harness.service.lock().await;
            let already_closed = {
                let mut state = self.lock_state()?;
                let closed = state.closed_sessions.contains(&session_id);
                if closed {
                    state.closing_sessions.remove(&session_id);
                }
                closed
            };
            if already_closed {
                continue;
            }
            if let Some(turn_id) = harness
                .active_turn
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?
                .clone()
                && !matches!(turn_id.as_str(), "reserving" | "builtin-command")
                && let Err(error) = service.interrupt(&session_id, &turn_id)
                && first_error.is_none()
            {
                first_error = Some(AcpError::Client(error));
            }
            if let Err(error) = service.close_session(&session_id).await
                && first_error.is_none()
            {
                first_error = Some(AcpError::Client(error));
            }
            let shutdown_succeeded = match service.shutdown() {
                Ok(()) => true,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(AcpError::Client(error));
                    }
                    false
                }
            };
            let mut state = self.lock_state()?;
            state.closing_sessions.remove(&session_id);
            if shutdown_succeeded {
                state.closed_sessions.insert(session_id);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn call_client(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| AcpError::UnsupportedClientFlow(method.to_owned()))?;
        tokio::time::timeout(self.client_tool_timeout, client.request(method, params))
            .await
            .map_err(|_| AcpError::ClientToolTimeout(method.to_owned()))?
            .map_err(AcpError::ClientTool)
    }

    fn session_harness(&self, session_id: &str) -> Result<Arc<AcpHarness<D>>, AcpError> {
        self.require_initialized()?;
        let state = self.lock_state()?;
        if state.closing_sessions.contains(session_id) {
            return Err(AcpError::SessionNotFound(session_id.to_owned()));
        }
        state
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))
    }

    fn reserve_session_load(&self, session_id: &str) -> Result<(), AcpError> {
        let mut state = self.lock_state()?;
        if state.sessions.contains_key(session_id)
            || state.loading_sessions.contains(session_id)
            || state.closing_sessions.contains(session_id)
        {
            return Err(AcpError::SessionConflict(session_id.to_owned()));
        }
        state.loading_sessions.insert(session_id.to_owned());
        Ok(())
    }

    fn require_initialized(&self) -> Result<(), AcpError> {
        if self.lock_state()?.initialized {
            Ok(())
        } else {
            Err(AcpError::NotInitialized)
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, AgentState<D>>, AcpError> {
        self.state.lock().map_err(|_| AcpError::StatePoisoned)
    }

    fn live_session_settings(&self, session_id: &str) -> Result<Option<SessionSettings>, AcpError> {
        self.lock_state()?
            .sessions
            .get(session_id)
            .map(|harness| harness.settings())
            .transpose()
    }

    fn resolve_user_message_anchor(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<usize, AcpError> {
        if let Some(anchor) = parse_history_user_message_id(message_id) {
            return Ok(anchor);
        }
        let harness = self.lock_state()?.sessions.get(session_id).cloned();
        if let Some(harness) = harness
            && let Some(anchor) = harness.user_message_anchor(message_id)?
        {
            return Ok(anchor);
        }
        Err(AcpError::InvalidParams(format!(
            "cannot fork from unknown user messageId `{message_id}`"
        )))
    }
}

fn session_options(
    cwd: &str,
    session_id: Option<String>,
    additional_directories: Vec<String>,
    mcp_servers: Vec<Value>,
    resume: Option<String>,
    settings: &SessionSettings,
) -> SessionOptions {
    SessionOptions {
        working_directory: cwd.to_owned(),
        session_id,
        add_directories: additional_directories,
        trusted: !mcp_servers.is_empty(),
        agent: None,
        tool_filters: Vec::new(),
        enabled_tools: Vec::new(),
        disabled_tools: vec!["ask_user_question".to_owned(), "exit_plan_mode".to_owned()],
        mcp_servers,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_price_micros: None,
        mode: Some(settings.mode_id.clone()),
        thinking: settings.thinking != "off",
        reasoning_effort: (settings.thinking != "off").then(|| settings.thinking.clone()),
        auto_approve: false,
        resume,
        continue_session: false,
    }
}

fn all_history_entries<D>(
    service: &mut HeadlessService<D>,
    session_id: &str,
) -> Result<Vec<Value>, AcpError>
where
    D: TurnDriver,
{
    const PAGE_SIZE: usize = 500;
    let mut history = Vec::new();
    loop {
        let result = service.public_call(
            "history/list",
            json!({
                "sessionId": session_id,
                "offset": history.len(),
                "limit": PAGE_SIZE,
            }),
        )?;
        let page = result
            .get("history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if history.len().saturating_add(page.len()) > MAX_HISTORY_ENTRIES {
            return Err(AcpError::InvalidResponse(format!(
                "history exceeded the {MAX_HISTORY_ENTRIES}-entry limit"
            )));
        }
        let complete = page.len() < PAGE_SIZE;
        history.extend(page);
        if complete {
            return Ok(history);
        }
    }
}

fn parse_history_user_message_id(message_id: &str) -> Option<usize> {
    let mut parts = message_id.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("history"), Some(index), Some("user"), None) => index.parse().ok(),
        _ => None,
    }
}

fn fork_source_keep_messages(
    history: &[Value],
    anchor: usize,
    message_id: &str,
) -> Result<usize, AcpError> {
    let Some(message) = history.get(anchor) else {
        return Err(AcpError::InvalidParams(format!(
            "cannot fork from stale user messageId `{message_id}`"
        )));
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return Err(AcpError::InvalidParams(format!(
            "fork messageId `{message_id}` does not identify a user message"
        )));
    }
    Ok(history
        .iter()
        .enumerate()
        .skip(anchor.saturating_add(1))
        .find(|(_, message)| message.get("role").and_then(Value::as_str) == Some("user"))
        .map_or(history.len(), |(index, _)| index))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSettings {
    mode_id: String,
    thinking: String,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            mode_id: "code".to_owned(),
            thinking: "medium".to_owned(),
        }
    }
}

impl SessionSettings {
    fn from_release3_result(result: &BTreeMap<String, Value>) -> Self {
        let config = result
            .get("metadata")
            .and_then(|metadata| metadata.get("config"))
            .and_then(Value::as_object);
        let mode_id = config
            .and_then(|config| config.get("mode"))
            .and_then(Value::as_str)
            .filter(|mode| matches!(*mode, "code" | "plan"))
            .unwrap_or("code")
            .to_owned();
        let reasoning_effort = config
            .and_then(|config| {
                config
                    .get("reasoningEffort")
                    .or_else(|| config.get("reasoning_effort"))
            })
            .and_then(Value::as_str)
            .filter(|level| matches!(*level, "low" | "medium" | "high" | "max"));
        let thinking = match config.and_then(|config| config.get("thinking")) {
            Some(Value::Bool(false)) => "off",
            Some(Value::Bool(true)) => reasoning_effort.unwrap_or("medium"),
            Some(Value::String(level))
                if matches!(level.as_str(), "off" | "low" | "medium" | "high" | "max") =>
            {
                level
            }
            _ => reasoning_effort.unwrap_or("medium"),
        }
        .to_owned();
        Self { mode_id, thinking }
    }

    fn as_config(&self) -> Value {
        let mut config = json!({
            "mode": self.mode_id,
            "thinking": self.thinking != "off",
        });
        if self.thinking != "off" {
            config["reasoningEffort"] = json!(self.thinking);
        }
        config
    }
}

fn harness_from_service<D>(
    mut service: HeadlessService<D>,
    session_id: &str,
) -> Result<AcpHarness<D>, AcpError>
where
    D: TurnDriver,
{
    let view = service.session(session_id)?;
    let mode_id = view
        .intent
        .mode
        .filter(|mode| matches!(mode.as_str(), "code" | "plan"))
        .unwrap_or_else(|| "code".to_owned());
    let thinking = if view.intent.thinking {
        view.intent
            .reasoning_effort
            .filter(|level| matches!(level.as_str(), "low" | "medium" | "high" | "max"))
            .unwrap_or_else(|| "medium".to_owned())
    } else {
        "off".to_owned()
    };
    Ok(AcpHarness {
        service: tokio::sync::Mutex::new(service),
        active_turn: Mutex::new(None),
        cancel_requested: AtomicBool::new(false),
        cancel_notify: tokio::sync::Notify::new(),
        mode_id: Mutex::new(mode_id),
        config_options: Mutex::new(BTreeMap::from([("thinking".to_owned(), thinking)])),
        user_message_anchors: Mutex::new(BTreeMap::new()),
        next_user_message: AtomicU64::new(1),
        cwd: view.working_directory,
    })
}

fn validate_session_paths(
    cwd: &str,
    additional_directories: Option<&[String]>,
) -> Result<(), AcpError> {
    let additional_directories = additional_directories.unwrap_or_default();
    if additional_directories.len() > MAX_ADDITIONAL_DIRECTORIES {
        return Err(AcpError::InvalidParams(format!(
            "additionalDirectories cannot contain more than {MAX_ADDITIONAL_DIRECTORIES} roots"
        )));
    }
    if !Path::new(cwd).is_absolute() {
        return Err(AcpError::InvalidParams(
            "cwd must be an absolute path".to_owned(),
        ));
    }
    if let Some(path) = additional_directories
        .iter()
        .find(|path| !Path::new(path).is_absolute())
    {
        return Err(AcpError::InvalidParams(format!(
            "additional directory `{path}` must be an absolute path"
        )));
    }
    Ok(())
}

fn ensure_matching_cwd(requested: &str, persisted: &str, operation: &str) -> Result<(), AcpError> {
    if same_path(requested, persisted) {
        Ok(())
    } else {
        Err(AcpError::InvalidParams(format!(
            "cannot {operation} a session saved for `{persisted}` from cwd `{requested}`"
        )))
    }
}

fn same_path(left: &str, right: &str) -> bool {
    let canonical =
        |value: &str| std::fs::canonicalize(value).unwrap_or_else(|_| PathBuf::from(value));
    canonical(left) == canonical(right)
}

fn project_acp_mcp_servers(servers: &[Value]) -> Result<Vec<Value>, AcpError> {
    if servers.len() > MAX_MCP_SERVERS {
        return Err(AcpError::InvalidParams(format!(
            "mcpServers cannot contain more than {MAX_MCP_SERVERS} servers"
        )));
    }
    servers
        .iter()
        .map(|server| {
            let object = server.as_object().ok_or_else(|| {
                AcpError::InvalidParams("MCP server must be an object".to_owned())
            })?;
            let transport = object
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| AcpError::InvalidParams("MCP server requires a type".to_owned()))?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    AcpError::InvalidParams("stdio MCP server requires a name".to_owned())
                })?;
            match transport {
                "stdio" => {
                    let command = object
                        .get("command")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| {
                            AcpError::InvalidParams(
                                "stdio MCP server requires a command".to_owned(),
                            )
                        })?;
                    let args = object
                        .get("args")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .map(|argument| {
                            argument.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                                AcpError::InvalidParams(
                                    "stdio MCP server args must be strings".to_owned(),
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let environment =
                        object
                            .get("env")
                            .and_then(Value::as_array)
                            .map(Vec::as_slice)
                            .unwrap_or_default()
                            .iter()
                            .map(|variable| {
                                let name = variable
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        AcpError::InvalidParams(
                                            "stdio MCP environment entry requires name".to_owned(),
                                        )
                                    })?;
                                let value = variable
                                    .get("value")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        AcpError::InvalidParams(
                                            "stdio MCP environment entry requires value".to_owned(),
                                        )
                                    })?;
                                Ok((name.to_owned(), value.to_owned()))
                            })
                            .collect::<Result<BTreeMap<_, _>, AcpError>>()?;
                    Ok(json!({
                        "name": name,
                        "transport": "stdio",
                        "command": command,
                        "args": args,
                        "env": environment,
                    }))
                }
                "http" => {
                    let url = object
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| {
                            AcpError::InvalidParams("HTTP MCP server requires a URL".to_owned())
                        })?;
                    let headers = object
                        .get("headers")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .map(|header| {
                            let name =
                                header.get("name").and_then(Value::as_str).ok_or_else(|| {
                                    AcpError::InvalidParams(
                                        "HTTP MCP header requires a name".to_owned(),
                                    )
                                })?;
                            let value =
                                header.get("value").and_then(Value::as_str).ok_or_else(|| {
                                    AcpError::InvalidParams(
                                        "HTTP MCP header requires a value".to_owned(),
                                    )
                                })?;
                            Ok((name.to_owned(), value.to_owned()))
                        })
                        .collect::<Result<BTreeMap<_, _>, AcpError>>()?;
                    Ok(json!({
                        "name": name,
                        "transport": "streamable-http",
                        "url": url,
                        "headers": headers,
                    }))
                }
                unsupported => Err(AcpError::UnsupportedClientFlow(format!(
                    "MCP transport `{unsupported}` is not supported by the public app-server"
                ))),
            }
        })
        .collect()
}

fn acp_session_info(session: &Value) -> Result<AcpSessionInfo, AcpError> {
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidResponse("listed session omitted id".to_owned()))?
        .to_owned();
    let cwd = session
        .get("workingDirectory")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AcpError::InvalidResponse(format!("listed session `{session_id}` omitted cwd"))
        })?
        .to_owned();
    let title = session
        .get("title")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let updated_at = session
        .get("endTime")
        .and_then(Value::as_str)
        .or_else(|| session.get("startTime").and_then(Value::as_str))
        .map(ToOwned::to_owned);
    let mut meta = BTreeMap::new();
    for (source, target) in [
        ("parentSessionId", "parentSessionId"),
        ("messageCount", "messageCount"),
    ] {
        if let Some(value) = session.get(source)
            && !value.is_null()
        {
            meta.insert(target.to_owned(), value.clone());
        }
    }
    Ok(AcpSessionInfo {
        session_id,
        cwd,
        title,
        updated_at,
        additional_directories: None,
        meta,
    })
}

fn decode_session_cursor(cursor: Option<&str>) -> Result<usize, AcpError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    cursor
        .strip_prefix(SESSION_CURSOR_PREFIX)
        .and_then(|offset| offset.parse::<usize>().ok())
        .ok_or_else(|| AcpError::InvalidParams("invalid session/list cursor".to_owned()))
}

fn encode_session_cursor(offset: usize) -> String {
    format!("{SESSION_CURSOR_PREFIX}{offset}")
}

fn session_response(session_id: String, settings: &SessionSettings) -> AcpSession {
    AcpSession {
        session_id,
        modes: Some(json!({
            "currentModeId": settings.mode_id,
            "availableModes": [
                {"id": "code", "name": "Code", "description": "Execute changes"},
                {"id": "plan", "name": "Plan", "description": "Inspect and plan"},
            ],
        })),
        config_options: Some(thinking_config_options(&settings.thinking)),
    }
}

fn thinking_config_options(current_value: &str) -> Vec<Value> {
    vec![json!({
        "id": "thinking",
        "name": "Thinking",
        "type": "select",
        "currentValue": current_value,
        "options": [
            {"value": "off", "name": "Off"},
            {"value": "low", "name": "Low"},
            {"value": "medium", "name": "Medium"},
            {"value": "high", "name": "High"},
            {"value": "max", "name": "Max"},
        ],
    })]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuiltinCommand {
    name: String,
    arguments: String,
    raw: String,
}

fn builtin_command(content: &[Value], commands: &[Value]) -> Option<BuiltinCommand> {
    let [block] = content else {
        return None;
    };
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let raw = block.get("text").and_then(Value::as_str)?.trim();
    let command = raw.strip_prefix('/')?;
    let mut parts = command.splitn(2, char::is_whitespace);
    let name = parts.next()?.to_ascii_lowercase();
    if !commands
        .iter()
        .any(|command| command.get("name").and_then(Value::as_str) == Some(name.as_str()))
    {
        return None;
    }
    Some(BuiltinCommand {
        name,
        arguments: parts.next().map(str::trim).unwrap_or_default().to_owned(),
        raw: raw.to_owned(),
    })
}

fn command_response() -> AcpPromptResponse {
    AcpPromptResponse {
        stop_reason: "end_turn".to_owned(),
        usage: AcpUsage {
            total_tokens: 0,
            input_tokens: 0,
            output_tokens: 0,
        },
    }
}

fn acp_approval_options(detail: &Value) -> Vec<Value> {
    let choices = detail
        .get("choices")
        .and_then(Value::as_array)
        .map(|choices| {
            choices
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_else(|| {
            BTreeSet::from([
                "approve",
                "approve_for_session",
                "approve_permanently",
                "deny",
            ])
        });
    [
        (
            "approve",
            json!({
                "optionId": "allow_once",
                "name": "Allow once",
                "kind": "allow_once",
            }),
        ),
        (
            "approve_for_session",
            json!({
                "optionId": "allow_always",
                "name": "Allow for this session",
                "kind": "allow_always",
            }),
        ),
        (
            "approve_permanently",
            json!({
                "optionId": "allow_always_permanent",
                "name": "Always allow",
                "kind": "allow_always",
            }),
        ),
        (
            "deny",
            json!({
                "optionId": "reject_once",
                "name": "Reject",
                "kind": "reject_once",
            }),
        ),
    ]
    .into_iter()
    .filter_map(|(choice, option)| choices.contains(choice).then_some(option))
    .collect()
}

fn approval_output_from_acp(response: Option<&Value>, options: &[Value]) -> Value {
    let selected = response
        .filter(|response| {
            response.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected")
        })
        .and_then(|response| response.pointer("/outcome/optionId"))
        .and_then(Value::as_str);
    let offered = |option_id: &str| {
        options
            .iter()
            .any(|option| option.get("optionId").and_then(Value::as_str) == Some(option_id))
    };
    let decision = match selected.filter(|option_id| offered(option_id)) {
        Some("allow_once") => "approve",
        Some("allow_always") => "approve_for_session",
        Some("allow_always_permanent") => "approve_permanently",
        Some("reject_once" | "reject_always") => "deny",
        _ => "cancel_turn",
    };
    json!({
        "type": "approval",
        "decision": {"type": decision},
    })
}

fn cancelled_approval_output() -> Value {
    json!({
        "type": "approval",
        "decision": {"type": "cancel_turn"},
    })
}

async fn wait_for_cancel<D>(harness: &AcpHarness<D>)
where
    D: TurnDriver,
{
    loop {
        if harness.cancel_requested.load(Ordering::Acquire) {
            return;
        }
        harness.cancel_notify.notified().await;
    }
}

fn send_command_chunk(
    session_id: &str,
    kind: &str,
    text: &str,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    queue_update(
        sender,
        AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update: json!({
                "sessionUpdate": kind,
                "content": {"type": "text", "text": text},
                "messageId": format!("command-{}", now_millis()),
            }),
        },
    )
}

fn send_command_reply(
    session_id: &str,
    text: &str,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    send_command_chunk(session_id, "agent_message_chunk", text, sender)
}

fn send_command_tool_update(
    session_id: &str,
    update: Value,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    queue_update(
        sender,
        AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update,
        },
    )
}

fn teleport_summary(history: &[Value]) -> String {
    const MAX_SUMMARY_CHARS: usize = 32 * 1024;
    let lines = history
        .iter()
        .rev()
        .filter_map(|entry| {
            let role = entry.get("role").and_then(Value::as_str)?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let text = match entry.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            (!text.trim().is_empty()).then(|| format!("{role}: {text}"))
        })
        .take(32)
        .collect::<Vec<_>>();
    let summary = lines.into_iter().rev().collect::<Vec<_>>().join("\n\n");
    let bounded = summary.chars().take(MAX_SUMMARY_CHARS).collect::<String>();
    if bounded.is_empty() {
        "Continue this session in Vibe Code".to_owned()
    } else {
        bounded
    }
}

fn teleport_events_from_notifications(
    notifications: &[vibe_app_server::client::PublicNotification],
) -> Result<Vec<ProgrammaticTeleportEvent>, AcpError> {
    notifications
        .iter()
        .filter(|notification| notification.method == "vibeCode/teleport/event")
        .map(|notification| {
            notification
                .params
                .get("event")
                .cloned()
                .ok_or_else(|| {
                    AcpError::InvalidResponse("Teleport notification omitted its event".to_owned())
                })
                .and_then(|event| serde_json::from_value(event).map_err(AcpError::Json))
        })
        .collect()
}

fn emit_teleport_events(
    session_id: &str,
    call_id: &str,
    events: &[ProgrammaticTeleportEvent],
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    for event in events {
        let (title, status, teleport_status, raw_output) = match event {
            ProgrammaticTeleportEvent::SummarizingContext { .. } => (
                "Summarizing session context",
                "in_progress",
                "summarizing_context",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::CheckingGit { .. } => (
                "Checking Git repository",
                "in_progress",
                "preparing_workspace",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::PushRequired {
                unpushed_count,
                branch_not_pushed,
                ..
            } => (
                "Git push approval required",
                "in_progress",
                "push_required",
                json!({
                    "unpushedCount": unpushed_count,
                    "branchNotPushed": branch_not_pushed,
                }),
            ),
            ProgrammaticTeleportEvent::Pushing { .. } => (
                "Pushing Git changes",
                "in_progress",
                "syncing_remote",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::StartingWorkflow { .. } => (
                "Starting Vibe Code workflow",
                "in_progress",
                "starting_workflow",
                Value::Null,
            ),
            ProgrammaticTeleportEvent::Complete { url, .. } => (
                "Session teleported to Vibe Code",
                "completed",
                "completed",
                json!({"url": url}),
            ),
            ProgrammaticTeleportEvent::Failed { error, .. } => (
                "Teleport failed",
                "failed",
                "failed",
                json!({"message": error.message, "code": error.code}),
            ),
        };
        send_command_tool_update(
            session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "title": title,
                "kind": "other",
                "status": status,
                "rawOutput": raw_output,
                "_meta": {
                    "tool_name": "teleport",
                    "teleport": {"status": teleport_status},
                },
            }),
            sender,
        )?;
    }
    Ok(())
}

fn send_teleport_failed(
    session_id: &str,
    call_id: &str,
    message: &str,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    let message = message.chars().take(4_096).collect::<String>();
    send_command_tool_update(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "title": "Teleport failed",
            "kind": "other",
            "status": "failed",
            "rawOutput": {"message": message},
            "_meta": {"tool_name": "teleport", "teleport": {"status": "failed"}},
        }),
        sender,
    )
}

fn send_teleport_cancelled(
    session_id: &str,
    call_id: &str,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    send_command_tool_update(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "title": "Teleport cancelled",
            "kind": "other",
            "status": "failed",
            "rawOutput": {"message": "Teleport was cancelled; the local session remains available."},
            "_meta": {"tool_name": "teleport", "teleport": {"status": "failed"}},
        }),
        sender,
    )
}

async fn execute_proxy_command<D>(arguments: &str, harness: &Arc<AcpHarness<D>>) -> String
where
    D: TurnDriver,
{
    const KEYS: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "SSL_CERT_FILE"];
    let parts = arguments.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() {
        return "Usage: `/proxy-setup <HTTP_PROXY|HTTPS_PROXY|NO_PROXY|SSL_CERT_FILE> [value]`"
            .to_owned();
    }
    if parts.len() > 2 {
        return "Proxy values containing whitespace are not supported.".to_owned();
    }
    let key = parts[0].to_ascii_uppercase();
    if !KEYS.contains(&key.as_str()) {
        return "Proxy key must be HTTP_PROXY, HTTPS_PROXY, NO_PROXY, or SSL_CERT_FILE.".to_owned();
    }
    let path = key.to_ascii_lowercase();
    let mutation = match parts.get(1) {
        Some(value) => json!({"path": [path], "value": value}),
        None => json!({"path": [path], "remove": true}),
    };
    let mut service = harness.service.lock().await;
    match service.public_call(
        "config/batchWrite",
        json!({
            "writes": [{
                "target": "user",
                "expectedFingerprint": null,
                "mutations": [mutation],
            }],
        }),
    ) {
        Ok(_) if parts.len() == 2 => {
            format!("Set `{key}` in user configuration. Start a new session for it to take effect.")
        }
        Ok(_) => {
            format!(
                "Removed `{key}` from user configuration. Start a new session for it to take effect."
            )
        }
        Err(error) => format!("Proxy configuration failed: {error}"),
    }
}

fn metadata_session_id(result: &BTreeMap<String, Value>) -> Result<String, AcpError> {
    result
        .get("metadata")
        .and_then(|metadata| {
            metadata
                .get("session_id")
                .or_else(|| metadata.get("sessionId"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| AcpError::InvalidResponse("missing resumed session ID".to_owned()))
}

fn turn_request(content: Vec<Value>) -> Result<TurnRequest, AcpError> {
    if content.is_empty() {
        return Err(AcpError::InvalidParams(
            "prompt must contain at least one content block".to_owned(),
        ));
    }
    if content.len() > MAX_PROMPT_BLOCKS {
        return Err(AcpError::InvalidParams(format!(
            "prompt cannot contain more than {MAX_PROMPT_BLOCKS} content blocks"
        )));
    }
    let mut public = Vec::with_capacity(content.len());
    let mut text = Vec::new();
    let mut content_bytes = 0_usize;
    for block in &content {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::InvalidParams("content block type is required".to_owned()))?;
        content_bytes = content_bytes.saturating_add(serde_json::to_vec(block)?.len());
        if content_bytes > MAX_EMBEDDED_CONTENT_BYTES {
            return Err(AcpError::InvalidParams(format!(
                "prompt content exceeds {MAX_EMBEDDED_CONTENT_BYTES} bytes"
            )));
        }
        match kind {
            "text" => {
                let value = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AcpError::InvalidParams("text block requires text".to_owned()))?
                    .to_owned();
                text.push(value.clone());
                public.push(PublicContentBlock::Text { text: value });
            }
            "image" => {
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AcpError::InvalidParams("image block requires mimeType".to_owned())
                    })?;
                if !matches!(
                    mime,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    return Err(AcpError::InvalidParams(format!(
                        "unsupported image MIME type `{mime}`"
                    )));
                }
                let data = block.get("data").and_then(Value::as_str).ok_or_else(|| {
                    AcpError::InvalidParams("image block requires base64 data".to_owned())
                })?;
                if data.is_empty()
                    || base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .is_err()
                {
                    return Err(AcpError::InvalidParams(
                        "image data is not canonical base64".to_owned(),
                    ));
                }
                public.push(PublicContentBlock::Image {
                    attachment: block.clone(),
                });
            }
            "resource" | "resource_link" => {
                let has_uri = block
                    .get("uri")
                    .or_else(|| block.pointer("/resource/uri"))
                    .and_then(Value::as_str)
                    .is_some_and(|uri| !uri.trim().is_empty());
                if !has_uri {
                    return Err(AcpError::InvalidParams(
                        "resource block requires a URI".to_owned(),
                    ));
                }
                public.push(PublicContentBlock::Resource {
                    resource: block.clone(),
                });
            }
            unsupported => {
                return Err(AcpError::InvalidParams(format!(
                    "unsupported content block `{unsupported}`"
                )));
            }
        }
    }
    if text.is_empty() {
        let fallback = "[Attached content]".to_owned();
        public.insert(
            0,
            PublicContentBlock::Text {
                text: fallback.clone(),
            },
        );
        text.push(fallback);
    }
    Ok(TurnRequest {
        prompt: text.join("\n\n"),
        input: public,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: Some(Value::Array(content)),
        mention_stats: None,
    })
}

#[derive(Default)]
struct AcpUpdateProjection {
    entries: BTreeMap<String, PublicHistoryEntry>,
    message_ids: BTreeMap<String, String>,
    next_user_message_id: Option<String>,
}

impl AcpUpdateProjection {
    fn with_user_message_id(message_id: String) -> Self {
        Self {
            next_user_message_id: Some(message_id),
            ..Self::default()
        }
    }

    fn message_id(&mut self, entry: &PublicHistoryEntry) -> String {
        let entry_id = &entry.metadata().id;
        if let Some(message_id) = self.message_ids.get(entry_id) {
            return message_id.clone();
        }
        let message_id = if matches!(
            entry,
            PublicHistoryEntry::Message {
                role: PublicMessageRole::User,
                ..
            }
        ) {
            self.next_user_message_id
                .take()
                .unwrap_or_else(|| entry_id.clone())
        } else {
            entry_id.clone()
        };
        self.message_ids
            .insert(entry_id.clone(), message_id.clone());
        message_id
    }
}

fn send_acp_updates(
    session_id: &str,
    update: ProgrammaticUpdate,
    projection: &mut AcpUpdateProjection,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
        return Ok(());
    };
    let entry_id = entry.metadata().id.clone();
    let message_id = projection.message_id(entry.as_ref());
    let previous = projection.entries.insert(entry_id, (*entry).clone());
    let updates = public_entry_updates(previous.as_ref(), entry.as_ref())?;
    for mut update in updates {
        if message_id != entry.metadata().id
            && update.get("messageId").and_then(Value::as_str) == Some(entry.metadata().id.as_str())
        {
            update["messageId"] = json!(message_id);
        }
        queue_update(
            sender,
            AcpSessionUpdate {
                session_id: session_id.to_owned(),
                update,
            },
        )?;
    }
    Ok(())
}

fn public_entry_updates(
    previous: Option<&PublicHistoryEntry>,
    entry: &PublicHistoryEntry,
) -> Result<Vec<Value>, AcpError> {
    let entry_id = &entry.metadata().id;
    match entry {
        PublicHistoryEntry::Message { role, content, .. } => {
            let kind = match role {
                PublicMessageRole::User => "user_message_chunk",
                PublicMessageRole::Assistant => "agent_message_chunk",
                PublicMessageRole::System => return Ok(Vec::new()),
            };
            if let Some(previous) = previous {
                let PublicHistoryEntry::Message {
                    role: previous_role,
                    content: previous_content,
                    ..
                } = previous
                else {
                    return Err(AcpError::InvalidResponse(format!(
                        "public entry `{entry_id}` changed type"
                    )));
                };
                if previous_role != role {
                    return Err(AcpError::InvalidResponse(format!(
                        "public message `{entry_id}` changed role"
                    )));
                }
                let delta = cumulative_text_delta(
                    &public_text(previous_content),
                    &public_text(content),
                    entry_id,
                )?;
                return Ok(delta
                    .filter(|text| !text.is_empty())
                    .map(|text| {
                        vec![json!({
                            "sessionUpdate": kind,
                            "content": {"type": "text", "text": text},
                            "messageId": entry_id,
                        })]
                    })
                    .unwrap_or_default());
            }
            Ok(content
                .iter()
                .filter_map(acp_public_content)
                .map(|content| {
                    json!({
                        "sessionUpdate": kind,
                        "content": content,
                        "messageId": entry_id,
                    })
                })
                .collect())
        }
        PublicHistoryEntry::Reasoning { text, .. } => {
            let value = if let Some(previous) = previous {
                let PublicHistoryEntry::Reasoning {
                    text: previous_text,
                    ..
                } = previous
                else {
                    return Err(AcpError::InvalidResponse(format!(
                        "public entry `{entry_id}` changed type"
                    )));
                };
                cumulative_text_delta(previous_text, text, entry_id)?
            } else {
                Some(text.clone())
            };
            Ok(value
                .filter(|value| !value.is_empty())
                .map(|value| {
                    vec![json!({
                        "sessionUpdate": "agent_thought_chunk",
                        "content": {"type": "text", "text": value},
                        "messageId": entry_id,
                    })]
                })
                .unwrap_or_default())
        }
        PublicHistoryEntry::Effect {
            title,
            state,
            detail,
            ..
        } => {
            let previous_output = match previous {
                Some(PublicHistoryEntry::Effect { state, .. }) => effect_output_text(state),
                Some(_) => {
                    return Err(AcpError::InvalidResponse(format!(
                        "public entry `{entry_id}` changed type"
                    )));
                }
                None => "",
            };
            let output = effect_output_text(state);
            let output_delta = cumulative_text_delta(previous_output, output, entry_id)?;
            let mut update = json!({
                "sessionUpdate": if previous.is_some() {"tool_call_update"} else {"tool_call"},
                "toolCallId": entry_id,
                "title": title,
                "kind": "other",
                "status": acp_effect_status(state),
                "rawInput": detail,
                "rawOutput": effect_raw_output(state),
            });
            if let Some(delta) = output_delta.filter(|delta| !delta.is_empty()) {
                update["content"] = json!([{
                    "type": "content",
                    "content": {"type": "text", "text": delta},
                }]);
            }
            Ok(vec![update])
        }
        PublicHistoryEntry::Callback {
            callback_id,
            title,
            state,
            detail,
            ..
        } => Ok(vec![json!({
            "sessionUpdate": if previous.is_some() {"tool_call_update"} else {"tool_call"},
            "toolCallId": callback_id,
            "title": title,
            "kind": "other",
            "status": acp_callback_status(state),
            "rawInput": detail,
        })]),
        PublicHistoryEntry::Notice {
            message, detail, ..
        } => Ok(previous
            .is_none_or(|previous| previous != entry)
            .then(|| {
                json!({
                    "sessionUpdate": "session_info_update",
                    "title": message,
                    "_meta": {"detail": detail, "entryId": entry_id},
                })
            })
            .into_iter()
            .collect()),
        PublicHistoryEntry::Checkpoint {
            kind,
            message,
            details,
            ..
        } => {
            let encoded = serde_json::to_value(entry)?;
            Ok(vec![json!({
                "sessionUpdate": if previous.is_some() {"tool_call_update"} else {"tool_call"},
                "toolCallId": entry_id,
                "title": message.as_deref().unwrap_or("Checkpoint"),
                "kind": "think",
                "status": if encoded.get("generationStatus").and_then(Value::as_str)
                    == Some("completed") {"completed"} else {"in_progress"},
                "rawInput": details,
                "_meta": {"checkpointKind": kind},
            })])
        }
    }
}

fn public_text(content: &[PublicContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            PublicContentBlock::Text { text } => Some(text.as_str()),
            PublicContentBlock::Image { .. } | PublicContentBlock::Resource { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn cumulative_text_delta(
    previous: &str,
    current: &str,
    entry_id: &str,
) -> Result<Option<String>, AcpError> {
    if previous == current {
        return Ok(None);
    }
    current
        .strip_prefix(previous)
        .map(|delta| Some(delta.to_owned()))
        .ok_or_else(|| {
            AcpError::InvalidResponse(format!(
                "public entry `{entry_id}` replaced previously streamed text"
            ))
        })
}

fn acp_public_content(block: &PublicContentBlock) -> Option<Value> {
    match block {
        PublicContentBlock::Text { text } => {
            (!text.is_empty()).then(|| json!({"type": "text", "text": text}))
        }
        PublicContentBlock::Image { attachment } => normalize_image_content(attachment),
        PublicContentBlock::Resource { resource } => normalize_resource_content(resource),
    }
}

fn normalize_image_content(attachment: &Value) -> Option<Value> {
    if attachment.get("type").and_then(Value::as_str) == Some("image")
        && attachment.get("data").is_some_and(Value::is_string)
    {
        return Some(attachment.clone());
    }
    let data = attachment
        .get("data")
        .or_else(|| attachment.pointer("/source/data"))
        .and_then(Value::as_str)?;
    let mime_type = attachment
        .get("mimeType")
        .or_else(|| attachment.get("mediaType"))
        .and_then(Value::as_str)?;
    Some(json!({"type": "image", "data": data, "mimeType": mime_type}))
}

fn normalize_resource_content(resource: &Value) -> Option<Value> {
    match resource.get("type").and_then(Value::as_str) {
        Some("resource" | "resource_link") => Some(resource.clone()),
        _ if resource.get("uri").is_some_and(Value::is_string) => {
            Some(json!({"type": "resource", "resource": resource}))
        }
        _ => None,
    }
}

fn effect_output_text(state: &PublicEffectState) -> &str {
    match state {
        PublicEffectState::Pending | PublicEffectState::Skipped { .. } => "",
        PublicEffectState::Running { output_text }
        | PublicEffectState::Blocked { output_text, .. }
        | PublicEffectState::Completed { output_text, .. }
        | PublicEffectState::Failed { output_text, .. }
        | PublicEffectState::Cancelled { output_text, .. } => output_text,
    }
}

fn effect_raw_output(state: &PublicEffectState) -> Value {
    match state {
        PublicEffectState::Completed { output, .. } => output.clone(),
        PublicEffectState::Failed { error, .. } => {
            serde_json::to_value(error).unwrap_or(Value::Null)
        }
        PublicEffectState::Cancelled { reason, .. } | PublicEffectState::Skipped { reason, .. } => {
            json!(reason)
        }
        PublicEffectState::Pending
        | PublicEffectState::Running { .. }
        | PublicEffectState::Blocked { .. } => Value::Null,
    }
}

pub fn history_entry_updates(entry: &Value, index: usize) -> Result<Vec<Value>, AcpError> {
    if entry.get("type").and_then(Value::as_str).is_some() {
        let public = serde_json::from_value::<PublicHistoryEntry>(entry.clone())?;
        return public_entry_updates(None, &public);
    }
    let role = entry
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidResponse("history entry omitted role".to_owned()))?;
    let base_id = format!("history:{index}:{role}");
    match role {
        "system" | "private" => Ok(Vec::new()),
        "user" => Ok(history_message_chunks(
            "user_message_chunk",
            &base_id,
            entry.get("content"),
        )),
        "assistant" => {
            let mut updates = Vec::new();
            if let Some(reasoning) = entry
                .get("reasoning")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                updates.push(json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": reasoning},
                    "messageId": format!("history:{index}:reasoning"),
                }));
            }
            updates.extend(history_message_chunks(
                "agent_message_chunk",
                &base_id,
                entry.get("content"),
            ));
            for tool_call in entry
                .get("tool_calls")
                .or_else(|| entry.get("toolCalls"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let tool_call_id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(&base_id);
                let title = tool_call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Tool");
                let arguments = tool_call.get("arguments").cloned().unwrap_or(Value::Null);
                let raw_input = arguments.as_str().map_or(arguments.clone(), |arguments| {
                    serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments))
                });
                updates.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": tool_call_id,
                    "title": title,
                    "kind": "other",
                    "status": "pending",
                    "rawInput": raw_input,
                }));
            }
            Ok(updates)
        }
        "tool" => {
            let call_id = entry
                .get("call_id")
                .or_else(|| entry.get("callId"))
                .and_then(Value::as_str)
                .unwrap_or(&base_id);
            let content = entry
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let is_error = entry
                .get("is_error")
                .or_else(|| entry.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "status": if is_error {"failed"} else {"completed"},
                "content": (!content.is_empty()).then(|| vec![json!({
                    "type": "content",
                    "content": {"type": "text", "text": content},
                })]),
                "rawOutput": content,
            })])
        }
        unsupported => Err(AcpError::InvalidResponse(format!(
            "unsupported history role `{unsupported}`"
        ))),
    }
}

fn history_message_chunks(kind: &str, message_id: &str, content: Option<&Value>) -> Vec<Value> {
    normalized_history_content(content)
        .into_iter()
        .map(|content| {
            json!({
                "sessionUpdate": kind,
                "content": content,
                "messageId": message_id,
            })
        })
        .collect()
}

fn normalized_history_content(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type": "text", "text": text})]
        }
        Some(Value::Array(blocks)) => blocks.iter().filter_map(normalize_history_block).collect(),
        Some(Value::Object(_)) => content
            .and_then(normalize_history_block)
            .into_iter()
            .collect(),
        Some(Value::String(_)) | Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![json!({"type": "text", "text": other.to_string()})],
    }
}

fn normalize_history_block(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| json!({"type": "text", "text": text})),
        Some("image") => block
            .get("attachment")
            .map_or_else(|| normalize_image_content(block), normalize_image_content),
        Some("resource") => block.get("resource").map_or_else(
            || normalize_resource_content(block),
            |resource| {
                if resource.get("uri").is_some() {
                    Some(json!({"type": "resource", "resource": resource}))
                } else {
                    normalize_resource_content(resource)
                }
            },
        ),
        Some("resource_link") => Some(block.clone()),
        _ => None,
    }
}

fn acp_effect_status(state: &PublicEffectState) -> &'static str {
    match state {
        PublicEffectState::Pending => "pending",
        PublicEffectState::Running { .. } | PublicEffectState::Blocked { .. } => "in_progress",
        PublicEffectState::Completed { .. } | PublicEffectState::Skipped { .. } => "completed",
        PublicEffectState::Failed { .. } | PublicEffectState::Cancelled { .. } => "failed",
    }
}

fn acp_callback_status(state: &PublicCallbackState) -> &'static str {
    match state {
        PublicCallbackState::Open => "pending",
        PublicCallbackState::Answered { .. } => "completed",
        PublicCallbackState::Cancelled { .. } | PublicCallbackState::Expired { .. } => "failed",
    }
}

fn send_usage(
    session_id: &str,
    turn: &ProgrammaticTurn,
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    let mut update = json!({
        "sessionUpdate": "usage_update",
        "used": turn.context_tokens,
        "size": 200_000,
    });
    if turn.usage.price_micros > 0 {
        update["cost"] = json!({
            "amount": turn.usage.price_micros as f64 / 1_000_000.0,
            "currency": "USD",
        });
    }
    queue_update(
        sender,
        AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update,
        },
    )
}

fn queue_update(
    sender: &tokio::sync::mpsc::Sender<AcpSessionUpdate>,
    update: AcpSessionUpdate,
) -> Result<(), AcpError> {
    sender.try_send(update).map_err(|error| match error {
        tokio::sync::mpsc::error::TrySendError::Full(_) => AcpError::Backpressure,
        tokio::sync::mpsc::error::TrySendError::Closed(_) => AcpError::Disconnected,
    })
}

fn clear_active<D>(harness: &AcpHarness<D>) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    harness.cancel_requested.store(false, Ordering::Release);
    *harness
        .active_turn
        .lock()
        .map_err(|_| AcpError::StatePoisoned)? = None;
    Ok(())
}

fn acp_stop_reason(turn: &ProgrammaticTurn) -> &'static str {
    match turn.stop_reason {
        vibe_app_server::client::PublicTurnStopReason::Complete => "end_turn",
        vibe_app_server::client::PublicTurnStopReason::MaxSteps => "max_turn_requests",
        vibe_app_server::client::PublicTurnStopReason::TokenLimit
        | vibe_app_server::client::PublicTurnStopReason::PriceLimit
        | vibe_app_server::client::PublicTurnStopReason::ResponseLength => "max_tokens",
        vibe_app_server::client::PublicTurnStopReason::Refusal => "refusal",
        vibe_app_server::client::PublicTurnStopReason::Cancelled
        | vibe_app_server::client::PublicTurnStopReason::Failed => "cancelled",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpInitializeRequest {
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u16,
    #[serde(default)]
    pub client_capabilities: AcpClientCapabilities,
    #[serde(default)]
    pub client_info: Option<AcpClientInfo>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

impl Default for AcpInitializeRequest {
    fn default() -> Self {
        Self {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: AcpClientCapabilities::default(),
            client_info: None,
            meta: None,
        }
    }
}

fn default_protocol_version() -> u16 {
    ACP_PROTOCOL_VERSION
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpClientCapabilities {
    pub fs: AcpFilesystemCapabilities,
    pub terminal: bool,
    pub session: Value,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpFilesystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpClientInfo {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpInitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AcpAgentCapabilities,
    pub auth_methods: Vec<Value>,
    pub agent_info: AcpImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: AcpPromptCapabilities,
    pub session_capabilities: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptCapabilities {
    pub audio: bool,
    pub embedded_context: bool,
    pub image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpImplementation {
    pub name: String,
    pub title: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpNewSession {
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpLoadSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpListSessions {
    pub cwd: Option<String>,
    pub cursor: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpForkSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub new_session_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub additional_directories: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSession {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionInfo {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionList {
    pub sessions: Vec<AcpSessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpHistoryPage {
    pub entries: Vec<Value>,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionUpdate {
    pub session_id: String,
    pub update: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptResponse {
    pub stop_reason: String,
    pub usage: AcpUsage,
}

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("ACP agent is not initialized")]
    NotInitialized,
    #[error("ACP agent is already initialized")]
    AlreadyInitialized,
    #[error("ACP protocol version {0} is not supported")]
    UnsupportedProtocol(u16),
    #[error("ACP authentication method `{0}` is not supported")]
    UnsupportedAuthentication(String),
    #[error("ACP session `{0}` was not found")]
    SessionNotFound(String),
    #[error("ACP session `{0}` already exists")]
    SessionConflict(String),
    #[error("invalid ACP parameters: {0}")]
    InvalidParams(String),
    #[error("invalid ACP response: {0}")]
    InvalidResponse(String),
    #[error("ACP client flow is unsupported: {0}")]
    UnsupportedClientFlow(String),
    #[error("ACP client tool `{0}` timed out")]
    ClientToolTimeout(String),
    #[error("ACP client tool failed: {0}")]
    ClientTool(String),
    #[error("ACP driver failed: {0}")]
    Driver(String),
    #[error("ACP configuration failed: {0}")]
    Configuration(String),
    #[error("ACP state lock is poisoned")]
    StatePoisoned,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("ACP client disconnected")]
    Disconnected,
    #[error("ACP update queue is saturated")]
    Backpressure,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use vibe_app_server::client::{DriverError, DriverFuture, EchoTurnDriver, TurnReservation};
    use vibe_app_server::release4::{
        CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectPage, ProjectRepository,
        TeleportCloud, TeleportStartRequest,
    };

    struct FixtureProjectCloud;

    impl ProjectCloud for FixtureProjectCloud {
        fn create(
            &self,
            name: &str,
            repo_url: &str,
            default_branch: &str,
        ) -> Result<Project, CloudError> {
            Ok(Project {
                project_id: "project-1".to_owned(),
                name: name.to_owned(),
                repositories: vec![ProjectRepository {
                    repo_url: repo_url.to_owned(),
                    default_branch: Some(default_branch.to_owned()),
                }],
                is_read_only: false,
            })
        }

        fn list(&self, _cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
            Ok(ProjectPage {
                projects: vec![Project {
                    project_id: "project-1".to_owned(),
                    name: "Fixture".to_owned(),
                    repositories: vec![ProjectRepository {
                        repo_url: "https://github.com/example/repository.git".to_owned(),
                        default_branch: Some("main".to_owned()),
                    }],
                    is_read_only: false,
                }],
                next_cursor: None,
            })
        }
    }

    struct FixtureTeleportCloud;

    impl TeleportCloud for FixtureTeleportCloud {
        fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
            Ok(format!(
                "https://cloud.example.test/teleport/{}",
                request.idempotency_key
            ))
        }
    }

    struct CleanFixtureGit;

    impl GitProbe for CleanFixtureGit {
        fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
            Ok(GitSnapshot {
                repository: "https://github.com/example/repository.git".to_owned(),
                dirty: false,
                unpushed: false,
            })
        }

        fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
            Ok(())
        }
    }

    struct RecordingTurnDriver {
        inner: EchoTurnDriver,
        reservations: Arc<Mutex<Vec<TurnReservation>>>,
    }

    impl RecordingTurnDriver {
        fn new(reservations: Arc<Mutex<Vec<TurnReservation>>>) -> Self {
            Self {
                inner: EchoTurnDriver::new("answer"),
                reservations,
            }
        }
    }

    impl TurnDriver for RecordingTurnDriver {
        fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
            self.reservations
                .lock()
                .expect("reservations")
                .push(reservation.clone());
            self.inner.run(reservation)
        }
    }

    #[test]
    fn bounded_update_queue_fails_closed_on_saturation_and_disconnect() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        queue_update(
            &sender,
            AcpSessionUpdate {
                session_id: "session".to_owned(),
                update: json!({"sessionUpdate": "agent_message_chunk"}),
            },
        )
        .expect("first update fits");
        assert!(matches!(
            queue_update(
                &sender,
                AcpSessionUpdate {
                    session_id: "session".to_owned(),
                    update: json!({"sessionUpdate": "agent_message_chunk"}),
                },
            ),
            Err(AcpError::Backpressure)
        ));
        drop(receiver);
        assert!(matches!(
            queue_update(
                &sender,
                AcpSessionUpdate {
                    session_id: "session".to_owned(),
                    update: json!({"sessionUpdate": "agent_message_chunk"}),
                },
            ),
            Err(AcpError::Disconnected)
        ));
    }

    #[test]
    fn persisted_thinking_boolean_and_effort_round_trip_without_losing_off() {
        let disabled = SessionSettings::from_release3_result(&BTreeMap::from([(
            "metadata".to_owned(),
            json!({"config": {"mode": "plan", "thinking": false, "reasoningEffort": "high"}}),
        )]));
        assert_eq!(disabled.mode_id, "plan");
        assert_eq!(disabled.thinking, "off");
        assert_eq!(
            disabled.as_config(),
            json!({"mode": "plan", "thinking": false})
        );

        let enabled = SessionSettings::from_release3_result(&BTreeMap::from([(
            "metadata".to_owned(),
            json!({"config": {"thinking": true, "reasoningEffort": "high"}}),
        )]));
        assert_eq!(enabled.thinking, "high");
        assert_eq!(
            enabled.as_config(),
            json!({"mode": "code", "thinking": true, "reasoningEffort": "high"})
        );
    }

    #[tokio::test]
    async fn lifecycle_rich_content_and_updates_stay_on_public_app_server_contracts() {
        let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        let initialized = agent.initialize().expect("ACP initializes");
        assert_eq!(initialized.protocol_version, 1);
        assert!(initialized.agent_capabilities.load_session);
        assert_eq!(initialized.auth_methods[0]["id"], "environment");
        assert_eq!(
            initialized.auth_methods[0]["vars"][0]["name"],
            "MISTRAL_API_KEY"
        );
        agent.authenticate("environment").expect("auth");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: Some(Vec::new()),
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("ACP session starts");
        let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);
        let response = agent
            .prompt_content_streaming(
                &session.session_id,
                vec![
                    json!({"type": "text", "text": "question"}),
                    json!({"type": "image", "mimeType": "image/png", "data": "AA=="}),
                    json!({"type": "resource", "uri": "file:///workspace/context.txt", "text": "ctx"}),
                ],
                sender,
            )
            .await
            .expect("ACP prompt completes");
        assert_eq!(response.stop_reason, "end_turn");
        let mut updates = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        assert!(
            updates
                .iter()
                .any(|update| { update.update["sessionUpdate"] == json!("agent_message_chunk") })
        );
        assert_eq!(
            updates.last().map(|update| &update.update["sessionUpdate"]),
            Some(&json!("usage_update"))
        );
        agent
            .close_session(&session.session_id)
            .await
            .expect("ACP session closes");
        agent.disconnect().await.expect("ACP disconnects");
    }

    #[tokio::test]
    async fn malformed_input_and_update_backpressure_release_the_session_for_later_prompts() {
        let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");

        let (sender, _updates) = tokio::sync::mpsc::channel(1);
        assert!(matches!(
            agent
                .prompt_content_streaming(
                    &session.session_id,
                    vec![json!({"type": "image", "mimeType": "image/png", "data": "===="})],
                    sender,
                )
                .await,
            Err(AcpError::InvalidParams(_))
        ));
        assert!(
            agent
                .prompt(&session.session_id, "after invalid")
                .await
                .is_ok()
        );

        let (sender, _updates) = tokio::sync::mpsc::channel(1);
        assert!(matches!(
            agent
                .prompt_streaming(&session.session_id, "overflow", sender)
                .await,
            Err(AcpError::Backpressure)
        ));
        assert!(
            agent
                .prompt(&session.session_id, "after overflow")
                .await
                .is_ok()
        );
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn built_in_help_is_advertised_and_does_not_start_a_model_turn() {
        let reservations = Arc::new(Mutex::new(Vec::new()));
        let agent =
            AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        let (sender, mut updates) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);

        let response = agent
            .prompt_content_streaming(
                &session.session_id,
                vec![json!({"type": "text", "text": "/help"})],
                sender,
            )
            .await
            .expect("help command");

        assert_eq!(response, command_response());
        assert!(reservations.lock().expect("reservations").is_empty());
        let updates = std::iter::from_fn(|| updates.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].update["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1].update["sessionUpdate"], "agent_message_chunk");
        let help = updates[1].update["content"]["text"]
            .as_str()
            .expect("help text");
        assert!(help.contains("/compact"));
        assert!(help.contains("/reload"));
        assert!(available_commands().iter().all(|command| {
            let name = command["name"].as_str().unwrap_or_default();
            help.contains(&format!("/{name}"))
        }));
    }

    #[tokio::test]
    async fn shared_session_root_supports_history_fork_and_reload() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let session_root = temporary.path().join("sessions");
        let agent =
            AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
                .expect("agent starts")
                .with_session_root(session_root);
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: temporary.path().display().to_string(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");

        agent
            .prompt(&session.session_id, "question")
            .await
            .expect("prompt");
        let history = agent
            .history(&session.session_id, 0, 50)
            .await
            .expect("history");
        assert_eq!(history.entries.len(), 2);
        let forked = agent
            .fork_session(AcpForkSession {
                session_id: session.session_id.clone(),
                cwd: temporary.path().display().to_string(),
                new_session_id: Some("acp-fork".to_owned()),
                message_id: None,
                additional_directories: Vec::new(),
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("fork");
        agent
            .close_session(&forked.session_id)
            .await
            .expect("close fork");
        agent
            .close_session(&session.session_id)
            .await
            .expect("close source");
        let loaded = agent
            .load_session(AcpLoadSession {
                session_id: session.session_id,
                cwd: temporary.path().display().to_string(),
                additional_directories: Vec::new(),
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("reload");
        assert_eq!(
            agent
                .history(&loaded.session_id, 0, 50)
                .await
                .expect("reloaded history")
                .entries
                .len(),
            2
        );
    }

    #[test]
    fn session_load_reservation_is_atomic() {
        let agent = Arc::new(AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts"));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let handles = (0..2)
                .map(|_| {
                    let agent = agent.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        agent.reserve_session_load("durable-session")
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("reservation thread"))
                .collect::<Vec<_>>()
        });

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AcpError::SessionConflict(_))))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_close_and_disconnect_finish_the_single_owned_service() {
        let agent = Arc::new(AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts"));
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");

        let harness = agent
            .session_harness(&session.session_id)
            .expect("session harness");
        let service_guard = harness.service.lock().await;
        let closing_agent = agent.clone();
        let closing_session_id = session.session_id.clone();
        let close_task =
            tokio::spawn(async move { closing_agent.close_session(&closing_session_id).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if agent
                    .lock_state()
                    .expect("agent state")
                    .closing_sessions
                    .contains(&session.session_id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close reserves the session");

        let disconnecting_agent = agent.clone();
        let disconnect_task = tokio::spawn(async move { disconnecting_agent.disconnect().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !agent.lock_state().expect("agent state").initialized {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect takes ownership");
        drop(service_guard);

        tokio::time::timeout(Duration::from_secs(1), close_task)
            .await
            .expect("close finishes")
            .expect("close task")
            .expect("session closes");
        tokio::time::timeout(Duration::from_secs(1), disconnect_task)
            .await
            .expect("disconnect finishes")
            .expect("disconnect task")
            .expect("agent disconnects");

        let state = agent.lock_state().expect("agent state");
        assert!(!state.sessions.contains_key(&session.session_id));
        assert!(!state.closing_sessions.contains(&session.session_id));
        assert!(state.closed_sessions.contains(&session.session_id));
        assert!(!state.initialized);
    }

    #[tokio::test]
    async fn restart_stable_user_message_ids_fork_through_the_selected_turn() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let session_root = temporary.path().join("sessions");
        let cwd = temporary.path().to_string_lossy().into_owned();
        let source_id = {
            let agent = AcpAgent::new(
                EchoTurnDriver::new("answer").with_session_root(session_root.clone()),
            )
            .expect("agent starts")
            .with_session_root(session_root.clone());
            agent.initialize().expect("initialize");
            let session = agent
                .new_session(AcpNewSession {
                    cwd: cwd.clone(),
                    additional_directories: None,
                    mcp_servers: Vec::new(),
                    meta: None,
                })
                .expect("session");
            agent
                .prompt(&session.session_id, "first")
                .await
                .expect("first turn");
            agent
                .prompt(&session.session_id, "second")
                .await
                .expect("second turn");
            agent
                .close_session(&session.session_id)
                .await
                .expect("close source");
            session.session_id
        };

        let agent =
            AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
                .expect("agent restarts")
                .with_session_root(session_root);
        agent.initialize().expect("initialize restarted agent");
        let fork = agent
            .fork_session(AcpForkSession {
                session_id: source_id,
                cwd,
                new_session_id: Some("turn-prefix-fork".to_owned()),
                message_id: Some("history:0:user".to_owned()),
                additional_directories: Vec::new(),
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("fork from replayed user ID");
        let history = agent
            .history(&fork.session_id, 0, 50)
            .await
            .expect("fork history");
        assert_eq!(
            history
                .entries
                .iter()
                .filter_map(|entry| entry.get("content").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            ["first", "answer"]
        );
        assert_eq!(
            history_entry_updates(&history.entries[0], 0).expect("replay first user")[0]["messageId"],
            "history:0:user"
        );
    }

    #[test]
    fn cumulative_public_entries_emit_only_new_text_and_keep_tool_identity() {
        let message = |text: &str, status: &str| {
            serde_json::from_value::<PublicHistoryEntry>(json!({
                "type": "message",
                "id": "message-1",
                "sessionId": "session-1",
                "turnId": "turn-1",
                "createdAt": 1,
                "updatedAt": 2,
                "generationStatus": status,
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
            }))
            .expect("public message")
        };
        let first = message("Hel", "in_progress");
        let second = message("Hello", "in_progress");
        let completed = message("Hello", "completed");
        let first_updates = public_entry_updates(None, &first).expect("initial update");
        let second_updates = public_entry_updates(Some(&first), &second).expect("delta update");
        let completion_updates =
            public_entry_updates(Some(&second), &completed).expect("completion update");
        assert_eq!(first_updates[0]["content"]["text"], "Hel");
        assert_eq!(second_updates[0]["content"]["text"], "lo");
        assert_eq!(second_updates[0]["messageId"], "message-1");
        assert!(completion_updates.is_empty());

        let reasoning = |text: &str| {
            serde_json::from_value::<PublicHistoryEntry>(json!({
                "type": "reasoning",
                "id": "reasoning-1",
                "sessionId": "session-1",
                "turnId": "turn-1",
                "createdAt": 1,
                "updatedAt": 2,
                "generationStatus": "in_progress",
                "text": text,
            }))
            .expect("public reasoning")
        };
        let first_reasoning = reasoning("think");
        let second_reasoning = reasoning("thinking");
        let reasoning_delta = public_entry_updates(Some(&first_reasoning), &second_reasoning)
            .expect("reasoning delta");
        assert_eq!(reasoning_delta[0]["content"]["text"], "ing");
        assert_eq!(reasoning_delta[0]["messageId"], "reasoning-1");

        let effect = |status: &str, generation_status: &str, output_text: &str| {
            serde_json::from_value::<PublicHistoryEntry>(json!({
                "type": "effect",
                "id": "tool-1",
                "sessionId": "session-1",
                "turnId": "turn-1",
                "createdAt": 1,
                "updatedAt": 2,
                "generationStatus": generation_status,
                "title": "Shell",
                "detail": {"command": "cargo check"},
                "state": {
                    "status": status,
                    "outputText": output_text,
                    "output": {"ok": true},
                    "durationMs": 1,
                    "display": {},
                },
            }))
            .expect("public effect")
        };
        let running = effect("running", "in_progress", "a");
        let completed_effect = effect("completed", "completed", "ab");
        let started = public_entry_updates(None, &running).expect("tool starts");
        let progressed =
            public_entry_updates(Some(&running), &completed_effect).expect("tool progresses");
        assert_eq!(started[0]["sessionUpdate"], "tool_call");
        assert_eq!(progressed[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(progressed[0]["toolCallId"], "tool-1");
        assert_eq!(progressed[0]["content"][0]["content"]["text"], "b");
        assert_eq!(progressed[0]["status"], "completed");
    }

    #[test]
    fn structured_history_replay_preserves_roles_content_and_effects() {
        assert!(
            history_entry_updates(
                &json!({"role": "system", "content": "private system prompt"}),
                0,
            )
            .expect("system replay")
            .is_empty()
        );

        let assistant = history_entry_updates(
            &json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "image", "data": "AA==", "mimeType": "image/png"},
                    {
                        "type": "resource_link",
                        "name": "docs",
                        "uri": "file:///workspace/docs.md"
                    }
                ],
                "reasoning": "reason",
                "tool_calls": [{
                    "id": "call-1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"src/lib.rs\"}"
                }]
            }),
            1,
        )
        .expect("assistant replay");
        assert_eq!(
            assistant
                .iter()
                .map(|update| update["sessionUpdate"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            [
                "agent_thought_chunk",
                "agent_message_chunk",
                "agent_message_chunk",
                "agent_message_chunk",
                "tool_call",
            ]
        );
        assert_eq!(assistant[2]["content"]["type"], "image");
        assert_eq!(assistant[3]["content"]["type"], "resource_link");
        assert_eq!(assistant[4]["toolCallId"], "call-1");

        let tool = history_entry_updates(
            &json!({
                "role": "tool",
                "call_id": "call-1",
                "content": "file body",
                "is_error": false,
            }),
            2,
        )
        .expect("tool replay");
        assert_eq!(tool[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(tool[0]["toolCallId"], "call-1");
        assert_eq!(tool[0]["status"], "completed");
        assert!(
            assistant
                .iter()
                .chain(&tool)
                .all(|update| update["content"]["text"] != "private system prompt")
        );
    }

    #[test]
    fn prompt_and_mcp_projection_reject_oversized_or_malformed_untrusted_inputs() {
        assert!(matches!(
            turn_request(vec![json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "====",
            })]),
            Err(AcpError::InvalidParams(_))
        ));
        assert!(matches!(
            turn_request(vec![json!({"type": "resource", "text": "missing URI"})]),
            Err(AcpError::InvalidParams(_))
        ));
        assert!(matches!(
            turn_request(
                (0..=MAX_PROMPT_BLOCKS)
                    .map(|_| json!({"type": "text", "text": "x"}))
                    .collect()
            ),
            Err(AcpError::InvalidParams(_))
        ));

        let projected = project_acp_mcp_servers(&[json!({
            "type": "http",
            "name": "remote",
            "url": "https://mcp.example.test",
            "headers": [{"name": "Authorization", "value": "token"}],
        })])
        .expect("HTTP MCP projects");
        assert_eq!(projected[0]["transport"], "streamable-http");
        assert_eq!(projected[0]["headers"]["Authorization"], "token");
        assert!(matches!(
            project_acp_mcp_servers(&[json!({
                "type": "sse",
                "name": "legacy",
                "url": "https://mcp.example.test",
            })]),
            Err(AcpError::UnsupportedClientFlow(_))
        ));
    }

    #[test]
    fn production_cloud_is_lazy_when_the_configured_credential_is_absent() {
        const MISSING: &str = "VIBE_ACP_CLOUD_CREDENTIAL_MUST_REMAIN_UNSET_582F1";
        assert!(std::env::var_os(MISSING).is_none());
        let temporary = tempfile::tempdir().expect("temporary session root");
        let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
            .expect("agent starts")
            .with_session_root(temporary.path().join("sessions"))
            .with_credential_environment(MISSING)
            .with_production_cloud();
        agent.initialize().expect("initialize needs no credential");
        let session = agent
            .new_session(AcpNewSession {
                cwd: temporary.path().to_string_lossy().into_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("new session needs no cloud credential");
        assert_eq!(
            agent
                .list_sessions(Some(&temporary.path().to_string_lossy()), None)
                .expect("list needs no cloud credential")
                .sessions[0]
                .session_id,
            session.session_id
        );
    }

    #[tokio::test]
    async fn teleport_command_streams_public_cloud_events_and_completion_url() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let cwd = temporary.path().to_string_lossy().into_owned();
        let session_root = temporary.path().join("sessions");
        let release4 = Release4Service::with_backends(
            Arc::new(FixtureProjectCloud),
            Arc::new(FixtureTeleportCloud),
            Arc::new(CleanFixtureGit),
        );
        let open = release4
            .dispatch(
                "vibeCode/projects/open",
                &serde_json::from_value(json!({
                    "sessionId": "link-seed",
                    "workingDirectory": cwd,
                    "purpose": "configure",
                }))
                .expect("project open params"),
            )
            .expect("project picker");
        let picker_id = open.result["pickerId"]
            .as_str()
            .expect("picker ID")
            .to_owned();
        release4
            .dispatch(
                "vibeCode/projects/select",
                &serde_json::from_value(json!({
                    "sessionId": "link-seed",
                    "pickerId": picker_id,
                    "projectId": "project-1",
                }))
                .expect("project selection params"),
            )
            .expect("project link");

        let agent =
            AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
                .expect("agent starts")
                .with_session_root(session_root)
                .with_release4_service(release4);
        agent.initialize().expect("initialize");
        let session = agent
            .new_session(AcpNewSession {
                cwd,
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        agent
            .prompt(&session.session_id, "context to continue")
            .await
            .expect("history prompt");

        let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);
        let response = agent
            .prompt_streaming(&session.session_id, "/teleport", sender)
            .await
            .expect("Teleport command");
        assert_eq!(response, command_response());
        let updates = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|update| update.update)
            .collect::<Vec<_>>();
        let statuses = updates
            .iter()
            .filter_map(|update| update.pointer("/_meta/teleport/status"))
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(statuses.contains(&"summarizing_context"), "{statuses:?}");
        assert!(statuses.contains(&"preparing_workspace"), "{statuses:?}");
        assert!(statuses.contains(&"starting_workflow"), "{statuses:?}");
        assert!(statuses.contains(&"completed"), "{statuses:?}");
        assert!(updates.iter().any(|update| {
            update["status"] == "completed"
                && update
                    .pointer("/rawOutput/url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.starts_with("https://cloud.example.test/teleport/"))
        }));
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn list_pagination_is_stable_filtered_and_never_repeats_active_sessions() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session_root = temporary.path().join("sessions");
        let first_cwd = temporary.path().join("first");
        let second_cwd = temporary.path().join("second");
        std::fs::create_dir_all(&first_cwd).expect("first cwd");
        std::fs::create_dir_all(&second_cwd).expect("second cwd");
        let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
            .expect("agent")
            .with_session_root(session_root);
        agent.initialize().expect("initialize");

        for _ in 0..SESSION_LIST_PAGE_SIZE.saturating_add(1) {
            agent
                .new_session(AcpNewSession {
                    cwd: first_cwd.to_string_lossy().into_owned(),
                    additional_directories: None,
                    mcp_servers: Vec::new(),
                    meta: None,
                })
                .expect("first cwd session");
        }
        agent
            .new_session(AcpNewSession {
                cwd: second_cwd.to_string_lossy().into_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("second cwd session");

        let first = agent
            .list_sessions(Some(&first_cwd.to_string_lossy()), None)
            .expect("first page");
        assert_eq!(first.sessions.len(), SESSION_LIST_PAGE_SIZE);
        let cursor = first.next_cursor.as_deref().expect("next cursor");
        let second = agent
            .list_sessions(Some(&first_cwd.to_string_lossy()), Some(cursor))
            .expect("second page");
        assert_eq!(second.sessions.len(), 1);
        assert!(second.next_cursor.is_none());
        assert!(
            first
                .sessions
                .iter()
                .chain(&second.sessions)
                .all(|session| {
                    session.cwd == first_cwd.to_string_lossy()
                        && session.meta.get("active") == Some(&json!(true))
                })
        );
        let ids = first
            .sessions
            .iter()
            .chain(&second.sessions)
            .map(|session| &session.session_id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), SESSION_LIST_PAGE_SIZE.saturating_add(1));

        let other = agent
            .list_sessions(Some(&second_cwd.to_string_lossy()), None)
            .expect("other cwd");
        assert_eq!(other.sessions.len(), 1);
        assert_eq!(other.sessions[0].cwd, second_cwd.to_string_lossy());
        assert!(matches!(
            agent.list_sessions(None, Some("not-a-cursor")),
            Err(AcpError::InvalidParams(_))
        ));
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn load_and_fork_reconstruct_settings_and_attach_roots_and_stdio_mcp() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session_root = temporary.path().join("sessions");
        let cwd = temporary.path().join("workspace");
        let additional = temporary.path().join("shared");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&additional).expect("additional root");
        let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
            .expect("agent")
            .with_session_root(session_root);
        agent.initialize().expect("initialize");
        let source = agent
            .new_session(AcpNewSession {
                cwd: cwd.to_string_lossy().into_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("source");
        agent
            .set_mode(&source.session_id, "plan")
            .await
            .expect("mode");
        agent
            .set_config_option(&source.session_id, "thinking", "high")
            .await
            .expect("thinking");
        let mcp_server = json!({
            "type": "stdio",
            "name": "fixture",
            "command": "/bin/false",
            "args": [],
            "env": [{"name": "FIXTURE", "value": "1"}],
        });
        let forked = agent
            .fork_session(AcpForkSession {
                session_id: source.session_id.clone(),
                cwd: cwd.to_string_lossy().into_owned(),
                new_session_id: Some("settings-fork".to_owned()),
                message_id: None,
                additional_directories: vec![additional.to_string_lossy().into_owned()],
                mcp_servers: vec![mcp_server.clone()],
                meta: None,
            })
            .expect("fork");
        assert_eq!(
            forked
                .modes
                .as_ref()
                .and_then(|modes| modes.get("currentModeId")),
            Some(&json!("plan"))
        );
        assert_eq!(
            forked.config_options.as_ref().and_then(|options| {
                options
                    .iter()
                    .find(|option| option["id"] == "thinking")
                    .map(|option| option["currentValue"].clone())
            }),
            Some(json!("high"))
        );
        let harness = agent
            .session_harness(&forked.session_id)
            .expect("fork harness");
        let mut service = harness.service.lock().await;
        let view = service.session(&forked.session_id).expect("fork view");
        assert_eq!(
            view.intent.add_directories,
            [additional.to_string_lossy().into_owned()]
        );
        assert_eq!(view.intent.mcp_servers[0]["transport"], "stdio");
        assert_eq!(view.intent.mcp_servers[0]["env"]["FIXTURE"], "1");
        drop(service);

        assert!(matches!(
            agent.fork_session(AcpForkSession {
                session_id: source.session_id.clone(),
                cwd: cwd.to_string_lossy().into_owned(),
                new_session_id: None,
                message_id: Some("message-1".to_owned()),
                additional_directories: Vec::new(),
                mcp_servers: Vec::new(),
                meta: None,
            }),
            Err(AcpError::InvalidParams(_))
        ));

        agent
            .close_session(&forked.session_id)
            .await
            .expect("close fork");
        let loaded = agent
            .load_session(AcpLoadSession {
                session_id: forked.session_id,
                cwd: cwd.to_string_lossy().into_owned(),
                additional_directories: vec![additional.to_string_lossy().into_owned()],
                mcp_servers: vec![mcp_server],
                meta: None,
            })
            .expect("load");
        assert_eq!(
            loaded
                .modes
                .as_ref()
                .and_then(|modes| modes.get("currentModeId")),
            Some(&json!("plan"))
        );
        assert_eq!(
            loaded.config_options.as_ref().and_then(|options| {
                options
                    .iter()
                    .find(|option| option["id"] == "thinking")
                    .map(|option| option["currentValue"].clone())
            }),
            Some(json!("high"))
        );
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn mode_and_thinking_updates_change_the_next_turn_reservation_intent() {
        let reservations = Arc::new(Mutex::new(Vec::new()));
        let agent =
            AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");

        agent
            .prompt(&session.session_id, "before settings")
            .await
            .expect("initial prompt");
        agent
            .set_mode(&session.session_id, "plan")
            .await
            .expect("mode changes");
        agent
            .set_config_option(&session.session_id, "thinking", "high")
            .await
            .expect("thinking changes");
        agent
            .prompt(&session.session_id, "after settings")
            .await
            .expect("updated prompt");

        let reservations = reservations.lock().expect("reservations");
        assert_eq!(reservations.len(), 2);
        assert_eq!(reservations[0].intent.mode.as_deref(), Some("code"));
        assert!(reservations[0].intent.thinking);
        assert_eq!(
            reservations[0].intent.reasoning_effort.as_deref(),
            Some("medium")
        );
        assert_eq!(reservations[1].intent.mode.as_deref(), Some("plan"));
        assert!(reservations[1].intent.thinking);
        assert_eq!(
            reservations[1].intent.reasoning_effort.as_deref(),
            Some("high")
        );
    }

    #[tokio::test]
    async fn embedded_resource_context_reaches_the_turn_driver() {
        let reservations = Arc::new(Mutex::new(Vec::new()));
        let agent =
            AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        let resource = json!({
            "type": "resource",
            "uri": "file:///workspace/context.txt",
            "text": "embedded context",
        });
        let (sender, _updates) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);

        agent
            .prompt_content_streaming(
                &session.session_id,
                vec![
                    json!({"type": "text", "text": "use the context"}),
                    resource.clone(),
                ],
                sender,
            )
            .await
            .expect("prompt");

        let reservations = reservations.lock().expect("reservations");
        let reservation = reservations.last().expect("recorded reservation");
        assert_eq!(reservation.prompt, "use the context");
        assert_eq!(
            reservation.input,
            vec![
                PublicContentBlock::Text {
                    text: "use the context".to_owned(),
                },
                PublicContentBlock::Resource { resource },
            ]
        );
    }

    #[tokio::test]
    async fn simultaneous_sessions_are_isolated_and_missing_sessions_fail_closed() {
        let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let first = agent
            .new_session(AcpNewSession {
                cwd: "/first".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("first session");
        let second = agent
            .new_session(AcpNewSession {
                cwd: "/second".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("second session");
        assert_ne!(first.session_id, second.session_id);
        let (first_result, second_result) = tokio::join!(
            agent.prompt(&first.session_id, "first"),
            agent.prompt(&second.session_id, "second")
        );
        assert!(first_result.is_ok());
        assert!(second_result.is_ok());
        assert!(matches!(
            agent.prompt("missing", "question").await,
            Err(AcpError::SessionNotFound(_))
        ));
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn idle_cancel_is_a_noop_and_close_is_idempotent_for_owned_sessions() {
        let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        agent
            .cancel(&session.session_id)
            .await
            .expect("idle cancellation is a no-op");
        agent
            .close_session(&session.session_id)
            .await
            .expect("first close");
        agent
            .close_session(&session.session_id)
            .await
            .expect("repeated close");
        assert!(matches!(
            agent.prompt(&session.session_id, "after close").await,
            Err(AcpError::SessionNotFound(_))
        ));
        assert!(matches!(
            agent.close_session("unknown").await,
            Err(AcpError::SessionNotFound(_))
        ));
        agent.disconnect().await.expect("disconnect");
    }

    struct RecordingClient {
        calls: Mutex<Vec<String>>,
        delay: Duration,
    }

    impl AcpClientPort for RecordingClient {
        fn request<'a>(&'a self, method: &'a str, _params: Value) -> AcpClientFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| "lock poisoned".to_owned())?
                    .push(method.to_owned());
                tokio::time::sleep(self.delay).await;
                Ok(json!({"ok": true}))
            })
        }
    }

    struct PermissionClient {
        params: Mutex<Option<Value>>,
    }

    impl AcpClientPort for PermissionClient {
        fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a> {
            Box::pin(async move {
                if method != "session/request_permission" {
                    return Err(format!("unexpected method `{method}`"));
                }
                *self.params.lock().map_err(|_| "lock poisoned".to_owned())? = Some(params);
                Ok(json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_always",
                    }
                }))
            })
        }
    }

    struct ApprovalInvokingDriver {
        inner: EchoTurnDriver,
    }

    impl TurnDriver for ApprovalInvokingDriver {
        fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
            Box::pin(async move {
                reservation
                    .tools
                    .invoke(
                        "read",
                        ToolInvocation {
                            call_id: "read-for-approval".to_owned(),
                            arguments: json!({"path": "approval.txt"}),
                        },
                    )
                    .await
                    .map_err(|error| DriverError::Tool(error.to_string()))?;
                self.inner.run(reservation).await
            })
        }
    }

    struct UserInputOnceDriver {
        inner: EchoTurnDriver,
        attempts: AtomicU64,
    }

    impl TurnDriver for UserInputOnceDriver {
        fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
            Box::pin(async move {
                if self.attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    let result = reservation
                        .tools
                        .invoke(
                            "ask_user_question",
                            ToolInvocation {
                                call_id: "unsupported-user-input".to_owned(),
                                arguments: json!({
                                    "questions": [{
                                        "question": "Choose one",
                                        "header": "Choice",
                                        "options": [
                                            {"label": "First", "description": "First choice"},
                                            {"label": "Second", "description": "Second choice"}
                                        ],
                                        "multiSelect": false,
                                        "hideOther": true
                                    }]
                                }),
                            },
                        )
                        .await;
                    return Err(DriverError::Tool(match result {
                        Ok(_) => "unsupported ACP user input unexpectedly succeeded".to_owned(),
                        Err(error) => error.to_string(),
                    }));
                }
                self.inner.run(reservation).await
            })
        }
    }

    struct BlockingPermissionClient {
        started: tokio::sync::Notify,
    }

    impl AcpClientPort for BlockingPermissionClient {
        fn request<'a>(&'a self, method: &'a str, _params: Value) -> AcpClientFuture<'a> {
            Box::pin(async move {
                if method != "session/request_permission" {
                    return Err(format!("unexpected method `{method}`"));
                }
                self.started.notify_one();
                std::future::pending::<Result<Value, String>>().await
            })
        }
    }

    #[test]
    fn approval_choices_and_invalid_client_outcomes_fail_closed() {
        let options = acp_approval_options(&json!({
            "choices": ["approve", "deny", "cancel_turn"],
        }));
        assert_eq!(
            options
                .iter()
                .filter_map(|option| option.get("optionId").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            ["allow_once", "reject_once"]
        );
        assert_eq!(
            approval_output_from_acp(
                Some(&json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once",
                    }
                })),
                &options,
            )["decision"]["type"],
            "approve"
        );
        for response in [
            None,
            Some(json!({"outcome": {"outcome": "cancelled"}})),
            Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_always",
                }
            })),
            Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "unknown",
                }
            })),
        ] {
            assert_eq!(
                approval_output_from_acp(response.as_ref(), &options)["decision"]["type"],
                "cancel_turn"
            );
        }
    }

    #[tokio::test]
    async fn canonical_approval_callback_routes_through_the_acp_client() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(directory.path().join("approval.txt"), "approved\n")
            .expect("workspace file");
        let client = Arc::new(PermissionClient {
            params: Mutex::new(None),
        });
        let agent = AcpAgent::new(ApprovalInvokingDriver {
            inner: EchoTurnDriver::new("answer"),
        })
        .expect("agent starts")
        .with_client_port(client.clone(), Duration::from_secs(1));
        agent.initialize().expect("initialize");
        let session = agent
            .new_session(AcpNewSession {
                cwd: directory.path().to_string_lossy().into_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        agent
            .prompt(&session.session_id, "read the file")
            .await
            .expect("approved prompt");
        let params = client
            .params
            .lock()
            .expect("permission params")
            .clone()
            .expect("permission request");
        assert_eq!(params["sessionId"], session.session_id);
        assert_eq!(
            params["options"][0]["kind"],
            serde_json::Value::String("allow_once".to_owned())
        );
        assert_eq!(params["toolCall"]["rawInput"]["effect"]["toolName"], "read");
        assert!(
            params["toolCall"]["rawInput"]["requiredPermissions"][0]
                .as_str()
                .is_some_and(|permission| permission.starts_with("read "))
        );
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn unsupported_user_input_is_denied_without_reaching_the_acp_client() {
        let directory = tempfile::tempdir().expect("workspace");
        let client = Arc::new(PermissionClient {
            params: Mutex::new(None),
        });
        let agent = AcpAgent::new(UserInputOnceDriver {
            inner: EchoTurnDriver::new("answer"),
            attempts: AtomicU64::new(0),
        })
        .expect("agent starts")
        .with_client_port(client.clone(), Duration::from_secs(1));
        agent.initialize().expect("initialize");
        let session = agent
            .new_session(AcpNewSession {
                cwd: directory.path().to_string_lossy().into_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");

        assert!(matches!(
            agent.prompt(&session.session_id, "ask").await,
            Err(AcpError::Driver(_))
        ));
        assert!(client.params.lock().expect("permission params").is_none());
        agent
            .prompt(&session.session_id, "continue")
            .await
            .expect("unsupported callback released the session");
        agent.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn disconnect_cancels_a_pending_canonical_approval() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(directory.path().join("approval.txt"), "approval\n")
            .expect("workspace file");
        let client = Arc::new(BlockingPermissionClient {
            started: tokio::sync::Notify::new(),
        });
        let agent = Arc::new(
            AcpAgent::new(ApprovalInvokingDriver {
                inner: EchoTurnDriver::new("answer"),
            })
            .expect("agent starts")
            .with_client_port(client.clone(), Duration::from_secs(30)),
        );
        agent.initialize().expect("initialize");
        let session = agent
            .new_session(AcpNewSession {
                cwd: directory.path().to_string_lossy().into_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        let prompt_agent = agent.clone();
        let session_id = session.session_id;
        let prompt = tokio::spawn(async move { prompt_agent.prompt(&session_id, "read").await });
        tokio::time::timeout(Duration::from_secs(1), client.started.notified())
            .await
            .expect("permission request started");
        tokio::time::timeout(Duration::from_secs(1), agent.disconnect())
            .await
            .expect("disconnect did not hang")
            .expect("disconnect");
        let result = tokio::time::timeout(Duration::from_secs(1), prompt)
            .await
            .expect("prompt did not stop")
            .expect("prompt task");
        assert!(result.is_err(), "disconnect must not approve pending work");
    }

    #[tokio::test]
    async fn client_tool_factory_registers_capabilities_and_invokes_the_client_port() {
        let client = Arc::new(RecordingClient {
            calls: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let factory = AcpClientToolFactory {
            client: Some(client.clone()),
            capabilities: AcpClientCapabilities {
                fs: AcpFilesystemCapabilities {
                    read_text_file: true,
                    write_text_file: false,
                },
                terminal: true,
                session: Value::Null,
                meta: None,
            },
            timeout: Duration::from_secs(1),
        };
        let tools = ToolRegistry::default();

        factory
            .register("session-1", &tools)
            .expect("client tools register");
        let names = tools
            .list()
            .expect("tool specs")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "acp_read_text_file",
                "acp_terminal_create",
                "acp_terminal_kill",
                "acp_terminal_output",
                "acp_terminal_release",
                "acp_terminal_wait_for_exit",
            ]
        );

        let output = tools
            .invoke(
                "acp_read_text_file",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"path": "src/lib.rs"}),
                },
            )
            .await
            .expect("client tool invocation");
        assert_eq!(output.typed_result, json!({"ok": true}));
        assert_eq!(
            client.calls.lock().expect("calls").as_slice(),
            ["fs/read_text_file"]
        );
    }

    #[tokio::test]
    async fn client_tools_are_capability_scoped_and_timeout_without_cross_session_state() {
        let client = Arc::new(RecordingClient {
            calls: Mutex::new(Vec::new()),
            delay: Duration::from_millis(50),
        });
        let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
            .expect("agent starts")
            .with_client_port(client.clone(), Duration::from_millis(5));
        agent
            .initialize_with(AcpInitializeRequest {
                protocol_version: 1,
                client_capabilities: AcpClientCapabilities {
                    fs: AcpFilesystemCapabilities {
                        read_text_file: true,
                        write_text_file: false,
                    },
                    terminal: true,
                    session: Value::Null,
                    meta: None,
                },
                client_info: None,
                meta: None,
            })
            .expect("initialize");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            })
            .expect("session");
        assert!(matches!(
            agent
                .client_tool(
                    "fs/read_text_file",
                    json!({"sessionId": session.session_id, "path": "src/lib.rs"})
                )
                .await,
            Err(AcpError::ClientToolTimeout(_))
        ));
        assert_eq!(
            client.calls.lock().expect("calls").as_slice(),
            ["fs/read_text_file"]
        );
        assert!(agent.deny_unsupported_user_input().is_err());
        agent.disconnect().await.expect("disconnect");
    }

    #[test]
    fn malformed_lifecycle_requests_are_rejected_without_runtime_mutation() {
        let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        assert!(matches!(
            agent.new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
                meta: None,
            }),
            Err(AcpError::NotInitialized)
        ));
        assert!(matches!(
            agent.initialize_with(AcpInitializeRequest {
                protocol_version: 99,
                ..AcpInitializeRequest::default()
            }),
            Err(AcpError::UnsupportedProtocol(99))
        ));
    }
}
