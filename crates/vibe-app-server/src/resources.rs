use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;
use vibe_core::integrations::{
    ConnectorBackend, ConnectorDefinition, ConnectorRegistry, ConnectorView, redact,
};
use vibe_core::mcp::{
    McpPeerFactory, McpRegistry, McpServerConfig, McpServerView, McpTransportConfig,
};
use vibe_core::platform::{Platform, parse_policy_path};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode,
    PermissionStore, TrustDecision, TrustRootKind,
};
use vibe_core::process::{ProcessSpec, TerminalManager};
use vibe_core::shell::{ShellConfig, ShellPolicyContext, analyze_shell};
use vibe_core::tools::ToolRegistry;

pub const RESOURCE_METHODS: &[&str] = &[
    "account/read",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "mcp/add",
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
    "mcp/add",
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
const MAX_RESOURCE_STRING_BYTES: usize = 65_536;

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
    pub policy: PermissionStore,
    pub tools: ToolRegistry,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceBackendRequest {
    pub session_id: String,
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

pub type ResourceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ResourceError>> + Send + 'a>>;

pub trait ResourceBackend: Send + Sync {
    fn open_session(&self, session: ResourceSession) -> Result<(), ResourceError>;

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

struct BackendDenyApproval;

impl ApprovalAgent for BackendDenyApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

struct CoreResourceSession {
    working_directory: String,
    policy: PermissionStore,
    tools: ToolRegistry,
    mcp: McpRegistry,
    connectors: ConnectorRegistry,
    terminals: TerminalManager,
    shell_operations: Mutex<BTreeMap<String, String>>,
}

struct CoreResourceEntry {
    generation: u64,
    session: Arc<CoreResourceSession>,
}

#[derive(Clone)]
pub struct CoreResourceBackend {
    sessions: Arc<StdMutex<BTreeMap<String, CoreResourceEntry>>>,
    mcp_factory: Option<Arc<dyn McpPeerFactory>>,
    connector_definitions: Arc<Vec<ConnectorDefinition>>,
    connector_backend: Option<Arc<dyn ConnectorBackend>>,
    connector_base_url: Option<Url>,
    connector_credential_reference: Arc<str>,
    approval: Arc<dyn ApprovalAgent>,
}

impl Default for CoreResourceBackend {
    fn default() -> Self {
        Self {
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            mcp_factory: None,
            connector_definitions: Arc::new(Vec::new()),
            connector_backend: None,
            connector_base_url: None,
            connector_credential_reference: Arc::from("unconfigured"),
            approval: Arc::new(BackendDenyApproval),
        }
    }
}

impl CoreResourceBackend {
    #[must_use]
    pub fn with_mcp_factory(mut self, factory: Arc<dyn McpPeerFactory>) -> Self {
        self.mcp_factory = Some(factory);
        self
    }

    #[must_use]
    pub fn with_connectors(
        mut self,
        definitions: Vec<ConnectorDefinition>,
        backend: Arc<dyn ConnectorBackend>,
        credential_reference: impl Into<Arc<str>>,
        base_url: Url,
    ) -> Self {
        self.connector_definitions = Arc::new(definitions);
        self.connector_backend = Some(backend);
        self.connector_credential_reference = credential_reference.into();
        self.connector_base_url = Some(base_url);
        self
    }

    #[must_use]
    pub fn with_approval(mut self, approval: Arc<dyn ApprovalAgent>) -> Self {
        self.approval = approval;
        self
    }

    fn session(&self, session_id: &str) -> Result<Arc<CoreResourceSession>, ResourceError> {
        self.sessions
            .lock()
            .map_err(|_| {
                ResourceError::Unavailable("resource backend lock is poisoned".to_owned())
            })?
            .get(session_id)
            .map(|entry| Arc::clone(&entry.session))
            .ok_or_else(|| ResourceError::NotFound(format!("session `{session_id}` was not found")))
    }

    async fn dispatch_mcp(
        &self,
        session: &CoreResourceSession,
        request: &ResourceBackendRequest,
    ) -> Result<ResourceDispatch, ResourceError> {
        match request.method.as_str() {
            "mcp/read" => Ok(read_only([("mcp", mcp_view(session.mcp.read().await))])),
            "mcp/add" => {
                let factory = self.mcp_factory.clone().ok_or_else(|| {
                    ResourceError::Unavailable("MCP transport backend is not configured".to_owned())
                })?;
                let raw_url = required_string(&request.params, "url")?;
                let url = Url::parse(raw_url)
                    .map_err(|error| ResourceError::InvalidParams(error.to_string()))?;
                let alias = optional_string(&request.params, "name")?
                    .map(str::to_owned)
                    .unwrap_or_else(|| mcp_alias(&url));
                let transport = match optional_string(&request.params, "transport")?
                    .unwrap_or("streamable-http")
                {
                    "http" | "sse" => McpTransportConfig::Http {
                        url,
                        headers: BTreeMap::new(),
                    },
                    "streamable-http" => McpTransportConfig::StreamableHttp {
                        url,
                        headers: BTreeMap::new(),
                    },
                    value => {
                        return Err(ResourceError::InvalidParams(format!(
                            "unsupported MCP transport `{value}`"
                        )));
                    }
                };
                let diagnostics = session
                    .mcp
                    .discover_all(
                        vec![McpServerConfig {
                            alias,
                            transport,
                            enabled: true,
                            oauth: None,
                        }],
                        factory,
                        &session.tools,
                        session.policy.clone(),
                        self.approval.clone(),
                    )
                    .await;
                let state = mcp_view(session.mcp.read().await);
                Ok(canonical_mutation("mcp", state, "mcp/updated", diagnostics))
            }
            "mcp/refresh" => {
                let name = required_string(&request.params, "name")?;
                session
                    .mcp
                    .refresh(name)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let state = mcp_view(session.mcp.read().await);
                Ok(canonical_mutation("mcp", state, "mcp/updated", Vec::new()))
            }
            "mcp/toggle" => {
                if optional_string(&request.params, "toolName")?.is_some() {
                    return Err(ResourceError::Unavailable(
                        "per-tool MCP toggles are not supported by the configured backend"
                            .to_owned(),
                    ));
                }
                let name = required_string(&request.params, "name")?;
                let disabled = required_bool(&request.params, "disabled")?;
                session
                    .mcp
                    .toggle(name, !disabled)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let state = mcp_view(session.mcp.read().await);
                Ok(canonical_mutation("mcp", state, "mcp/updated", Vec::new()))
            }
            "mcp/login" | "mcp/logout" => Err(ResourceError::Unavailable(
                "MCP OAuth interaction backend is not configured".to_owned(),
            )),
            _ => Err(ResourceError::MethodNotFound(request.method.clone())),
        }
    }

    async fn dispatch_connectors(
        &self,
        session: &CoreResourceSession,
        request: &ResourceBackendRequest,
    ) -> Result<ResourceDispatch, ResourceError> {
        match request.method.as_str() {
            "connectors/read" => Ok(read_only([(
                "counts",
                connector_counts_value(&session.connectors.views().map_err(integration_error)?),
            )])),
            "connectors/auth/read" => {
                let name = required_string(&request.params, "name")?;
                let view = session
                    .connectors
                    .views()
                    .map_err(integration_error)?
                    .into_iter()
                    .find(|view| view.id == name || view.alias == name)
                    .ok_or_else(|| {
                        ResourceError::NotFound(format!("connector `{name}` was not found"))
                    })?;
                Ok(read_only([(
                    "url",
                    json!(format!("https://connectors.mistral.ai/auth/{}", view.id)),
                )]))
            }
            "connectors/refresh" => {
                let name = required_string(&request.params, "name")?;
                let definitions = self.connector_definitions.as_ref().clone();
                if !definitions
                    .iter()
                    .any(|definition| definition.id == name || definition.name == name)
                {
                    return Err(ResourceError::NotFound(format!(
                        "connector `{name}` was not found"
                    )));
                }
                session
                    .connectors
                    .discover(
                        definitions,
                        &self.connector_credential_reference,
                        self.connector_base_url.as_ref().ok_or_else(|| {
                            ResourceError::Unavailable(
                                "connector catalog backend is not configured".to_owned(),
                            )
                        })?,
                        now_millis(),
                    )
                    .await
                    .map_err(integration_error)?;
                let backend = self.connector_backend.clone().ok_or_else(|| {
                    ResourceError::Unavailable(
                        "connector transport backend is not configured".to_owned(),
                    )
                })?;
                session
                    .connectors
                    .register_tools(
                        &session.tools,
                        backend,
                        session.policy.clone(),
                        self.approval.clone(),
                    )
                    .map_err(integration_error)?;
                let state = connector_view(session.connectors.views().map_err(integration_error)?);
                Ok(canonical_mutation(
                    "connectors",
                    state,
                    "connectors/updated",
                    Vec::new(),
                ))
            }
            _ => Err(ResourceError::MethodNotFound(request.method.clone())),
        }
    }

    async fn dispatch_shell(
        &self,
        session: &CoreResourceSession,
        request: &ResourceBackendRequest,
    ) -> Result<ResourceDispatch, ResourceError> {
        let operation_id = required_string(&request.params, "operationId")?.to_owned();
        match request.method.as_str() {
            "shell/run" => {
                let command = required_string(&request.params, "command")?;
                if !matches!(
                    session
                        .policy
                        .try_trust_decision(&session.working_directory)
                        .map_err(policy_error)?,
                    Some(TrustDecision::Trusted | TrustDecision::SessionTrusted)
                ) {
                    return Err(ResourceError::Conflict(
                        "manual shell requires a trusted workspace".to_owned(),
                    ));
                }
                let platform = host_platform();
                let working_directory = parse_policy_path(platform, &session.working_directory)
                    .map_err(|error| ResourceError::InvalidParams(error.to_string()))?;
                let analysis = analyze_shell(
                    ShellConfig::default_for(platform).flavor,
                    command,
                    &ShellPolicyContext {
                        platform,
                        working_directory: working_directory.clone(),
                        roots: vec![working_directory],
                    },
                );
                if analysis.mode != PermissionMode::Always {
                    return Err(ResourceError::Conflict(format!(
                        "shell operation `{operation_id}` requires explicit approval: {}",
                        analysis.rationale.join("; ")
                    )));
                }
                let shell = ShellConfig::default_for(platform);
                let mut spec =
                    ProcessSpec::new(shell.executable, PathBuf::from(&session.working_directory));
                spec.arguments = shell
                    .arguments
                    .into_iter()
                    .chain(std::iter::once(command.to_owned()))
                    .collect();
                let terminal_id = session
                    .terminals
                    .run(spec)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let mut operations = session.shell_operations.lock().await;
                if operations.contains_key(&operation_id) {
                    drop(operations);
                    let _ = session.terminals.interrupt(&terminal_id).await;
                    let _ = session.terminals.release(&terminal_id).await;
                    return Err(ResourceError::Conflict(format!(
                        "shell operation `{operation_id}` already exists"
                    )));
                }
                operations.insert(operation_id.clone(), terminal_id.clone());
                let state = json!({
                    "operationId": operation_id,
                    "terminalId": terminal_id,
                    "status": "running"
                });
                Ok(canonical_mutation(
                    "shell",
                    state,
                    "shell/updated",
                    Vec::new(),
                ))
            }
            "shell/interrupt" => {
                let terminal_id = session
                    .shell_operations
                    .lock()
                    .await
                    .remove(&operation_id)
                    .ok_or_else(|| {
                        ResourceError::NotFound(format!(
                            "shell operation `{operation_id}` was not found"
                        ))
                    })?;
                let output = session
                    .terminals
                    .interrupt(&terminal_id)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                session
                    .terminals
                    .release(&terminal_id)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let state = json!({
                    "operationId": operation_id,
                    "terminalId": terminal_id,
                    "status": output.state
                });
                Ok(canonical_mutation(
                    "shell",
                    state,
                    "shell/updated",
                    Vec::new(),
                ))
            }
            _ => Err(ResourceError::MethodNotFound(request.method.clone())),
        }
    }
}

impl ResourceBackend for CoreResourceBackend {
    fn open_session(&self, session: ResourceSession) -> Result<(), ResourceError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ResourceError::Unavailable("resource backend lock is poisoned".to_owned())
        })?;
        if let Some(existing) = sessions.get_mut(&session.session_id) {
            if session.generation > existing.generation {
                existing.generation = session.generation;
            }
            return Ok(());
        }
        if !sessions.contains_key(&session.session_id) && sessions.len() >= MAX_RESOURCE_SESSIONS {
            return Err(ResourceError::Conflict(
                "resource backend session capacity was reached".to_owned(),
            ));
        }
        sessions.insert(
            session.session_id,
            CoreResourceEntry {
                generation: session.generation,
                session: Arc::new(CoreResourceSession {
                    working_directory: session.working_directory,
                    policy: session.policy,
                    tools: session.tools,
                    mcp: McpRegistry::default(),
                    connectors: ConnectorRegistry::default(),
                    terminals: TerminalManager::default(),
                    shell_operations: Mutex::new(BTreeMap::new()),
                }),
            },
        );
        Ok(())
    }

    fn dispatch<'a>(
        &'a self,
        request: ResourceBackendRequest,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move {
            let session = self.session(&request.session_id)?;
            match request.method.as_str() {
                method if method.starts_with("mcp/") => self.dispatch_mcp(&session, &request).await,
                method if method.starts_with("connectors/") => {
                    self.dispatch_connectors(&session, &request).await
                }
                method if method.starts_with("shell/") => {
                    self.dispatch_shell(&session, &request).await
                }
                _ => Err(ResourceError::MethodNotFound(request.method)),
            }
        })
    }

    fn close_session<'a>(&'a self, session_id: &'a str, generation: u64) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            let session = {
                let mut sessions = self.sessions.lock().map_err(|_| {
                    ResourceError::Unavailable("resource backend lock is poisoned".to_owned())
                })?;
                let matches_generation = sessions
                    .get(session_id)
                    .is_some_and(|entry| entry.generation == generation);
                matches_generation
                    .then(|| sessions.remove(session_id))
                    .flatten()
                    .map(|entry| entry.session)
            };
            let Some(session) = session else {
                return Ok(());
            };
            let mut failures = session.mcp.close().await;
            if let Err(error) = session.connectors.close().await {
                failures.push(redact(&error.to_string()));
            }
            if let Err(error) = session.terminals.cleanup_all().await {
                failures.push(redact(&error.to_string()));
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(ResourceError::Unavailable(failures.join("; ")))
            }
        })
    }
}

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
            "connectors/read" => Ok(read_only([("counts", self.connector_counts())])),
            "connectors/auth/read" => self.connector_auth(params),
            "connectors/refresh" => self.connector_refresh(params),
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
            "mcp/add" => self.mcp_add(params),
            "mcp/login" => self.mcp_auth(params, true),
            "mcp/logout" => self.mcp_auth(params, false),
            "mcp/read" => Ok(read_only([("mcp", self.mcp_state())])),
            "mcp/refresh" => Err(ResourceError::Unavailable(
                "MCP refresh backend is not attached".to_owned(),
            )),
            "mcp/toggle" => self.mcp_toggle(params),
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
            "shell/run" => self.shell_run(params, session_active),
            "shell/interrupt" => self.shell_interrupt(params),
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

    pub fn validate_backend_request(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
        session_active: bool,
    ) -> Result<(), ResourceError> {
        match method {
            "connectors/read" | "mcp/read" => Ok(()),
            "connectors/auth/read"
            | "connectors/refresh"
            | "mcp/login"
            | "mcp/logout"
            | "mcp/refresh" => {
                required_string(params, "name")?;
                Ok(())
            }
            "mcp/add" => {
                let raw_url = required_string(params, "url")?;
                let url = Url::parse(raw_url).map_err(|_| {
                    ResourceError::InvalidParams("url must be valid HTTPS".to_owned())
                })?;
                if url.scheme() != "https" {
                    return Err(ResourceError::InvalidParams(
                        "MCP HTTP endpoints require HTTPS".to_owned(),
                    ));
                }
                let name = optional_string(params, "name")?.unwrap_or_else(|| {
                    url.host_str()
                        .unwrap_or("mcp")
                        .trim_matches(|character: char| !character.is_ascii_alphanumeric())
                });
                if name.is_empty() {
                    return Err(ResourceError::InvalidParams(
                        "MCP source name cannot be empty".to_owned(),
                    ));
                }
                let transport = optional_string(params, "transport")?.unwrap_or("streamable-http");
                if !matches!(transport, "http" | "streamable-http" | "sse") {
                    return Err(ResourceError::InvalidParams(
                        "unsupported MCP transport".to_owned(),
                    ));
                }
                Ok(())
            }
            "mcp/toggle" => {
                required_string(params, "name")?;
                required_bool(params, "disabled")?;
                optional_string(params, "toolName")?;
                Ok(())
            }
            "shell/run" => {
                if session_active {
                    return Err(ResourceError::Conflict(
                        "manual shell cannot run during an active turn".to_owned(),
                    ));
                }
                required_string(params, "operationId")?;
                let command = required_string(params, "command")?;
                if command.trim().is_empty() {
                    return Err(ResourceError::InvalidParams(
                        "shell command cannot be empty".to_owned(),
                    ));
                }
                Ok(())
            }
            "shell/interrupt" => {
                required_string(params, "operationId")?;
                Ok(())
            }
            _ => Err(ResourceError::MethodNotFound(method.to_owned())),
        }
    }

    fn connector_counts(&self) -> Value {
        json!({
            "connected": self.connectors.values().filter(|connected| **connected).count(),
            "total": self.connectors.len()
        })
    }

    fn connector_auth(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let name = required_string(params, "name")?;
        let connected = self
            .connectors
            .get(name)
            .ok_or_else(|| ResourceError::NotFound(format!("connector `{name}` was not found")))?;
        Ok(read_only([(
            "url",
            if *connected {
                Value::Null
            } else {
                json!(format!("https://connectors.mistral.ai/auth/{name}"))
            },
        )]))
    }

    fn connector_refresh(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let name = required_string(params, "name")?;
        Err(ResourceError::Unavailable(format!(
            "connector `{name}` refresh backend is not attached"
        )))
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

    fn mcp_add(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let raw_url = required_string(params, "url")?;
        let url = Url::parse(raw_url)
            .map_err(|_| ResourceError::InvalidParams("url must be valid HTTPS".to_owned()))?;
        if url.scheme() != "https" {
            return Err(ResourceError::InvalidParams(
                "MCP HTTP endpoints require HTTPS".to_owned(),
            ));
        }
        let requested_name = optional_string(params, "name")?
            .map(str::to_owned)
            .unwrap_or_else(|| {
                url.host_str()
                    .unwrap_or("mcp")
                    .replace(|character: char| !character.is_ascii_alphanumeric(), "_")
            });
        if requested_name.is_empty() {
            return Err(ResourceError::InvalidParams(
                "MCP source name cannot be empty".to_owned(),
            ));
        }
        let transport = optional_string(params, "transport")?.unwrap_or("streamable-http");
        if !matches!(transport, "http" | "streamable-http" | "sse") {
            return Err(ResourceError::InvalidParams(
                "unsupported MCP transport".to_owned(),
            ));
        }
        let _ = (url, transport);
        Err(ResourceError::Unavailable(format!(
            "MCP source `{requested_name}` cannot be added because no MCP backend is attached"
        )))
    }

    fn mcp_auth(
        &mut self,
        params: &BTreeMap<String, Value>,
        login: bool,
    ) -> Result<ResourceDispatch, ResourceError> {
        let name = required_string(params, "name")?;
        let action = if login { "login" } else { "logout" };
        Err(ResourceError::Unavailable(format!(
            "MCP source `{name}` {action} backend is not attached"
        )))
    }

    fn mcp_toggle(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let name = required_string(params, "name")?;
        let _ = required_bool(params, "disabled")?;
        let _ = optional_string(params, "toolName")?;
        Err(ResourceError::Unavailable(format!(
            "MCP source `{name}` toggle backend is not attached"
        )))
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

    fn shell_run(
        &mut self,
        params: &BTreeMap<String, Value>,
        session_active: bool,
    ) -> Result<ResourceDispatch, ResourceError> {
        if session_active {
            return Err(ResourceError::Conflict(
                "manual shell cannot run during an active turn".to_owned(),
            ));
        }
        let operation_id = required_string(params, "operationId")?;
        let command = required_string(params, "command")?;
        if command.trim().is_empty() {
            return Err(ResourceError::InvalidParams(
                "shell command cannot be empty".to_owned(),
            ));
        }
        Err(ResourceError::Unavailable(format!(
            "shell operation `{operation_id}` cannot run because no shell backend is attached"
        )))
    }

    fn shell_interrupt(
        &mut self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResourceDispatch, ResourceError> {
        let operation_id = required_string(params, "operationId")?;
        Err(ResourceError::Unavailable(format!(
            "shell operation `{operation_id}` cannot be interrupted because no shell backend is attached"
        )))
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

fn mcp_view(views: Vec<McpServerView>) -> Value {
    json!({
        "sources": views.into_iter().map(|view| {
            json!({
                "name": view.alias,
                "kind": "server",
                "transport": view.transport,
                "status": view.status,
                "enabled": view.enabled,
                "diagnostic": view.diagnostic,
                "tools": view.tools.into_iter().map(|name| {
                    json!({"name": name, "description": "", "enabled": true})
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
            matches!(
                view.auth_state,
                vibe_core::integrations::ConnectorAuthState::Connected
                    | vibe_core::integrations::ConnectorAuthState::NotRequired
            )
        }).count(),
        "total": views.len()
    })
}

fn mcp_alias(url: &Url) -> String {
    let alias = url
        .host_str()
        .unwrap_or("mcp")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    alias.trim_matches('_').to_owned()
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

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn required_string<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, ResourceError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= MAX_RESOURCE_STRING_BYTES && !value.contains('\0')
        })
        .ok_or_else(|| ResourceError::InvalidParams(format!("{key} must be a non-empty string")))
}

fn optional_string<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ResourceError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if value.len() <= MAX_RESOURCE_STRING_BYTES && !value.contains('\0') =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(ResourceError::InvalidParams(format!(
            "{key} must be a string"
        ))),
    }
}

fn required_bool(params: &BTreeMap<String, Value>, key: &str) -> Result<bool, ResourceError> {
    params
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| ResourceError::InvalidParams(format!("{key} must be a boolean")))
}

fn required_object<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, ResourceError> {
    params
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| ResourceError::InvalidParams(format!("{key} must be an object")))
}

fn usize_param(
    params: &BTreeMap<String, Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ResourceError> {
    let Some(value) = params.get(key) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (*value >= min) && (*value <= max))
        .ok_or_else(|| ResourceError::InvalidParams(format!("{key} is out of range")))?;
    Ok(value)
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

    fn params(value: Value) -> BTreeMap<String, Value> {
        value
            .as_object()
            .expect("object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
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
                policy,
                tools: ToolRegistry::default(),
            })
            .expect("open session");
        let dispatch = backend
            .dispatch(ResourceBackendRequest {
                session_id: "s1".to_owned(),
                method: "shell/run".to_owned(),
                params: params(json!({
                    "sessionId": "s1",
                    "operationId": "shell-1",
                    "command": "pwd"
                })),
            })
            .await
            .expect("run shell");
        assert_eq!(
            dispatch
                .notification
                .as_ref()
                .map(|notification| notification.method.as_str()),
            Some("shell/updated")
        );
        backend.close_session("s1", 1).await.expect("cleanup");
    }

    #[tokio::test]
    async fn stale_close_cannot_remove_a_reattached_resource_generation() {
        let backend = CoreResourceBackend::default();
        let session = |generation| ResourceSession {
            session_id: "s1".to_owned(),
            generation,
            working_directory: "/workspace".to_owned(),
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
            .dispatch(ResourceBackendRequest {
                session_id: "s1".to_owned(),
                method: "mcp/read".to_owned(),
                params: BTreeMap::new(),
            })
            .await
            .expect("reattached resources remain available");
        assert!(dispatch.result.contains_key("mcp"));

        backend
            .close_session("s1", 2)
            .await
            .expect("current cleanup");
        assert!(matches!(
            backend
                .dispatch(ResourceBackendRequest {
                    session_id: "s1".to_owned(),
                    method: "mcp/read".to_owned(),
                    params: BTreeMap::new(),
                })
                .await,
            Err(ResourceError::NotFound(_))
        ));
    }
}
