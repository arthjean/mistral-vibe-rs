use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::host::now_millis;
use crate::params;
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
    logs: Vec<(u64, String, String)>,
    feedback_actions: Vec<String>,
    connectors: BTreeMap<String, bool>,
    mcp: BTreeMap<String, McpSource>,
    policy_stores: BTreeMap<String, PermissionStore>,
    tool_registries: BTreeMap<String, ToolRegistry>,
}

impl ResourceService {
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

    pub fn record_log(&mut self, timestamp: u64, level: &str, message: &str) {
        push_bounded(
            &mut self.logs,
            MAX_RESOURCE_RECORDS,
            (timestamp, bounded(level), redact(message)),
        );
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

    fn logs_read(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let offset = usize_param(params, "offset", 0, 0, usize::MAX)?;
        let limit = usize_param(params, "limit", 100, 1, 500)?;
        let entries = self
            .logs
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(index, (timestamp, level, message))| {
                json!({
                    "id": format!("log-{index}"),
                    "timestamp": timestamp,
                    "ppid": 0,
                    "pid": 0,
                    "level": level,
                    "message": redact(message),
                    "rawLine": redact(message)
                })
            })
            .collect::<Vec<_>>();
        let cursor = offset.saturating_add(entries.len());
        Ok(read_only([(
            "logs",
            json!({
                "entries": entries,
                "hasMore": cursor < self.logs.len(),
                "cursor": (cursor < self.logs.len()).then_some(cursor)
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

fn read_only<const N: usize>(entries: [(&str, Value); N]) -> ResourceDispatch {
    ResourceDispatch {
        result: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        signals: ResourceSignals::default(),
    }
}

/// A mutation that moved runtime state, with whatever it could not do cleanly.
///
/// The answer carries only what the method's own response declares. The runtime
/// is filled in by the server, which is the only owner able to compose it, and
/// each diagnostic is published as its own `warning`, which is how the reference
/// splits an answer from what a client must be told about.
/// A mutation whose answer carries the state it produced under one key.
///
/// `shell/*` is a local extension, so its shape is this port's to choose and the
/// state travels on the answer rather than through the runtime snapshot.
fn canonical_mutation(key: &str, state: Value, diagnostics: Vec<String>) -> ResourceDispatch {
    let mut result = BTreeMap::from([(key.to_owned(), state)]);
    if !diagnostics.is_empty() {
        result.insert("diagnostics".to_owned(), json!(diagnostics));
    }
    ResourceDispatch {
        result,
        signals: ResourceSignals {
            runtime_updated: true,
            warnings: diagnostics,
            auth_url: None,
            integrations: None,
        },
    }
}

fn runtime_mutation<const N: usize>(
    entries: [(&str, Value); N],
    diagnostics: Vec<String>,
) -> ResourceDispatch {
    ResourceDispatch {
        result: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        signals: ResourceSignals {
            runtime_updated: true,
            warnings: diagnostics,
            auth_url: None,
            integrations: None,
        },
    }
}

fn mcp_view(
    views: Vec<McpServerView>,
    connectors: Vec<ConnectorView>,
    tools: &ToolRegistry,
) -> Value {
    let descriptions = tools
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| (tool.name, tool.description))
        .collect::<BTreeMap<_, _>>();
    let mut discovery_errors = Map::new();
    let mut sources = Vec::with_capacity(views.len().saturating_add(connectors.len()));
    for view in views {
        if let Some(diagnostic) = &view.diagnostic {
            discovery_errors.insert(view.alias.clone(), json!(redact(diagnostic)));
        }
        let disabled_tools = view.disabled_tools;
        let source_available = view.enabled && view.status == McpServerStatus::Healthy;
        sources.push(json!({
            "name": view.alias,
            "kind": McpSourceKind::Server,
            "transport": view.transport,
            "status": server_status(view.enabled, view.status),
            "tools": view.tools.into_iter().map(|name| {
                let enabled = source_available && !disabled_tools.contains(&name);
                let description = descriptions.get(&name).cloned().unwrap_or_default();
                json!({"name": name, "description": description, "enabled": enabled})
            }).collect::<Vec<_>>()
        }));
    }
    for view in connectors {
        if let Some(diagnostic) = &view.diagnostic {
            discovery_errors.insert(view.name.clone(), json!(redact(diagnostic)));
        }
        let disabled_tools = view.disabled_tools;
        let status = connector_status(view.enabled, view.auth_state);
        let available = status == McpSourceStatus::Connected;
        sources.push(json!({
            "name": view.name,
            "kind": McpSourceKind::Connector,
            "transport": CONNECTOR_TRANSPORT,
            "status": status,
            "tools": view.tool_names.into_iter().map(|name| {
                let enabled = available && !disabled_tools.contains(&name);
                let description = descriptions.get(&name).cloned().unwrap_or_default();
                json!({"name": name, "description": description, "enabled": enabled})
            }).collect::<Vec<_>>()
        }));
    }
    json!({"sources": sources, "discoveryErrors": Value::Object(discovery_errors)})
}

/// How a configured MCP server stands, in the vocabulary the wire declares.
///
/// A source the operator switched off is `Disabled` whatever its transport last
/// reported, so a deliberate choice is never rendered as a breakage; a source
/// that failed to start is `Unavailable`, which is the distinction the reference
/// vocabulary keeps and the internal status does not.
fn server_status(enabled: bool, status: McpServerStatus) -> McpSourceStatus {
    if !enabled {
        return McpSourceStatus::Disabled;
    }
    match status {
        McpServerStatus::Healthy => McpSourceStatus::Connected,
        McpServerStatus::AuthRequired => McpSourceStatus::NeedsAuth,
        McpServerStatus::Failed => McpSourceStatus::Unavailable,
        McpServerStatus::Disabled => McpSourceStatus::Disabled,
    }
}

/// How a connector stands, in the same vocabulary.
fn connector_status(enabled: bool, state: ConnectorAuthState) -> McpSourceStatus {
    if !enabled {
        return McpSourceStatus::Disabled;
    }
    match state {
        ConnectorAuthState::Connected | ConnectorAuthState::NotRequired => {
            McpSourceStatus::Connected
        }
        ConnectorAuthState::Disconnected => McpSourceStatus::NeedsAuth,
        ConnectorAuthState::SetupRequired => McpSourceStatus::NeedsSetup,
        ConnectorAuthState::Failed => McpSourceStatus::Unavailable,
    }
}

fn connector_counts_value(views: &[ConnectorView]) -> Value {
    json!({
        "connected": views.iter().filter(|view| {
            view.enabled && matches!(
                view.auth_state,
                vibe_core::integrations::ConnectorAuthState::Connected
                    | vibe_core::integrations::ConnectorAuthState::NotRequired
            )
        }).count(),
        "total": views.len()
    })
}

fn validate_auth_url(url: String, source: &str) -> Result<String, ResourceError> {
    let parsed = Url::parse(&url)
        .map_err(|_| ResourceError::Unavailable(format!("{source} returned an invalid URL")))?;
    if parsed.scheme() == "https"
        || (parsed.scheme() == "http"
            && parsed
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1")))
    {
        Ok(url)
    } else {
        Err(ResourceError::Unavailable(format!(
            "{source} returned an unsafe URL"
        )))
    }
}

fn resolve_connector(
    session: &CoreResourceSession,
    name: &str,
) -> Result<ConnectorView, ResourceError> {
    session
        .connectors
        .views()
        .map_err(integration_error)?
        .into_iter()
        // The alias keeps the case the reference gives it, so an operator
        // naming a connector in lowercase must still reach it.
        .find(|view| {
            [&view.id, &view.alias, &view.name]
                .into_iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name))
        })
        .ok_or_else(|| ResourceError::NotFound(format!("connector `{name}` was not found")))
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

fn invalid_params(error: params::ParamError) -> ResourceError {
    ResourceError::InvalidParams(error.message())
}

fn required_string<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, ResourceError> {
    params::required_string(values, key).map_err(invalid_params)
}

fn optional_string<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ResourceError> {
    params::optional_string(values, key).map_err(invalid_params)
}

fn usize_param(
    values: &BTreeMap<String, Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ResourceError> {
    params::usize_param(values, key, default, min, max).map_err(invalid_params)
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
mod tests {
    use super::*;

    struct FakeMcpAuth {
        url: String,
        calls: StdMutex<Vec<String>>,
    }

    struct FakeConnectorTransport;

    struct CountingEmptyConnectorCatalog {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ConnectorCatalogBackend for CountingEmptyConnectorCatalog {
        fn catalog<'a>(&'a self) -> ResourceFuture<'a, ConnectorCatalog> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::AcqRel);
                Ok(ConnectorCatalog {
                    definitions: Vec::new(),
                    connected: BTreeSet::new(),
                })
            })
        }
    }

    impl ConnectorBackend for FakeConnectorTransport {
        fn call<'a>(
            &'a self,
            _connector_id: &'a str,
            _tool: &'a str,
            _arguments: Value,
            _max_response_bytes: usize,
        ) -> vibe_core::integrations::ConnectorFuture<'a> {
            Box::pin(async { Ok(vibe_core::tools::ToolExecutionOutput::text("{}")) })
        }
    }

    struct FakeConnectorAuth {
        url: String,
        connected: bool,
        calls: StdMutex<Vec<String>>,
    }

    impl ConnectorAuthBackend for FakeConnectorAuth {
        fn auth_url<'a>(
            &'a self,
            session_id: &'a str,
            connector_id: &'a str,
        ) -> ResourceFuture<'a, Option<String>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                    .push(format!("auth:{session_id}:{connector_id}"));
                Ok(Some(self.url.clone()))
            })
        }

        fn refresh<'a>(
            &'a self,
            session_id: &'a str,
            connector_id: &'a str,
        ) -> ResourceFuture<'a, bool> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                    .push(format!("refresh:{session_id}:{connector_id}"));
                Ok(self.connected)
            })
        }
    }

    impl McpAuthBackend for FakeMcpAuth {
        fn login<'a>(
            &'a self,
            session_id: &'a str,
            config: &'a McpServerConfig,
        ) -> ResourceFuture<'a, String> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                    .push(format!("login:{session_id}:{}", config.alias));
                Ok(self.url.clone())
            })
        }

        fn complete<'a>(
            &'a self,
            session_id: &'a str,
            config: &'a McpServerConfig,
        ) -> ResourceFuture<'a, bool> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                    .push(format!("complete:{session_id}:{}", config.alias));
                Ok(true)
            })
        }

        fn logout<'a>(
            &'a self,
            session_id: &'a str,
            config: &'a McpServerConfig,
        ) -> ResourceFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                    .push(format!("logout:{session_id}:{}", config.alias));
                Ok(())
            })
        }

        fn close_session<'a>(&'a self, session_id: &'a str) -> ResourceFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ResourceError::Unavailable("auth test lock".to_owned()))?
                    .push(format!("close:{session_id}"));
                Ok(())
            })
        }
    }

    fn params(value: Value) -> BTreeMap<String, Value> {
        value
            .as_object()
            .expect("object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn backend_request(
        session_id: &str,
        method: &str,
        params: BTreeMap<String, Value>,
    ) -> ResourceBackendRequest {
        ResourceBackendRequest::parse(session_id.to_owned(), method, &params, false)
            .expect("valid backend request")
    }

    fn disabled_mcp(alias: &str) -> McpServerConfig {
        McpServerConfig {
            alias: alias.to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: Url::parse("https://mcp.example/rpc").expect("MCP URL"),
                headers: BTreeMap::new(),
            },
            enabled: false,
            disabled_tools: Default::default(),
            startup_timeout_ms: vibe_core::mcp::DEFAULT_MCP_STARTUP_TIMEOUT_MS,
            tool_timeout_ms: vibe_core::mcp::DEFAULT_MCP_TOOL_TIMEOUT_MS,
            auth: Default::default(),
            prompt: None,
            sampling_enabled: true,
        }
    }

    fn oauth_connector() -> ConnectorDefinition {
        ConnectorDefinition {
            id: "drive-id".to_owned(),
            name: "Drive".to_owned(),
            base_url: Url::parse("https://connectors.example/drive").expect("connector URL"),
            auth_kind: vibe_core::integrations::ConnectorAuthKind::OAuth,
            tools: vec![vibe_core::integrations::ConnectorTool {
                name: "search".to_owned(),
                description: "Search files".to_owned(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
            }],
        }
    }

    #[test]
    fn trust_mutation_returns_a_canonical_notification_after_the_response() {
        let mut resources = ResourceService::default();
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = PermissionStore::default();
        resources
            .open_session("s1", policy.clone(), ToolRegistry::default())
            .expect("session");
        let dispatch = resources
            .dispatch(
                "workspace/trust/decision",
                &params(json!({
                    "sessionId": "s1",
                    "cwd": workspace.path(),
                    "decision": "trust_cwd"
                })),
                false,
            )
            .expect("trust decision");
        // The decision moved runtime state, which the server publishes as
        // `runtime/updated` rather than a name only this port ever spoke.
        assert!(dispatch.signals.runtime_updated);
        assert!(dispatch.signals.warnings.is_empty());
        assert!(dispatch.result.is_empty());
        assert_eq!(
            policy
                .try_trust_decision(workspace.path())
                .expect("canonical policy"),
            Some(TrustDecision::SessionTrusted)
        );
    }

    /// The stub this service used to answer the review surface from is gone,
    /// and with it the only reason the service knew what a review was. The
    /// methods are routed at the server against the session's engine now, so
    /// this service has to refuse them rather than answer an empty panel.
    #[test]
    fn the_resource_service_no_longer_answers_the_review_surface() {
        let mut resources = ResourceService::default();
        for method in [
            "review/approve",
            "review/baseline",
            "review/hunks",
            "review/revert",
            "review/state",
            "review/turnDiff",
        ] {
            let error = resources
                .dispatch(method, &params(json!({"sessionId": "s1"})), false)
                .expect_err("the service holds no review state");
            assert!(
                matches!(error, ResourceError::MethodNotFound(_)),
                "{method} must not be answered here: {error}"
            );
        }
    }

    #[test]
    fn diagnostics_and_logs_redact_sensitive_text() {
        let mut resources = ResourceService::default();
        resources.record_diagnostic("config.toml", "Authorization: Bearer secret");
        resources.record_log(1, "ERROR", "token=secret");
        let diagnostics = resources
            .dispatch("diagnostics/list", &BTreeMap::new(), false)
            .expect("diagnostics");
        let logs = resources
            .dispatch(
                "diagnostics/logs/read",
                &params(json!({"limit": 10, "offset": 0})),
                false,
            )
            .expect("logs");
        assert_eq!(
            diagnostics.result["issues"][0]["message"],
            "[redacted sensitive error]"
        );
        assert_eq!(
            logs.result["logs"]["entries"][0]["message"],
            "[redacted sensitive error]"
        );
    }

    /// US-107: a permanent approval the configuration file refused is kept for
    /// the session rather than failing the call, so the reason has to reach the
    /// operator. `diagnostics/list` and the runtime snapshot are where they
    /// read one, and the session that could not write is the session that
    /// reports it.
    #[tokio::test]
    async fn a_permanent_approval_that_could_not_be_written_is_reported() {
        let mut resources = ResourceService::default();
        let store = PermissionStore::default().with_allowlist_persistence(Arc::new(
            |_tool: &str, _patterns: &[String]| {
                Err("the configuration file is read-only".to_owned())
            },
        ));
        resources
            .open_session("session-1", store.clone(), ToolRegistry::default())
            .expect("the session opens");

        store
            .authorize(
                "bash",
                json!({"command": "cargo test"}),
                vibe_core::policy::PermissionContext::asking(vec![
                    vibe_core::policy::PermissionRequirement::command("cargo test"),
                ]),
                &PermanentApproval,
            )
            .await
            .expect("the call the operator approved still runs");

        let reported = |dispatch: &ResourceDispatch| {
            dispatch.result["issues"]
                .as_array()
                .expect("issues is a list")
                .iter()
                .filter_map(|issue| issue["message"].as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        };
        let diagnostics = resources
            .dispatch("diagnostics/list", &BTreeMap::new(), true)
            .expect("diagnostics");
        let listed = reported(&diagnostics);
        assert!(
            listed
                .iter()
                .any(|message| message.contains("bash") && message.contains("read-only")),
            "{listed:?}"
        );
        assert_eq!(
            diagnostics.result["issues"][0]["file"],
            json!(crate::server::CONFIG_FILE_LABEL)
        );

        let runtime = resources.runtime("session-1").expect("runtime");
        let issues = runtime["issues"].as_array().expect("issues is a list");
        assert!(
            issues.iter().any(|issue| {
                issue["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("read-only"))
            }),
            "the runtime snapshot names it too: {issues:?}"
        );

        // Another session's failure is not this session's problem.
        resources
            .open_session(
                "session-2",
                PermissionStore::default(),
                ToolRegistry::default(),
            )
            .expect("the second session opens");
        let other = resources.runtime("session-2").expect("runtime");
        assert!(
            other["issues"]
                .as_array()
                .expect("issues is a list")
                .is_empty(),
            "{:?}",
            other["issues"]
        );
    }

    struct PermanentApproval;

    impl vibe_core::policy::ApprovalAgent for PermanentApproval {
        fn request<'a>(
            &'a self,
            _request: vibe_core::policy::ApprovalRequest,
        ) -> vibe_core::policy::ApprovalFuture<'a> {
            Box::pin(async move { Ok(vibe_core::policy::ApprovalDecision::ApprovePermanently) })
        }
    }

    #[test]
    fn session_scoped_resource_state_is_bounded_and_released() {
        let mut resources = ResourceService::default();
        for index in 0..MAX_RESOURCE_SESSIONS {
            resources
                .open_session(
                    &format!("session-{index}"),
                    PermissionStore::default(),
                    ToolRegistry::default(),
                )
                .expect("within capacity");
        }
        assert!(matches!(
            resources.open_session(
                "overflow",
                PermissionStore::default(),
                ToolRegistry::default()
            ),
            Err(ResourceError::Conflict(_))
        ));

        resources.close_session("session-0");

        resources
            .open_session(
                "replacement",
                PermissionStore::default(),
                ToolRegistry::default(),
            )
            .expect("released capacity");
    }

    #[test]
    fn mcp_rejects_non_https_endpoints() {
        let mut resources = ResourceService::default();
        let error = resources
            .dispatch(
                "mcp/add",
                &params(json!({"url": "http://mcp.example"})),
                false,
            )
            .expect_err("insecure URL");
        assert!(matches!(error, ResourceError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn mcp_oauth_routes_the_exact_source_and_rejects_unsafe_urls() {
        let auth = Arc::new(FakeMcpAuth {
            url: "https://auth.example/authorize?state=opaque".to_owned(),
            calls: StdMutex::new(Vec::new()),
        });
        let backend = CoreResourceBackend::default().with_mcp_auth(auth.clone());
        backend
            .open_session(ResourceSession {
                session_id: "s1".to_owned(),
                generation: 1,
                working_directory: "/workspace".to_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("open session");
        backend
            .configure_mcp("s1", vec![disabled_mcp("source-a")])
            .await
            .expect("configure source");
        let login = backend
            .dispatch(backend_request(
                "s1",
                "mcp/login",
                params(json!({"name": "source-a"})),
            ))
            .await
            .expect("OAuth URL");
        // The URL is not on the answer: it crosses as `mcp/authUrl`, which is
        // where a reference client reads it, and the answer declares only the
        // runtime the server fills in.
        assert!(!login.result.contains_key("auth"));
        assert_eq!(
            login.signals.auth_url,
            Some(McpAuthUrl {
                name: "source-a".to_owned(),
                url: "https://auth.example/authorize?state=opaque".to_owned(),
            })
        );
        let completion = backend
            .dispatch(backend_request(
                "s1",
                "mcp/auth/complete",
                params(json!({"name": "source-a"})),
            ))
            .await
            .expect("OAuth completion is checked");
        assert_eq!(completion.result["auth"]["verified"], true);
        backend
            .dispatch(backend_request(
                "s1",
                "mcp/logout",
                params(json!({"name": "source-a"})),
            ))
            .await
            .expect("logout");
        assert_eq!(
            auth.calls.lock().expect("calls").as_slice(),
            [
                "login:s1:source-a",
                "complete:s1:source-a",
                "logout:s1:source-a"
            ]
        );
        assert!(matches!(
            backend
                .dispatch(backend_request(
                    "s1",
                    "mcp/login",
                    params(json!({"name": "unknown"})),
                ))
                .await,
            Err(ResourceError::NotFound(_))
        ));
        backend
            .close_session("s1", 1)
            .await
            .expect("OAuth session cleanup");
        assert_eq!(
            auth.calls.lock().expect("calls").last().map(String::as_str),
            Some("close:s1")
        );

        let unsafe_backend = CoreResourceBackend::default().with_mcp_auth(Arc::new(FakeMcpAuth {
            url: "http://auth.example/authorize".to_owned(),
            calls: StdMutex::new(Vec::new()),
        }));
        unsafe_backend
            .open_session(ResourceSession {
                session_id: "s2".to_owned(),
                generation: 1,
                working_directory: "/workspace".to_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("open unsafe session");
        unsafe_backend
            .configure_mcp("s2", vec![disabled_mcp("source-b")])
            .await
            .expect("configure unsafe source");
        assert!(matches!(
            unsafe_backend
                .dispatch(backend_request(
                    "s2",
                    "mcp/login",
                    params(json!({"name": "source-b"})),
                ))
                .await,
            Err(ResourceError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn connectors_initialize_lazily_and_route_auth_to_the_exact_source() {
        let auth = Arc::new(FakeConnectorAuth {
            url: "https://connectors.example/authorize?state=opaque".to_owned(),
            connected: true,
            calls: StdMutex::new(Vec::new()),
        });
        let backend = CoreResourceBackend::default()
            .with_connectors(
                vec![oauth_connector()],
                Arc::new(FakeConnectorTransport),
                "credential",
                Url::parse("https://connectors.example").expect("catalog URL"),
            )
            .with_connector_auth(auth.clone());
        backend
            .open_session(ResourceSession {
                session_id: "s1".to_owned(),
                generation: 1,
                working_directory: "/workspace".to_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("open session");

        let listed = backend
            .dispatch(backend_request("s1", "connectors/read", BTreeMap::new()))
            .await
            .expect("connectors initialize");
        // The sources are published in the MCP state; this answer is the counts
        // and nothing else.
        assert_eq!(
            listed.result.keys().map(String::as_str).collect::<Vec<_>>(),
            ["counts"]
        );
        assert_eq!(listed.result["counts"]["total"], json!(1));
        let login = backend
            .dispatch(backend_request(
                "s1",
                "connectors/auth/read",
                params(json!({"name": "drive"})),
            ))
            .await
            .expect("connector auth URL");
        assert_eq!(
            login.result["url"],
            "https://connectors.example/authorize?state=opaque"
        );
        assert_eq!(
            auth.calls.lock().expect("calls").as_slice(),
            ["auth:s1:drive-id"]
        );
        let refreshed = backend
            .dispatch(backend_request(
                "s1",
                "connectors/refresh",
                params(json!({"name": "drive"})),
            ))
            .await
            .expect("connector refresh");
        assert_eq!(
            refreshed
                .result
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["toolCount"],
            "the runtime is filled in by the server, which owns it"
        );
        assert!(refreshed.signals.runtime_updated);
        let connectors = backend
            .dispatch(backend_request("s1", "mcp/read", BTreeMap::new()))
            .await
            .expect("the merged source list");
        assert_eq!(
            connectors.result["mcp"]["sources"][0],
            json!({
                "name": "Drive",
                "kind": "connector",
                "transport": "connector",
                "status": "connected",
                "tools": [{
                    "name": "connector_Drive_search",
                    "description": "Search files",
                    "enabled": true,
                }],
            })
        );
        assert_eq!(
            auth.calls.lock().expect("calls").as_slice(),
            ["auth:s1:drive-id", "refresh:s1:drive-id"]
        );
    }

    #[tokio::test]
    async fn connector_persistence_failure_leaves_runtime_state_unchanged() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let vibe_home = temporary.path().join("home/.vibe");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&vibe_home).expect("config directory");
        std::fs::create_dir_all(&workspace).expect("workspace directory");
        let config_path = vibe_home.join("config.toml");
        let store = LayeredConfig::new(
            vibe_core::config::ConfigPaths {
                vibe_home,
                working_directory: workspace.clone(),
            },
            vibe_core::config::registry::default_document(),
        );
        let backend = CoreResourceBackend::default()
            .with_config(store)
            .with_connectors(
                vec![oauth_connector()],
                Arc::new(FakeConnectorTransport),
                "credential",
                Url::parse("https://connectors.example").expect("catalog URL"),
            );
        backend
            .open_session(ResourceSession {
                session_id: "transaction".to_owned(),
                generation: 1,
                working_directory: workspace.to_string_lossy().into_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("session opens");
        backend
            .dispatch(backend_request(
                "transaction",
                "connectors/read",
                BTreeMap::new(),
            ))
            .await
            .expect("connector initializes");
        std::fs::write(&config_path, "invalid = [").expect("corrupt config fixture");

        assert!(
            backend
                .dispatch(backend_request(
                    "transaction",
                    "connectors/toggle",
                    params(json!({"name": "drive", "disabled": true})),
                ))
                .await
                .is_err()
        );
        let session = backend.session("transaction").expect("session remains");
        let view = session
            .connectors
            .views()
            .expect("connector state")
            .remove(0);
        assert!(view.enabled);
    }

    /// Connector aliases used to be lowercased and are now published in the
    /// case the reference keeps, so a preference persisted by an older build
    /// names `drive` where the session now holds `Drive`. Resolving that entry
    /// is what stops an upgrade from silently re-enabling a connector the
    /// operator disabled.
    #[tokio::test]
    async fn a_preference_persisted_under_the_lowercased_alias_still_applies() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let vibe_home = temporary.path().join("home/.vibe");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&vibe_home).expect("config directory");
        std::fs::create_dir_all(&workspace).expect("workspace directory");
        std::fs::write(
            vibe_home.join("config.toml"),
            "[[connectors]]\nname = \"drive\"\ndisabled = true\ndisabled_tools = [\"search\"]\n",
        )
        .expect("preference written by an older build");
        let store = LayeredConfig::new(
            vibe_core::config::ConfigPaths {
                vibe_home,
                working_directory: workspace.clone(),
            },
            vibe_core::config::registry::default_document(),
        );
        let backend = CoreResourceBackend::default()
            .with_config(store)
            .with_connectors(
                vec![oauth_connector()],
                Arc::new(FakeConnectorTransport),
                "credential",
                Url::parse("https://connectors.example").expect("catalog URL"),
            );
        backend
            .open_session(ResourceSession {
                session_id: "upgraded".to_owned(),
                generation: 1,
                working_directory: workspace.to_string_lossy().into_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("session opens");

        backend
            .dispatch(backend_request(
                "upgraded",
                "connectors/read",
                BTreeMap::new(),
            ))
            .await
            .expect("connector initializes");

        let view = backend
            .session("upgraded")
            .expect("session")
            .connectors
            .views()
            .expect("connector state")
            .remove(0);
        assert_eq!(view.alias, "Drive");
        assert!(
            !view.enabled,
            "the persisted disable must survive the alias case change"
        );
        assert!(view.disabled_tools.contains("connector_Drive_search"));
    }

    #[tokio::test]
    async fn core_backend_denies_stdio_mcp_before_workspace_trust() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = CoreResourceBackend::default();
        backend
            .open_session(ResourceSession {
                session_id: "s1".to_owned(),
                generation: 1,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("open session");
        let error = backend
            .dispatch(backend_request(
                "s1",
                "mcp/add",
                params(json!({
                    "name": "untrusted",
                    "transport": "stdio",
                    "command": "must-not-launch"
                })),
            ))
            .await
            .expect_err("untrusted executable must be denied before spawn");
        assert!(
            matches!(error, ResourceError::Unavailable(message) if message.contains("workspace trust"))
        );
    }

    #[tokio::test]
    async fn core_backend_denies_stdio_mcp_working_directory_outside_trust() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside directory");
        let policy = PermissionStore::default();
        policy
            .try_set_trust(
                workspace.path(),
                TrustDecision::SessionTrusted,
                TrustRootKind::Workspace,
            )
            .expect("trust workspace");
        let backend = CoreResourceBackend::default();
        backend
            .open_session(ResourceSession {
                session_id: "s1".to_owned(),
                generation: 1,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                project_trusted: true,
                policy,
                tools: ToolRegistry::default(),
            })
            .expect("open session");
        let error = backend
            .dispatch(backend_request(
                "s1",
                "mcp/add",
                params(json!({
                    "name": "outside",
                    "transport": "stdio",
                    "command": "must-not-launch",
                    "workingDirectory": outside.path()
                })),
            ))
            .await
            .expect_err("outside working directory must be denied before spawn");
        assert!(
            matches!(error, ResourceError::Unavailable(message) if message.contains("workspace trust"))
        );
    }

    #[tokio::test]
    async fn core_backend_runs_trusted_shell_and_cleans_the_owned_process() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = PermissionStore::default();
        policy
            .try_set_trust(
                workspace.path(),
                TrustDecision::SessionTrusted,
                TrustRootKind::Workspace,
            )
            .expect("trust");
        let backend = CoreResourceBackend::default();
        backend
            .open_session(ResourceSession {
                session_id: "s1".to_owned(),
                generation: 1,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                project_trusted: true,
                policy,
                tools: ToolRegistry::default(),
            })
            .expect("open session");
        let dispatch = backend
            .dispatch(backend_request(
                "s1",
                "shell/run",
                params(json!({
                    "sessionId": "s1",
                    "operationId": "shell-1",
                    "command": "pwd"
                })),
            ))
            .await
            .expect("run shell");
        assert!(dispatch.signals.runtime_updated);
        let (completed, saw_output) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let mut saw_output = false;
                loop {
                    let dispatch = backend
                        .dispatch(backend_request(
                            "s1",
                            "shell/run",
                            params(json!({
                                "sessionId": "s1",
                                "operationId": "shell-1",
                                "command": "pwd"
                            })),
                        ))
                        .await
                        .expect("poll shell");
                    saw_output |= dispatch
                        .result
                        .get("shell")
                        .and_then(|shell| shell.pointer("/output/chunks"))
                        .and_then(Value::as_array)
                        .is_some_and(|chunks| !chunks.is_empty());
                    if dispatch
                        .result
                        .get("shell")
                        .and_then(|shell| shell.pointer("/output/state/status"))
                        .and_then(Value::as_str)
                        != Some("running")
                    {
                        break (dispatch, saw_output);
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("shell completes within the test deadline");
        assert_eq!(
            completed
                .result
                .get("shell")
                .and_then(|shell| shell.pointer("/output/state/status"))
                .and_then(Value::as_str),
            Some("exited")
        );
        assert!(
            saw_output,
            "shell output must be drained before process release"
        );
        let denied = backend
            .dispatch(backend_request(
                "s1",
                "shell/run",
                params(json!({
                    "sessionId": "s1",
                    "operationId": "shell-2",
                    "command": "rm forbidden"
                })),
            ))
            .await
            .expect_err("destructive shell command must be denied before spawn");
        assert!(matches!(denied, ResourceError::Conflict(_)));
        backend.close_session("s1", 1).await.expect("cleanup");
    }

    #[tokio::test]
    async fn stale_close_cannot_remove_a_reattached_resource_generation() {
        let backend = CoreResourceBackend::default();
        let session = |generation| ResourceSession {
            session_id: "s1".to_owned(),
            generation,
            working_directory: "/workspace".to_owned(),
            project_trusted: false,
            policy: PermissionStore::default(),
            tools: ToolRegistry::default(),
        };
        backend.open_session(session(1)).expect("first attachment");
        backend.open_session(session(2)).expect("reattachment");

        backend
            .close_session("s1", 1)
            .await
            .expect("stale cleanup is harmless");
        let dispatch = backend
            .dispatch(backend_request("s1", "mcp/read", BTreeMap::new()))
            .await
            .expect("reattached resources remain available");
        assert!(dispatch.result.contains_key("mcp"));

        backend
            .close_session("s1", 2)
            .await
            .expect("current cleanup");
        assert!(matches!(
            backend
                .dispatch(backend_request("s1", "mcp/read", BTreeMap::new()))
                .await,
            Err(ResourceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn empty_connector_catalog_initializes_once_under_concurrent_reads() {
        let catalog = Arc::new(CountingEmptyConnectorCatalog {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let backend = CoreResourceBackend::default().with_connector_catalog(
            catalog.clone(),
            Arc::new(FakeConnectorTransport),
            "credential",
            Url::parse("https://connectors.example.test").expect("connector URL"),
        );
        backend
            .open_session(ResourceSession {
                session_id: "connector-read".to_owned(),
                generation: 1,
                working_directory: "/workspace".to_owned(),
                project_trusted: false,
                policy: PermissionStore::default(),
                tools: ToolRegistry::default(),
            })
            .expect("open connector session");

        let first = backend.dispatch(backend_request(
            "connector-read",
            "connectors/read",
            BTreeMap::new(),
        ));
        let second = backend.dispatch(backend_request(
            "connector-read",
            "connectors/read",
            BTreeMap::new(),
        ));
        let (first, second) = tokio::join!(first, second);
        first.expect("first connector read");
        second.expect("second connector read");
        assert_eq!(catalog.calls.load(Ordering::Acquire), 1);
    }
}
