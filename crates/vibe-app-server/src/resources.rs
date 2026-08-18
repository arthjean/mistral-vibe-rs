use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::host::now_millis;
use crate::params::{self, optional_string, required_string, usize_param};
use crate::vocabulary::{McpSourceKind, McpSourceStatus};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;
use vibe_core::config::{ConfigSnapshot, LayeredConfig};
use vibe_core::integrations::{
    ConnectorAuthKind, ConnectorAuthState, ConnectorBackend, ConnectorDefinition,
    ConnectorRegistry, ConnectorView, redact,
};
use vibe_core::mcp::{
    DefaultMcpPeerFactory, McpPeerFactory, McpRegistry, McpServerConfig, McpServerStatus,
    McpServerView, McpTransportConfig,
};
use vibe_core::observability::{FileLog, LOG_DEFAULT_PAGE_LIMIT, LogLevel, entry_identity};
use vibe_core::platform::{Platform, parse_policy_path};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode,
    PermissionStore, TrustDecision, TrustRootKind,
};
use vibe_core::process::{ProcessSpec, TerminalManager};
use vibe_core::shell::{ShellCommandLists, ShellConfig, ShellPolicyContext, analyze_shell};
use vibe_core::tools::ToolRegistry;
use vibe_core::tools::config::ShellCommandConfig;

mod backend_command;
mod core_backend;
mod mcp_oauth;
mod mistral_connector;
mod views;

use views::*;

pub use backend_command::{
    ConnectorCommand, McpAddTransport, McpCommand, ResourceBackendCommand, ShellCommand,
};
pub use mcp_oauth::production_mcp_adapters;
pub use mistral_connector::MistralConnectorClient;

pub const RESOURCE_METHODS: &[&str] = &[
    "account/read",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "connectors/toggle",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "mcp/add",
    "mcp/auth/complete",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "narration/summarize",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/state",
    "review/turnDiff",
    "runtime/read",
    "session/ready/read",
    "session/ready/wait",
    "shell/interrupt",
    "shell/run",
    "stats/read",
    "telemetry/record",
    "tools/list",
    "workspace/trust/decision",
    "workspace/trust/status",
];

pub const BACKEND_RESOURCE_METHODS: &[&str] = &[
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "connectors/toggle",
    "mcp/add",
    "mcp/auth/complete",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "shell/interrupt",
    "shell/run",
];

/// The transport a connector is published under. Reference `project_mcp`
/// spells it the same way, because a connector reaches its tools through the
/// gateway rather than through a transport an operator configured.
const CONNECTOR_TRANSPORT: &str = "connector";

const MAX_RESOURCE_RECORDS: usize = 1_024;
const MAX_RESOURCE_SESSIONS: usize = 256;
const MAX_FEEDBACK_ACTIONS: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDispatch {
    pub result: BTreeMap<String, Value>,
    pub signals: ResourceSignals,
}

/// What a resource dispatch asks the server to publish after its answer.
///
/// The reference carries the same three facts on its `DispatchResult`: whether
/// runtime state moved, what went wrong recoverably, and the authorization URL
/// a source is waiting on. Naming the facts rather than a notification keeps the
/// wire vocabulary in one place, where it can stay the reference's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceSignals {
    /// The dispatch changed runtime state, so `runtime/updated` follows it.
    pub runtime_updated: bool,
    /// Recoverable problems, each published as its own `warning`.
    pub warnings: Vec<String>,
    /// An MCP source waiting on authorization, published as `mcp/authUrl`.
    pub auth_url: Option<McpAuthUrl>,
    /// What the backend knows about this session's integrations after the
    /// dispatch.
    ///
    /// The runtime snapshot is composed synchronously and the backend answers
    /// asynchronously, so the state travels back on the answer and is recorded
    /// for the next composition rather than being fetched during it.
    pub integrations: Option<IntegrationState>,
}

/// The integration surface a session publishes: every MCP source it can reach
/// and how many connectors are behind them.
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationState {
    /// An `MCPState`.
    pub mcp: Value,
    /// A `ConnectorCounts`.
    pub counts: Value,
}

/// The authorization an MCP source is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthUrl {
    pub name: String,
    pub url: String,
}

#[derive(Clone)]
pub struct ResourceSession {
    pub session_id: String,
    pub generation: u64,
    pub working_directory: String,
    pub project_trusted: bool,
    pub policy: PermissionStore,
    pub tools: ToolRegistry,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceBackendRequest {
    pub session_id: String,
    pub command: ResourceBackendCommand,
}

impl ResourceBackendRequest {
    pub fn parse(
        session_id: String,
        method: &str,
        params: &BTreeMap<String, Value>,
        session_active: bool,
    ) -> Result<Self, ResourceError> {
        Ok(Self {
            session_id,
            command: ResourceBackendCommand::parse(method, params, session_active)?,
        })
    }
}

pub type ResourceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ResourceError>> + Send + 'a>>;

pub trait McpAuthBackend: Send + Sync {
    fn login<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, String>;

    fn complete<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, bool>;

    fn logout<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, ()>;

    fn close_session<'a>(&'a self, session_id: &'a str) -> ResourceFuture<'a, ()>;
}

pub trait ConnectorAuthBackend: Send + Sync {
    fn auth_url<'a>(
        &'a self,
        session_id: &'a str,
        connector_id: &'a str,
    ) -> ResourceFuture<'a, Option<String>>;

    fn refresh<'a>(
        &'a self,
        session_id: &'a str,
        connector_id: &'a str,
    ) -> ResourceFuture<'a, bool>;
}

pub trait ConnectorCatalogBackend: Send + Sync {
    fn catalog<'a>(&'a self) -> ResourceFuture<'a, ConnectorCatalog>;
}

pub struct ConnectorCatalog {
    pub definitions: Vec<ConnectorDefinition>,
    pub connected: BTreeSet<String>,
}

pub trait ResourceBackend: Send + Sync {
    fn open_session(&self, session: ResourceSession) -> Result<(), ResourceError>;

    fn configure_mcp<'a>(
        &'a self,
        _session_id: &'a str,
        _configs: Vec<McpServerConfig>,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async {
            Err(ResourceError::Unavailable(
                "MCP transport backend is not configured".to_owned(),
            ))
        })
    }

    fn dispatch<'a>(
        &'a self,
        request: ResourceBackendRequest,
    ) -> ResourceFuture<'a, ResourceDispatch>;

    fn close_session<'a>(
        &'a self,
        _session_id: &'a str,
        _generation: u64,
    ) -> ResourceFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

pub use core_backend::CoreResourceBackend;
use core_backend::CoreResourceSession;

#[derive(Debug, Clone)]
struct McpSource {
    name: String,
    transport: String,
    status: String,
    tools: BTreeMap<String, bool>,
}

/// The session-scoped resource state.
///
/// The six `review/*` methods used to be answered from a map held here. They
/// are answered from the session's checkpoint engine now, in
/// `crate::server::review`, so nothing about a review lives in this service.
#[derive(Clone, Default)]
pub struct ResourceService {
    ready: bool,
    /// The last integration state each session's backend reported.
    backend_integrations: BTreeMap<String, IntegrationState>,
    diagnostics: Vec<(String, String)>,
    /// The file `diagnostics/logs/read` answers from and [`Self::record_log`]
    /// writes to. Reference constructs a `LogReader` over `LOG_FILE` beside the
    /// session; unset here means no home was resolved, and the method answers
    /// an empty page rather than an error.
    log: Option<FileLog>,
    feedback_actions: Vec<String>,
    connectors: BTreeMap<String, bool>,
    mcp: BTreeMap<String, McpSource>,
    policy_stores: BTreeMap<String, PermissionStore>,
    tool_registries: BTreeMap<String, ToolRegistry>,
}

impl ResourceService {
    /// The log file this service records to and answers `diagnostics/logs/read`
    /// from. Reference builds its `LogReader` over `LOG_FILE` directly; the
    /// path is passed in here so a test, and a second server in the same
    /// process, read the file they wrote rather than the operator's.
    #[must_use]
    pub fn logging_to(mut self, log: FileLog) -> Self {
        self.set_log(log);
        self
    }

    /// Re-points an already-built service at another file, which is what a
    /// server given a home after construction does.
    pub fn set_log(&mut self, log: FileLog) {
        self.log = Some(log);
    }

    pub fn dispatch(
        &mut self,
        method: &str,
        params: &BTreeMap<String, Value>,
        session_active: bool,
    ) -> Result<ResourceDispatch, ResourceError> {
        if BACKEND_RESOURCE_METHODS.contains(&method) {
            let command = ResourceBackendCommand::parse(method, params, session_active)?;
            return self.dispatch_backend_fallback(command);
        }
        match method {
            "diagnostics/list" => Ok(read_only([
                ("issues", Value::Array(self.reported_issues(None))),
                ("hooksCount", json!(0)),
            ])),
            "diagnostics/logs/read" => self.logs_read(params),
            "feedback/record" => self.feedback_record(params),
            "feedback/shouldShow" => Ok(read_only([(
                "show",
                json!(self.feedback_actions.is_empty()),
            )])),
            "narration/summarize" => self.narration(params),
            "session/ready/read" | "session/ready/wait" => {
                Ok(read_only([("ready", json!(self.ready))]))
            }
            "tools/list" => Ok(read_only([(
                "tools",
                self.tool_list(required_string(params, "sessionId")?)?,
            )])),
            "workspace/trust/status" => self.trust_status(params),
            "workspace/trust/decision" => self.trust_decision(params),
            _ => Err(ResourceError::MethodNotFound(method.to_owned())),
        }
    }

    fn dispatch_backend_fallback(
        &mut self,
        command: ResourceBackendCommand,
    ) -> Result<ResourceDispatch, ResourceError> {
        match command {
            // The counts are the whole answer here too: a fallback that
            // published the sources would answer in a shape
            // `ConnectorsReadResponse` refuses, and the attached backend does
            // not.
            ResourceBackendCommand::Connector(ConnectorCommand::Read) => {
                Ok(read_only([("counts", self.connector_counts())]))
            }
            ResourceBackendCommand::Connector(ConnectorCommand::AuthRead { name }) => {
                let connected = self.connectors.get(&name).ok_or_else(|| {
                    ResourceError::NotFound(format!("connector `{name}` was not found"))
                })?;
                Ok(read_only([(
                    "url",
                    if *connected {
                        Value::Null
                    } else {
                        json!(format!("https://connectors.mistral.ai/auth/{name}"))
                    },
                )]))
            }
            ResourceBackendCommand::Connector(ConnectorCommand::Refresh { name }) => {
                Err(ResourceError::Unavailable(format!(
                    "connector `{name}` refresh backend is not attached"
                )))
            }
            ResourceBackendCommand::Connector(ConnectorCommand::Toggle { .. }) => Err(
                ResourceError::Unavailable("connector toggle backend is not attached".to_owned()),
            ),
            ResourceBackendCommand::Mcp(McpCommand::Read) => {
                Ok(read_only([("mcp", self.mcp_state())]))
            }
            ResourceBackendCommand::Mcp(McpCommand::Add(add)) => {
                Err(ResourceError::Unavailable(format!(
                    "MCP source `{}` cannot be added because no MCP backend is attached",
                    add.requested_alias.as_deref().unwrap_or("mcp")
                )))
            }
            ResourceBackendCommand::Mcp(McpCommand::Login { name }) => {
                Err(ResourceError::Unavailable(format!(
                    "MCP source `{name}` login backend is not attached"
                )))
            }
            ResourceBackendCommand::Mcp(McpCommand::CompleteAuth { name }) => {
                Err(ResourceError::Unavailable(format!(
                    "MCP source `{name}` authentication backend is not attached"
                )))
            }
            ResourceBackendCommand::Mcp(McpCommand::Logout { name }) => {
                Err(ResourceError::Unavailable(format!(
                    "MCP source `{name}` logout backend is not attached"
                )))
            }
            ResourceBackendCommand::Mcp(McpCommand::Refresh { .. }) => Err(
                ResourceError::Unavailable("MCP refresh backend is not attached".to_owned()),
            ),
            ResourceBackendCommand::Mcp(McpCommand::Toggle { name, .. }) => {
                Err(ResourceError::Unavailable(format!(
                    "MCP source `{name}` toggle backend is not attached"
                )))
            }
            ResourceBackendCommand::Shell(ShellCommand::Run { operation_id, .. }) => {
                Err(ResourceError::Unavailable(format!(
                    "shell operation `{operation_id}` cannot run because no shell backend is attached"
                )))
            }
            ResourceBackendCommand::Shell(ShellCommand::Interrupt { operation_id }) => {
                Err(ResourceError::Unavailable(format!(
                    "shell operation `{operation_id}` cannot be interrupted because no shell backend is attached"
                )))
            }
        }
    }

    /// Records what a backend dispatch reported about a session's integrations.
    ///
    /// Bounded by the session capacity the service already enforces, so a
    /// long-lived server cannot accumulate state for sessions that are gone.
    pub(crate) fn record_integrations(&mut self, session_id: &str, state: IntegrationState) {
        if !self.backend_integrations.contains_key(session_id)
            && self.backend_integrations.len() >= MAX_RESOURCE_SESSIONS
            && let Some(oldest) = self.backend_integrations.keys().next().cloned()
        {
            self.backend_integrations.remove(&oldest);
        }
        self.backend_integrations
            .insert(session_id.to_owned(), state);
    }

    /// Whether a session has opened, which `runtime/read` answers with.
    pub(crate) const fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn record_diagnostic(&mut self, file: &str, message: &str) {
        push_bounded(
            &mut self.diagnostics,
            MAX_RESOURCE_RECORDS,
            (bounded(file), redact(message)),
        );
    }

    /// Records a diagnostic unless the same file and message are already
    /// published, so a second session reporting the same unloadable skill adds
    /// nothing. The reference builds one `SkillManager` per agent loop and
    /// projects its issues once; this is what keeps a server hosting several
    /// sessions from reporting one broken file per session.
    pub fn record_diagnostic_once(&mut self, file: &str, message: &str) {
        let entry = (bounded(file), redact(message));
        if self.diagnostics.contains(&entry) {
            return;
        }
        push_bounded(&mut self.diagnostics, MAX_RESOURCE_RECORDS, entry);
    }

    /// Writes one record to the log file this service reads back, or nowhere at
    /// all when no file was resolved. A record never fails a call: an unwritable
    /// file is dropped the way [`vibe_core::observability::log`] drops one.
    pub fn record_log(&self, level: LogLevel, message: &str) {
        if let Some(log) = &self.log {
            drop(log.write(level, &redact(message), None));
        }
    }

    pub fn close_session(&mut self, session_id: &str) {
        self.backend_integrations.remove(session_id);
        self.policy_stores.remove(session_id);
        self.tool_registries.remove(session_id);
        self.ready = !self.policy_stores.is_empty();
    }

    pub fn open_session(
        &mut self,
        session_id: &str,
        policy: PermissionStore,
        tools: ToolRegistry,
    ) -> Result<(), ResourceError> {
        if !self.policy_stores.contains_key(session_id)
            && self.policy_stores.len() >= MAX_RESOURCE_SESSIONS
        {
            return Err(ResourceError::Conflict(
                "resource session capacity was reached".to_owned(),
            ));
        }
        self.policy_stores.insert(session_id.to_owned(), policy);
        self.tool_registries.insert(session_id.to_owned(), tools);
        self.ready = true;
        Ok(())
    }

    /// What a client reading issues is told: the ones recorded while the
    /// sessions started, plus what a permission store could not honor
    /// afterward. `session` narrows the stores to one, or reads every one.
    ///
    /// A permanent approval whose write to `tools.<name>.allowlist` failed is
    /// kept for the session rather than failing the call the operator just
    /// approved, so the reason has to be readable somewhere. This is that
    /// somewhere: the store holds it and these are the two surfaces an operator
    /// asks.
    fn reported_issues(&self, session: Option<&str>) -> Vec<Value> {
        let stores = self
            .policy_stores
            .iter()
            .filter(move |(id, _)| session.is_none_or(|session| session == id.as_str()))
            .map(|(_, store)| store);
        self.diagnostics
            .iter()
            .map(|(file, message)| json!({"file": file, "message": redact(message)}))
            .chain(
                stores
                    .flat_map(PermissionStore::diagnostics)
                    .map(|message| {
                        json!({"file": crate::server::CONFIG_FILE_LABEL, "message": redact(&message)})
                    }),
            )
            .take(MAX_RESOURCE_RECORDS)
            .collect()
    }

    fn connector_counts(&self) -> Value {
        json!({
            "connected": self.connectors.values().filter(|connected| **connected).count(),
            "total": self.connectors.len()
        })
    }

    /// Reference `_diagnostics_logs_read`: one page of the log file, newest
    /// first, parsed into the seven fields a debug console renders.
    ///
    /// The page comes from the file rather than from anything this process
    /// remembers, so a line another process wrote is readable and the process
    /// identifiers are the ones that wrote it.
    fn logs_read(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", LOG_DEFAULT_PAGE_LIMIT, 1, 500)?;
        let page = self
            .log
            .as_ref()
            .map(|log| log.reader().get_logs(limit, offset))
            .unwrap_or_default();
        let entries = page
            .entries
            .iter()
            .map(|entry| {
                json!({
                    "id": entry_identity(&entry.raw_line),
                    "timestamp": entry.timestamp,
                    "ppid": entry.ppid,
                    "pid": entry.pid,
                    "level": entry.level,
                    "message": redact(&entry.message),
                    "rawLine": redact(&entry.raw_line)
                })
            })
            .collect::<Vec<_>>();
        Ok(read_only([(
            "logs",
            json!({
                "entries": entries,
                "hasMore": page.has_more,
                "cursor": page.cursor
            }),
        )]))
    }

    fn feedback_record(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let action = required_string(params, "action")?;
        if !matches!(action, "asked" | "given" | "snoozed") {
            return Err(ResourceError::InvalidParams(
                "action must be asked, given, or snoozed".to_owned(),
            ));
        }
        push_bounded(
            &mut self.feedback_actions,
            MAX_FEEDBACK_ACTIONS,
            action.to_owned(),
        );
        Ok(read_only([]))
    }

    fn narration(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let user = required_string(params, "userMessage")?;
        let assistant = required_string(params, "assistantText")?;
        let source = if assistant.trim().is_empty() {
            user
        } else {
            assistant
        };
        if source.trim().is_empty() {
            return Err(ResourceError::InvalidParams(
                "narration input cannot be empty".to_owned(),
            ));
        }
        Ok(read_only([(
            "summary",
            json!(source.chars().take(280).collect::<String>()),
        )]))
    }

    fn trust_status(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let cwd = optional_string(params, "cwd")?.unwrap_or(".");
        let session_id = required_string(params, "sessionId")?;
        let policy = self.policy_stores.get(session_id).ok_or_else(|| {
            ResourceError::NotFound(format!("session `{session_id}` was not found"))
        })?;
        let status = match policy.try_trust_decision(cwd).map_err(policy_error)? {
            Some(TrustDecision::Trusted) => "trusted",
            Some(TrustDecision::SessionTrusted) => "session",
            Some(TrustDecision::Untrusted) | None => "untrusted",
        };
        Ok(read_only([
            ("status", json!(status)),
            (
                "details",
                json!({
                    "cwd": cwd,
                    "repoRoot": null,
                    "detectedFiles": [],
                    "repoDetectedFiles": [],
                    "repoExplicitlyUntrusted": status == "untrusted",
                    "settingsPath": "",
                    "availableDecisions": ["trust_repo", "trust_cwd", "decline"]
                }),
            ),
        ]))
    }

    fn trust_decision(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let cwd = optional_string(params, "cwd")?.unwrap_or(".");
        let session_id = required_string(params, "sessionId")?;
        let decision = required_string(params, "decision")?;
        let (trust, kind) = match decision {
            "trust_repo" => (TrustDecision::Trusted, TrustRootKind::Workspace),
            "trust_cwd" => (TrustDecision::SessionTrusted, TrustRootKind::Workspace),
            "decline" => (TrustDecision::Untrusted, TrustRootKind::Workspace),
            _ => {
                return Err(ResourceError::InvalidParams(
                    "unsupported trust decision".to_owned(),
                ));
            }
        };
        let policy = self.policy_stores.get(session_id).ok_or_else(|| {
            ResourceError::NotFound(format!("session `{session_id}` was not found"))
        })?;
        policy
            .try_set_trust(cwd, trust, kind)
            .map_err(policy_error)?;
        Ok(ResourceDispatch {
            result: BTreeMap::new(),
            signals: ResourceSignals {
                runtime_updated: true,
                ..ResourceSignals::default()
            },
        })
    }

    /// The session's tool surface as `ToolSummary` declares it: a name and
    /// nothing else.
    ///
    /// The full specification a tool carries is not part of this contract. A
    /// client that needs a schema reads it from the tool call it is answering,
    /// which is where the reference publishes it too.
    fn tool_list(&self, session_id: &str) -> Result<Value, ResourceError> {
        let registry = self.tool_registries.get(session_id).ok_or_else(|| {
            ResourceError::NotFound(format!("session `{session_id}` was not found"))
        })?;
        let tools = registry
            .list()
            .map_err(|error| ResourceError::Unavailable(error.to_string()))?;
        Ok(Value::Array(
            tools
                .into_iter()
                .map(|spec| json!({"name": spec.name}))
                .collect(),
        ))
    }

    fn mcp_state(&self) -> Value {
        Value::Object(
            [
                (
                    "sources".to_owned(),
                    Value::Array(
                        self.mcp
                            .values()
                            .map(|source| {
                                json!({
                                    "name": source.name,
                                    "kind": "server",
                                    "transport": source.transport,
                                    "status": source.status,
                                    "tools": source.tools.iter().map(|(name, enabled)| {
                                        json!({"name": name, "description": "", "enabled": enabled})
                                    }).collect::<Vec<_>>()
                                })
                            })
                            .collect(),
                    ),
                ),
                ("discoveryErrors".to_owned(), json!({})),
            ]
            .into_iter()
            .collect::<Map<_, _>>(),
        )
    }

    /// The part of `RuntimeSnapshot` this service owns.
    ///
    /// The configuration, the catalogs and the session's accounting are held
    /// elsewhere; [`crate::server::AppServer`] merges them onto this, which is
    /// why the map is returned rather than a finished response.
    pub(crate) fn runtime(&self, session_id: &str) -> Result<Map<String, Value>, ResourceError> {
        let recorded = self.backend_integrations.get(session_id);
        Ok([
            ("tools".to_owned(), self.tool_list(session_id)?),
            (
                "issues".to_owned(),
                Value::Array(self.reported_issues(Some(session_id))),
            ),
            (
                "connectors".to_owned(),
                recorded.map_or_else(|| self.connector_counts(), |state| state.counts.clone()),
            ),
            (
                "mcp".to_owned(),
                recorded.map_or_else(|| self.mcp_state(), |state| state.mcp.clone()),
            ),
        ]
        .into_iter()
        .collect())
    }
}

/// Reads a JSON response body under a byte budget.
///
/// The budget is enforced while streaming rather than trusting
/// `Content-Length`, so a lying header cannot make the client allocate without
/// bound.
async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    label: &str,
    limit: usize,
) -> Result<T, ResourceError> {
    let exceeded = || ResourceError::Unavailable(format!("{label} exceeded its byte budget"));
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(exceeded());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(exceeded());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ResourceError::Unavailable(format!("{label} was invalid: {error}")))
}

fn integration_error(error: vibe_core::integrations::IntegrationError) -> ResourceError {
    match error {
        vibe_core::integrations::IntegrationError::ConnectorNotFound(name) => {
            ResourceError::NotFound(format!("connector `{name}` was not found"))
        }
        error => ResourceError::Unavailable(redact(&error.to_string())),
    }
}

fn host_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Posix
    }
}

impl From<params::ParamError> for ResourceError {
    fn from(error: params::ParamError) -> Self {
        Self::InvalidParams(error.message())
    }
}

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error("resource method `{0}` was not found")]
    MethodNotFound(String),
    #[error("{0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
}

fn bounded(value: &str) -> String {
    value.chars().take(2_048).collect()
}

fn policy_error(error: vibe_core::policy::PolicyError) -> ResourceError {
    match error {
        vibe_core::policy::PolicyError::Busy => {
            ResourceError::Conflict("permission state is busy; retry the decision".to_owned())
        }
        error => ResourceError::InvalidParams(error.to_string()),
    }
}

fn push_bounded<T>(values: &mut Vec<T>, limit: usize, value: T) {
    if values.len() == limit {
        values.remove(0);
    }
    values.push(value);
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionResourceParams {
    pub session_id: String,
}

#[cfg(test)]
mod resources_tests;
