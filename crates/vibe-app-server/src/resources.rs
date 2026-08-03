use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;
use crate::host::now_millis;
use crate::params;
use vibe_core::config::{ConfigSnapshot, LayeredConfig};
use vibe_core::integrations::{
    ConnectorAuthKind, ConnectorBackend, ConnectorDefinition, ConnectorRegistry, ConnectorView,
    redact,
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
use vibe_core::shell::{ShellConfig, ShellPolicyContext, analyze_shell};
use vibe_core::tools::ToolRegistry;

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

const MAX_RESOURCE_RECORDS: usize = 1_024;
const MAX_RESOURCE_SESSIONS: usize = 256;
const MAX_FEEDBACK_ACTIONS: usize = 256;
use crate::params::MAX_PARAM_STRING_BYTES as MAX_RESOURCE_STRING_BYTES;

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDispatch {
    pub result: BTreeMap<String, Value>,
    pub notification: Option<ResourceNotification>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceNotification {
    pub method: String,
    pub params: BTreeMap<String, Value>,
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

#[derive(Debug, Clone)]
struct ReviewFile {
    baseline: String,
    current: String,
    approved: bool,
}

#[derive(Clone, Default)]
pub struct ResourceService {
    ready: bool,
    diagnostics: Vec<(String, String)>,
    logs: Vec<(u64, String, String)>,
    feedback_actions: Vec<String>,
    connectors: BTreeMap<String, bool>,
    mcp: BTreeMap<String, McpSource>,
    review: BTreeMap<String, BTreeMap<String, ReviewFile>>,
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
            "account/read" => Ok(read_only([(
                "account",
                json!({
                    "status": "missing_key",
                    "plan": null,
                    "planOffer": null,
                    "rateLimitAction": null,
                    "teleportEligible": false,
                    "teleportAction": null
                }),
            )])),
            "diagnostics/list" => Ok(read_only([
                (
                    "issues",
                    Value::Array(
                        self.diagnostics
                            .iter()
                            .map(
                                |(file, message)| json!({"file": file, "message": redact(message)}),
                            )
                            .collect(),
                    ),
                ),
                ("hooksCount", json!(0)),
            ])),
            "diagnostics/logs/read" => self.logs_read(params),
            "feedback/record" => self.feedback_record(params),
            "feedback/shouldShow" => Ok(read_only([(
                "show",
                json!(self.feedback_actions.is_empty()),
            )])),
            "narration/summarize" => self.narration(params),
            "review/approve" => self.review_mutate(params, session_active, true),
            "review/revert" => self.review_mutate(params, session_active, false),
            "review/state" => {
                let session_id = required_string(params, "sessionId")?;
                Ok(read_only([
                    ("files", self.review_files(session_id)),
                    ("scopes", json!([])),
                ]))
            }
            "review/baseline" => self.review_baseline(params),
            "review/hunks" => self.review_hunks(params),
            "review/turnDiff" => self.review_turn_diff(params),
            "runtime/read" => Ok(read_only([
                (
                    "runtime",
                    self.runtime(required_string(params, "sessionId")?)?,
                ),
                (
                    "sessionLog",
                    json!({
                        "enabled": false,
                        "sessionId": null,
                        "persisted": false,
                        "path": null,
                        "title": null,
                        "needsInitialAutoTitle": false
                    }),
                ),
                ("ready", json!(self.ready)),
            ])),
            "session/ready/read" | "session/ready/wait" => {
                Ok(read_only([("ready", json!(self.ready))]))
            }
            "stats/read" => Ok(read_only([
                ("stats", empty_stats()),
                ("contextWindow", json!(0)),
            ])),
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
            ResourceBackendCommand::Connector(ConnectorCommand::Read) => Ok(read_only([
                ("counts", self.connector_counts()),
                ("connectors", self.connector_state()),
            ])),
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
                    add.alias
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

    pub fn record_review_change(
        &mut self,
        session_id: &str,
        path: &str,
        baseline: &str,
        current: &str,
    ) {
        if !self.review.contains_key(session_id)
            && self.review.len() >= MAX_RESOURCE_SESSIONS
            && let Some(oldest) = self.review.keys().next().cloned()
        {
            self.review.remove(&oldest);
        }
        let files = self.review.entry(session_id.to_owned()).or_default();
        if files.len() == MAX_RESOURCE_RECORDS
            && let Some(oldest) = files.keys().next().cloned()
        {
            files.remove(&oldest);
        }
        files.insert(
            bounded(path),
            ReviewFile {
                baseline: bounded_resource_string(baseline),
                current: bounded_resource_string(current),
                approved: false,
            },
        );
    }

    pub fn close_session(&mut self, session_id: &str) {
        self.review.remove(session_id);
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

    fn connector_counts(&self) -> Value {
        json!({
            "connected": self.connectors.values().filter(|connected| **connected).count(),
            "total": self.connectors.len()
        })
    }

    fn connector_state(&self) -> Value {
        json!({
            "sources": self.connectors.iter().map(|(name, connected)| {
                json!({
                    "id": name,
                    "alias": name,
                    "name": name,
                    "kind": "connector",
                    "transport": "https",
                    "authState": if *connected { "connected" } else { "disconnected" },
                    "toolNames": [],
                })
            }).collect::<Vec<_>>()
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

    fn review_mutate(
        &mut self,
        params: &BTreeMap<String, Value>,
        session_active: bool,
        approve: bool,
    ) -> Result<ResourceDispatch, ResourceError> {
        if session_active {
            return Err(ResourceError::Conflict(
                "review mutations require an idle session".to_owned(),
            ));
        }
        let session_id = required_string(params, "sessionId")?;
        let review = self.review.get_mut(session_id).ok_or_else(|| {
            ResourceError::NotFound(format!(
                "review state for session `{session_id}` was not found"
            ))
        })?;
        let target = required_object(params, "target")?;
        let kind = target
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| ResourceError::InvalidParams("target.kind is required".to_owned()))?;
        match kind {
            "all" | "lastTurns" | "scope" => {
                for file in review.values_mut() {
                    if approve {
                        file.approved = true;
                    } else {
                        file.current.clone_from(&file.baseline);
                        file.approved = false;
                    }
                }
            }
            "file" | "scopeFile" | "region" | "regions" => {
                let path = target.get("path").and_then(Value::as_str).ok_or_else(|| {
                    ResourceError::InvalidParams("target.path is required".to_owned())
                })?;
                let file = review.get_mut(path).ok_or_else(|| {
                    ResourceError::NotFound(format!("review file `{path}` was not found"))
                })?;
                if approve {
                    file.approved = true;
                } else {
                    file.current.clone_from(&file.baseline);
                    file.approved = false;
                }
            }
            _ => {
                return Err(ResourceError::InvalidParams(
                    "unsupported review target".to_owned(),
                ));
            }
        }
        Ok(read_only([]))
    }

    fn review_baseline(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let path = required_string(params, "path")?;
        let session_id = required_string(params, "sessionId")?;
        let file = self
            .review
            .get(session_id)
            .and_then(|files| files.get(path))
            .ok_or_else(|| {
                ResourceError::NotFound(format!("review file `{path}` was not found"))
            })?;
        Ok(read_only([("content", json!(file.baseline))]))
    }

    fn review_hunks(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let path = required_string(params, "path")?;
        let session_id = required_string(params, "sessionId")?;
        let file = self
            .review
            .get(session_id)
            .and_then(|files| files.get(path))
            .ok_or_else(|| {
                ResourceError::NotFound(format!("review file `{path}` was not found"))
            })?;
        let hunks = if file.baseline == file.current {
            json!([])
        } else {
            json!([{
                "side": "additions",
                "line": 0,
                "regions": [{"versionIndex": 0, "ordinal": 0}]
            }])
        };
        Ok(read_only([("hunks", hunks)]))
    }

    fn review_turn_diff(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let path = required_string(params, "path")?;
        let session_id = required_string(params, "sessionId")?;
        let file = self
            .review
            .get(session_id)
            .and_then(|files| files.get(path))
            .ok_or_else(|| {
                ResourceError::NotFound(format!("review file `{path}` was not found"))
            })?;
        Ok(read_only([
            (
                "status",
                json!(if file.baseline.is_empty() {
                    "created"
                } else if file.current.is_empty() {
                    "deleted"
                } else {
                    "modified"
                }),
            ),
            ("baseline", json!(file.baseline)),
            ("current", json!(file.current)),
        ]))
    }

    fn review_files(&self, session_id: &str) -> Value {
        Value::Array(
            self.review
                .get(session_id)
                .into_iter()
                .flat_map(|files| files.iter())
                .filter(|(_, file)| !file.approved && file.baseline != file.current)
                .map(|(path, file)| {
                    json!({
                        "path": path,
                        "status": if file.baseline.is_empty() { "created" } else if file.current.is_empty() { "deleted" } else { "modified" },
                        "regions": [{
                            "kind": "text",
                            "versionIndex": 0,
                            "ordinal": 0,
                            "owner": {"kind": "agent", "turnId": 0},
                            "baselineStart": 0,
                            "baselineLineCount": file.baseline.lines().count(),
                            "currentStart": 0,
                            "currentLineCount": file.current.lines().count(),
                            "decision": "pending",
                            "dependsOn": []
                        }]
                    })
                })
                .collect(),
        )
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
        let (status, trust, kind) = match decision {
            "trust_repo" => ("trusted", TrustDecision::Trusted, TrustRootKind::Workspace),
            "trust_cwd" => (
                "session",
                TrustDecision::SessionTrusted,
                TrustRootKind::Workspace,
            ),
            "decline" => (
                "untrusted",
                TrustDecision::Untrusted,
                TrustRootKind::Workspace,
            ),
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
            notification: Some(ResourceNotification {
                method: "workspace/trust/updated".to_owned(),
                params: [
                    ("cwd".to_owned(), json!(cwd)),
                    ("status".to_owned(), json!(status)),
                ]
                .into_iter()
                .collect(),
            }),
        })
    }

    fn tool_list(&self, session_id: &str) -> Result<Value, ResourceError> {
        let registry = self.tool_registries.get(session_id).ok_or_else(|| {
            ResourceError::NotFound(format!("session `{session_id}` was not found"))
        })?;
        let tools = registry
            .list()
            .map_err(|error| ResourceError::Unavailable(error.to_string()))?;
        serde_json::to_value(tools).map_err(|error| ResourceError::Unavailable(error.to_string()))
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

    fn runtime(&self, session_id: &str) -> Result<Value, ResourceError> {
        Ok(json!({
            "config": empty_config(),
            "baseConfig": empty_config(),
            "activeAgent": {
                "name": "default",
                "displayName": "Default",
                "description": "",
                "safety": "neutral",
                "agentType": "agent"
            },
            "agents": [],
            "skills": [],
            "tools": self.tool_list(session_id)?,
            "stats": empty_stats(),
            "contextWindow": 0,
            "issues": self.diagnostics.iter().map(|(file, message)| {
                json!({"file": file, "message": redact(message)})
            }).collect::<Vec<_>>(),
            "hooksCount": 0,
            "connectors": self.connector_counts(),
            "mcp": self.mcp_state()
        }))
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
    let exceeded =
        || ResourceError::Unavailable(format!("{label} exceeded its byte budget"));
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
        notification: None,
    }
}

fn empty_stats() -> Value {
    json!({
        "steps": 0,
        "sessionPromptTokens": 0,
        "sessionCompletionTokens": 0,
        "inputPricePerMillion": 0.0,
        "outputPricePerMillion": 0.0,
        "toolCallsAgreed": 0,
        "toolCallsRejected": 0,
        "toolCallsFailed": 0,
        "toolCallsSucceeded": 0,
        "contextTokens": 0,
        "lastTurnPromptTokens": 0,
        "lastTurnCompletionTokens": 0,
        "lastTurnDuration": 0.0,
        "tokensPerSecond": 0.0
    })
}

fn empty_config() -> Value {
    json!({
        "activeModel": {
            "name": "",
            "alias": "",
            "thinking": "off",
            "supportsImages": false
        },
        "theme": "default",
        "disableWelcomeBannerAnimation": false,
        "autocopyToClipboard": false,
        "fileWatcherForAutocomplete": false,
        "askConfirmationOnExit": true,
        "voiceModeEnabled": false,
        "narratorEnabled": false,
        "showThinkingNodes": true,
        "enableUpdateChecks": true,
        "enableNotifications": false,
        "vibeCodeEnabled": false,
        "models": [],
        "transcription": {
            "model": {"name": "", "sampleRate": 0, "encoding": "pcm_s16le", "language": "", "targetStreamingDelayMs": 0},
            "provider": {"apiBase": "", "apiKeyEnvVar": "", "client": "mistral"}
        },
        "speech": {
            "model": {"name": "", "voice": "", "responseFormat": "pcm"},
            "provider": {"apiBase": "", "apiKeyEnvVar": "", "client": "mistral"}
        },
        "validationWarnings": []
    })
}

fn canonical_mutation(
    key: &str,
    state: Value,
    notification_method: &str,
    diagnostics: Vec<String>,
) -> ResourceDispatch {
    let mut result = BTreeMap::from([(key.to_owned(), state.clone())]);
    if !diagnostics.is_empty() {
        result.insert("diagnostics".to_owned(), json!(diagnostics));
    }
    ResourceDispatch {
        result,
        notification: Some(ResourceNotification {
            method: notification_method.to_owned(),
            params: BTreeMap::from([(key.to_owned(), state)]),
        }),
    }
}

fn mcp_view(views: Vec<McpServerView>, tools: &ToolRegistry) -> Value {
    let descriptions = tools
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| (tool.name, tool.description))
        .collect::<BTreeMap<_, _>>();
    json!({
        "sources": views.into_iter().map(|view| {
            let disabled_tools = view.disabled_tools;
            let source_available = view.enabled
                && view.status == vibe_core::mcp::McpServerStatus::Healthy;
            json!({
                "name": view.alias,
                "kind": "server",
                "transport": view.transport,
                "status": view.status,
                "enabled": view.enabled,
                "diagnostic": view.diagnostic,
                "tools": view.tools.into_iter().map(|name| {
                    let enabled = source_available && !disabled_tools.contains(&name);
                    let description = descriptions.get(&name).cloned().unwrap_or_default();
                    json!({"name": name, "description": description, "enabled": enabled})
                }).collect::<Vec<_>>()
            })
        }).collect::<Vec<_>>(),
        "discoveryErrors": {}
    })
}

fn connector_view(views: Vec<ConnectorView>) -> Value {
    json!({
        "sources": views
    })
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
        .find(|view| view.id == name || view.alias == name || view.name == name)
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

fn required_object<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, ResourceError> {
    params::required_object(values, key).map_err(invalid_params)
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

fn bounded_resource_string(value: &str) -> String {
    let mut end = value.len().min(MAX_RESOURCE_STRING_BYTES);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
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
        assert_eq!(
            dispatch
                .notification
                .as_ref()
                .map(|item| item.method.as_str()),
            Some("workspace/trust/updated")
        );
        assert!(dispatch.result.is_empty());
        assert_eq!(
            dispatch.notification.expect("notification").params["status"],
            "session"
        );
        assert_eq!(
            policy
                .try_trust_decision(workspace.path())
                .expect("canonical policy"),
            Some(TrustDecision::SessionTrusted)
        );
    }

    #[test]
    fn review_mutation_rejects_an_active_turn_without_state_change() {
        let mut resources = ResourceService::default();
        resources.record_review_change("s1", "src/lib.rs", "old", "new");
        let session = params(json!({"sessionId": "s1"}));
        let before = resources
            .dispatch("review/state", &session, false)
            .expect("state");
        let error = resources
            .dispatch(
                "review/revert",
                &params(json!({"sessionId": "s1", "target": {"kind": "all"}})),
                true,
            )
            .expect_err("active turn rejected");
        assert!(matches!(error, ResourceError::Conflict(_)));
        assert_eq!(
            resources
                .dispatch("review/state", &session, false)
                .expect("state"),
            before
        );
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
        assert_eq!(
            login.result["auth"]["url"],
            "https://auth.example/authorize?state=opaque"
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
        assert_eq!(listed.result["connectors"]["sources"][0]["id"], "drive-id");
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
            refreshed.result["connectors"]["sources"][0]["authState"],
            "connected"
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
            toml::Table::new(),
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
        assert_eq!(
            dispatch
                .notification
                .as_ref()
                .map(|notification| notification.method.as_str()),
            Some("shell/updated")
        );
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
