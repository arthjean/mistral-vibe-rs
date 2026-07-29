use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock};
use url::Url;

use crate::policy::{ApprovalAgent, PermissionRequirement, PermissionStore, PolicyGuardedTool};
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolHandler, ToolInvocation,
    ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

const CONNECTOR_CACHE_TTL_MS: u64 = 600_000;
const MAX_PUBLIC_LOG_MESSAGE: usize = 2_048;
const MAX_CONNECTORS: usize = 256;
const MAX_CONNECTOR_TOOLS: usize = 256;
const MAX_DIAGNOSTICS: usize = 1_024;
const MAX_LOG_RECORDS: usize = 4_096;
const MAX_FEEDBACK_RECORDS: usize = 256;
const CONNECTOR_CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub type ConnectorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, IntegrationError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorAuthKind {
    None,
    OAuth,
    CredentialSetup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorAuthState {
    NotRequired,
    Disconnected,
    Connected,
    SetupRequired,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorDefinition {
    pub id: String,
    pub name: String,
    pub base_url: Url,
    pub auth_kind: ConnectorAuthKind,
    #[serde(default)]
    pub tools: Vec<ConnectorTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorView {
    pub id: String,
    pub alias: String,
    pub name: String,
    pub base_url: Url,
    pub auth_kind: ConnectorAuthKind,
    pub auth_state: ConnectorAuthState,
    pub tool_names: Vec<String>,
    #[serde(default)]
    pub diagnostic: Option<String>,
}

pub trait ConnectorBackend: Send + Sync {
    fn call<'a>(
        &'a self,
        connector_id: &'a str,
        tool: &'a str,
        arguments: Value,
        max_response_bytes: usize,
    ) -> ConnectorFuture<'a>;
}

#[derive(Default)]
struct ConnectorState {
    cache_key: Option<String>,
    cached_at: u64,
    views: BTreeMap<String, ConnectorView>,
    definitions: BTreeMap<String, ConnectorDefinition>,
    auth_gates: BTreeMap<String, Arc<AsyncRwLock<ConnectorAuthState>>>,
    tools: Option<ToolRegistry>,
}

#[derive(Clone, Default)]
pub struct ConnectorRegistry {
    state: Arc<Mutex<ConnectorState>>,
    mutation: Arc<AsyncMutex<()>>,
}

impl ConnectorRegistry {
    pub async fn discover(
        &self,
        definitions: Vec<ConnectorDefinition>,
        credential_reference: &str,
        base_url: &Url,
        now: u64,
    ) -> Result<Vec<ConnectorView>, IntegrationError> {
        let _mutation = self.mutation.lock().await;
        if definitions.len() > MAX_CONNECTORS {
            return Err(IntegrationError::InvalidConnector(format!(
                "connector count exceeds limit of {MAX_CONNECTORS}"
            )));
        }
        let cache_key = format!("{credential_reference}\n{base_url}");
        let (old_gates, old_tools, old_names) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
            if state.cache_key.as_deref() == Some(&cache_key)
                && now.saturating_sub(state.cached_at) <= CONNECTOR_CACHE_TTL_MS
            {
                return Ok(state.views.values().cloned().collect());
            }
            let old_names = state
                .views
                .values()
                .flat_map(|view| view.tool_names.iter().cloned())
                .collect::<Vec<_>>();
            (
                std::mem::take(&mut state.auth_gates),
                state.tools.clone(),
                old_names,
            )
        };
        for gate in old_gates.values() {
            *gate.write().await = ConnectorAuthState::Disconnected;
        }
        if let Some(tools) = old_tools {
            for name in old_names {
                let _ = tools.set_availability(
                    &name,
                    ToolSource::Connector,
                    ToolAvailability::Unavailable,
                );
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        let mut aliases = BTreeMap::<String, usize>::new();
        state.views.clear();
        state.definitions.clear();
        for definition in definitions {
            validate_definition(&definition)?;
            let base_alias = normalize_alias(&definition.name);
            let count = aliases.entry(base_alias.clone()).or_default();
            *count = count.saturating_add(1);
            let alias = if *count == 1 {
                base_alias
            } else {
                format!("{base_alias}_{count}")
            };
            let auth_state = match definition.auth_kind {
                ConnectorAuthKind::None => ConnectorAuthState::NotRequired,
                ConnectorAuthKind::OAuth => ConnectorAuthState::Disconnected,
                ConnectorAuthKind::CredentialSetup => ConnectorAuthState::SetupRequired,
            };
            let mut tool_names = definition
                .tools
                .iter()
                .map(|tool| format!("connector_{alias}_{}", normalize_alias(&tool.name)))
                .collect::<Vec<_>>();
            tool_names.sort();
            state.views.insert(
                definition.id.clone(),
                ConnectorView {
                    id: definition.id.clone(),
                    alias,
                    name: definition.name.clone(),
                    base_url: definition.base_url.clone(),
                    auth_kind: definition.auth_kind,
                    auth_state,
                    tool_names,
                    diagnostic: None,
                },
            );
            state.auth_gates.insert(
                definition.id.clone(),
                Arc::new(AsyncRwLock::new(auth_state)),
            );
            state.definitions.insert(definition.id.clone(), definition);
        }
        state.cache_key = Some(cache_key);
        state.cached_at = now;
        Ok(state.views.values().cloned().collect())
    }

    pub fn views(&self) -> Result<Vec<ConnectorView>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?
            .views
            .values()
            .cloned()
            .collect())
    }

    pub async fn set_auth(
        &self,
        connector_id: &str,
        connected: bool,
    ) -> Result<ConnectorView, IntegrationError> {
        let _mutation = self.mutation.lock().await;
        let (auth_state, gate, tools, tool_names) =
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
                let view = state
                    .views
                    .get(connector_id)
                    .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.to_owned()))?;
                let auth_state = if connected {
                    ConnectorAuthState::Connected
                } else {
                    match view.auth_kind {
                        ConnectorAuthKind::None => ConnectorAuthState::NotRequired,
                        ConnectorAuthKind::OAuth => ConnectorAuthState::Disconnected,
                        ConnectorAuthKind::CredentialSetup => ConnectorAuthState::SetupRequired,
                    }
                };
                (
                    auth_state,
                    state.auth_gates.get(connector_id).cloned().ok_or_else(|| {
                        IntegrationError::ConnectorNotFound(connector_id.to_owned())
                    })?,
                    state.tools.clone(),
                    view.tool_names.clone(),
                )
            };
        *gate.write().await = auth_state;
        if let Some(tools) = tools {
            let availability = if matches!(
                auth_state,
                ConnectorAuthState::Connected | ConnectorAuthState::NotRequired
            ) {
                ToolAvailability::Available
            } else {
                ToolAvailability::Unavailable
            };
            for name in tool_names {
                tools
                    .set_availability(&name, ToolSource::Connector, availability)
                    .map_err(|error| IntegrationError::Tool(error.to_string()))?;
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        let view = state
            .views
            .get_mut(connector_id)
            .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.to_owned()))?;
        view.auth_state = auth_state;
        Ok(view.clone())
    }

    pub async fn fail_auth(
        &self,
        connector_id: &str,
        error: &str,
    ) -> Result<ConnectorView, IntegrationError> {
        let _mutation = self.mutation.lock().await;
        let (gate, tools, tool_names) =
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
                let view = state
                    .views
                    .get(connector_id)
                    .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.to_owned()))?;
                (
                    state.auth_gates.get(connector_id).cloned().ok_or_else(|| {
                        IntegrationError::ConnectorNotFound(connector_id.to_owned())
                    })?,
                    state.tools.clone(),
                    view.tool_names.clone(),
                )
            };
        *gate.write().await = ConnectorAuthState::Failed;
        if let Some(tools) = tools {
            for name in tool_names {
                tools
                    .set_availability(&name, ToolSource::Connector, ToolAvailability::Unavailable)
                    .map_err(|tool_error| IntegrationError::Tool(tool_error.to_string()))?;
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        let view = state
            .views
            .get_mut(connector_id)
            .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.to_owned()))?;
        view.auth_state = ConnectorAuthState::Failed;
        view.diagnostic = Some(redact(error));
        Ok(view.clone())
    }

    pub async fn close(&self) -> Result<(), IntegrationError> {
        let _mutation = self.mutation.lock().await;
        let (gates, tools, tool_names) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
            let gates = std::mem::take(&mut state.auth_gates);
            let tools = state.tools.take();
            let tool_names = state
                .views
                .values()
                .flat_map(|view| view.tool_names.iter().cloned())
                .collect::<Vec<_>>();
            state.cache_key = None;
            state.cached_at = 0;
            state.views.clear();
            state.definitions.clear();
            (gates, tools, tool_names)
        };
        for gate in gates.values() {
            *gate.write().await = ConnectorAuthState::Disconnected;
        }
        if let Some(tools) = tools {
            for name in tool_names {
                tools
                    .set_availability(&name, ToolSource::Connector, ToolAvailability::Unavailable)
                    .map_err(|error| IntegrationError::Tool(error.to_string()))?;
            }
        }
        Ok(())
    }

    pub fn register_tools(
        &self,
        tools: &ToolRegistry,
        backend: Arc<dyn ConnectorBackend>,
        policy: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
    ) -> Result<Vec<String>, IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        state.tools = Some(tools.clone());
        let mut registered = Vec::new();
        for (connector_id, definition) in &state.definitions {
            let view = state
                .views
                .get(connector_id)
                .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.clone()))?;
            for remote in &definition.tools {
                let public_name =
                    format!("connector_{}_{}", view.alias, normalize_alias(&remote.name));
                let backend = backend.clone();
                let connector = connector_id.clone();
                let remote_name = remote.name.clone();
                let auth_gate = state
                    .auth_gates
                    .get(connector_id)
                    .cloned()
                    .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.clone()))?;
                let handler: Arc<dyn ToolHandler> = Arc::new(
                    move |invocation: &ToolInvocation,
                          output: ToolOutputSink|
                          -> OwnedToolHandlerFuture {
                        let backend = backend.clone();
                        let connector = connector.clone();
                        let remote_name = remote_name.clone();
                        let arguments = invocation.arguments.clone();
                        let auth_gate = auth_gate.clone();
                        Box::pin(async move {
                            let auth = auth_gate.read_owned().await;
                            if !matches!(
                                *auth,
                                ConnectorAuthState::Connected | ConnectorAuthState::NotRequired
                            ) {
                                return Err(ToolError::Unavailable(format!(
                                    "connector `{connector}` is not authenticated"
                                )));
                            }
                            let response = tokio::time::timeout(
                                CONNECTOR_CALL_TIMEOUT,
                                backend.call(
                                    &connector,
                                    &remote_name,
                                    arguments,
                                    output.remaining_bytes(),
                                ),
                            )
                            .await
                            .map_err(|_| {
                                ToolError::Execution(
                                    "connector request exceeded its deadline".to_owned(),
                                )
                            })?
                            .map_err(|error| ToolError::Execution(error.to_string()))?;
                            if response.len() > output.remaining_bytes() {
                                return Err(ToolError::OutputTooLarge {
                                    actual: response.len(),
                                    limit: output.remaining_bytes(),
                                });
                            }
                            serde_json::from_slice(&response)
                                .map_err(|error| ToolError::InvalidResult(error.to_string()))
                        })
                    },
                );
                let endpoint = definition.base_url.clone();
                let guarded = Arc::new(PolicyGuardedTool::new(
                    public_name.clone(),
                    policy.clone(),
                    approval.clone(),
                    Arc::new(move |_invocation| {
                        Ok(vec![PermissionRequirement::Network {
                            url: endpoint.clone(),
                        }])
                    }),
                    handler,
                ));
                tools
                    .register(
                        ToolSpec {
                            name: public_name.clone(),
                            description: remote.description.clone(),
                            input_schema: remote.input_schema.clone(),
                            output_schema: remote.output_schema.clone(),
                            config: Value::Null,
                            state: Value::Null,
                            availability: if matches!(
                                view.auth_state,
                                ConnectorAuthState::Connected | ConnectorAuthState::NotRequired
                            ) {
                                ToolAvailability::Available
                            } else {
                                ToolAvailability::Unavailable
                            },
                            presentation: ToolPresentationKind::Connector,
                            source: ToolSource::Connector,
                            selection_priority: 40,
                        },
                        guarded,
                    )
                    .map_err(|error| IntegrationError::Tool(error.to_string()))?;
                registered.push(public_name);
            }
        }
        registered.sort();
        Ok(registered)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub authenticated: bool,
    pub account_id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    pub timestamp: u64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationalStats {
    pub turns: u64,
    pub tool_calls: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeView {
    pub version: String,
    pub ready: bool,
    pub active_sessions: usize,
}

#[derive(Default)]
struct OperationalState {
    account: Option<AccountView>,
    diagnostics: Vec<Diagnostic>,
    logs: Vec<LogRecord>,
    feedback: Vec<String>,
    stats: OperationalStats,
    active_sessions: usize,
    ready: bool,
}

pub struct OperationalResources {
    version: String,
    state: Mutex<OperationalState>,
}

impl OperationalResources {
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            state: Mutex::new(OperationalState::default()),
        }
    }

    pub fn set_account(&self, account: AccountView) -> Result<(), IntegrationError> {
        self.state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .account = Some(account);
        Ok(())
    }

    pub fn account(&self) -> Result<Option<AccountView>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .account
            .clone())
    }

    pub fn record_diagnostic(
        &self,
        code: impl Into<String>,
        message: &str,
        source: impl Into<String>,
    ) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        push_bounded(
            &mut state.diagnostics,
            MAX_DIAGNOSTICS,
            Diagnostic {
                code: bounded_text(&code.into(), 128),
                message: redact(message),
                source: bounded_text(&source.into(), 128),
            },
        );
        Ok(())
    }

    pub fn diagnostics(&self) -> Result<Vec<Diagnostic>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .diagnostics
            .clone())
    }

    pub fn record_log(
        &self,
        timestamp: u64,
        level: impl Into<String>,
        message: &str,
    ) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        push_bounded(
            &mut state.logs,
            MAX_LOG_RECORDS,
            LogRecord {
                timestamp,
                level: bounded_text(&level.into(), 32),
                message: redact(message),
            },
        );
        Ok(())
    }

    pub fn logs(&self, offset: usize, limit: usize) -> Result<Vec<LogRecord>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .logs
            .iter()
            .skip(offset)
            .take(limit.min(1_000))
            .cloned()
            .collect())
    }

    pub fn record_feedback(&self, message: &str) -> Result<usize, IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        push_bounded(&mut state.feedback, MAX_FEEDBACK_RECORDS, redact(message));
        Ok(state.feedback.len())
    }

    pub fn should_show_feedback(&self) -> Result<bool, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .feedback
            .is_empty())
    }

    pub fn summarize(&self, text: &str) -> Result<String, IntegrationError> {
        if text.trim().is_empty() {
            return Err(IntegrationError::UnsupportedNarration(
                "cannot summarize empty text".to_owned(),
            ));
        }
        Ok(text.chars().take(280).collect())
    }

    pub fn set_runtime(&self, ready: bool, active_sessions: usize) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        state.ready = ready;
        state.active_sessions = active_sessions;
        Ok(())
    }

    pub fn runtime(&self) -> Result<RuntimeView, IntegrationError> {
        let state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        Ok(RuntimeView {
            version: self.version.clone(),
            ready: state.ready,
            active_sessions: state.active_sessions,
        })
    }

    pub fn stats(&self) -> Result<OperationalStats, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .stats
            .clone())
    }

    pub fn record_tool_outcome(&self, failed: bool) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        state.stats.tool_calls = state.stats.tool_calls.saturating_add(1);
        if failed {
            state.stats.failures = state.stats.failures.saturating_add(1);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum IntegrationError {
    #[error("{0} lock is poisoned")]
    LockPoisoned(&'static str),
    #[error("connector `{0}` was not found")]
    ConnectorNotFound(String),
    #[error("invalid connector: {0}")]
    InvalidConnector(String),
    #[error("connector tool failed: {0}")]
    Tool(String),
    #[error("narration is unavailable: {0}")]
    UnsupportedNarration(String),
    #[error("connector backend unavailable")]
    BackendUnavailable,
}

fn validate_definition(definition: &ConnectorDefinition) -> Result<(), IntegrationError> {
    if definition.id.is_empty()
        || definition.name.is_empty()
        || definition.base_url.scheme() != "https"
    {
        return Err(IntegrationError::InvalidConnector(
            "id, name, and HTTPS base URL are required".to_owned(),
        ));
    }
    if definition.tools.len() > MAX_CONNECTOR_TOOLS {
        return Err(IntegrationError::InvalidConnector(format!(
            "tool count exceeds limit of {MAX_CONNECTOR_TOOLS}"
        )));
    }
    if definition.tools.iter().any(|tool| {
        tool.name.is_empty()
            || tool.input_schema.get("type").and_then(Value::as_str) != Some("object")
    }) {
        return Err(IntegrationError::InvalidConnector(
            "tool names and object schemas are required".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_alias(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    normalized.trim_matches('_').to_owned()
}

pub fn redact(message: &str) -> String {
    let bounded = bounded_text(message, MAX_PUBLIC_LOG_MESSAGE);
    let lowered = bounded.to_ascii_lowercase();
    let sensitive = [
        "authorization:",
        "bearer ",
        "api_key=",
        "api-key=",
        "apikey=",
        "api key",
        "x-api-key",
        "token=",
        "access_token",
        "refresh_token",
        "password=",
        "secret=",
        "client_secret",
        "client-secret",
    ];
    let uri_userinfo = lowered.find("://").is_some_and(|scheme| {
        lowered[scheme + 3..]
            .split('/')
            .next()
            .is_some_and(|authority| authority.contains('@'))
    });
    if uri_userinfo || sensitive.iter().any(|marker| lowered.contains(marker)) {
        return "[redacted sensitive error]".to_owned();
    }
    bounded
}

fn bounded_text(message: &str, max_chars: usize) -> String {
    message.chars().take(max_chars).collect()
}

fn push_bounded<T>(values: &mut Vec<T>, limit: usize, value: T) {
    if values.len() == limit {
        values.remove(0);
    }
    values.push(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode, PermissionRule,
    };
    use crate::tools::ToolExecutionOutput;
    use serde_json::json;

    struct Approve;

    impl ApprovalAgent for Approve {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
        }
    }

    struct FakeBackend;

    impl ConnectorBackend for FakeBackend {
        fn call<'a>(
            &'a self,
            _connector_id: &'a str,
            tool: &'a str,
            _arguments: Value,
            max_response_bytes: usize,
        ) -> ConnectorFuture<'a> {
            Box::pin(async move {
                let bytes =
                    serde_json::to_vec(&ToolExecutionOutput::text(format!("{tool} complete")))
                        .map_err(|error| IntegrationError::Tool(error.to_string()))?;
                if bytes.len() > max_response_bytes {
                    return Err(IntegrationError::Tool(
                        "connector response exceeded budget".to_owned(),
                    ));
                }
                Ok(bytes)
            })
        }
    }

    fn definition(id: &str, name: &str) -> ConnectorDefinition {
        ConnectorDefinition {
            id: id.to_owned(),
            name: name.to_owned(),
            base_url: Url::parse("https://connectors.example").expect("url"),
            auth_kind: ConnectorAuthKind::None,
            tools: vec![ConnectorTool {
                name: "search".to_owned(),
                description: "Search".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                output_schema: None,
            }],
        }
    }

    #[tokio::test]
    async fn discovery_cache_keys_credentials_and_deduplicates_aliases() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        let views = registry
            .discover(
                vec![
                    definition("one", "Issue Tracker"),
                    definition("two", "Issue Tracker"),
                ],
                "credential-a",
                &base,
                100,
            )
            .await
            .expect("discover");
        assert_eq!(views[0].alias, "issue_tracker");
        assert_eq!(views[1].alias, "issue_tracker_2");
        let cached = registry
            .discover(Vec::new(), "credential-a", &base, 200)
            .await
            .expect("cached");
        assert_eq!(cached, views);
        let refreshed = registry
            .discover(Vec::new(), "credential-b", &base, 200)
            .await
            .expect("new key");
        assert!(refreshed.is_empty());
    }

    #[tokio::test]
    async fn connector_tools_remain_server_policy_guarded() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        registry
            .discover(vec![definition("one", "Tracker")], "credential", &base, 1)
            .await
            .expect("discover");
        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        policy
            .add_rule(PermissionRule {
                tool: "connector_tracker_search".to_owned(),
                scope: "network https://connectors.example/".to_owned(),
                mode: PermissionMode::Always,
                rationale: "test connector".to_owned(),
            })
            .await;
        registry
            .register_tools(&tools, Arc::new(FakeBackend), policy, Arc::new(Approve))
            .expect("register");
        let output = tools
            .invoke(
                "connector_tracker_search",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({}),
                },
            )
            .await
            .expect("invoke");
        assert_eq!(output.model_text, "search complete");
    }

    #[tokio::test]
    async fn connector_logout_revokes_registered_tool_before_next_call() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        let mut connector = definition("one", "Tracker");
        connector.auth_kind = ConnectorAuthKind::OAuth;
        registry
            .discover(vec![connector], "credential", &base, 1)
            .await
            .expect("discover");
        registry.set_auth("one", true).await.expect("login");

        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        policy
            .add_rule(PermissionRule {
                tool: "connector_tracker_search".to_owned(),
                scope: "network https://connectors.example/".to_owned(),
                mode: PermissionMode::Always,
                rationale: "test connector".to_owned(),
            })
            .await;
        registry
            .register_tools(&tools, Arc::new(FakeBackend), policy, Arc::new(Approve))
            .expect("register");
        registry.set_auth("one", false).await.expect("logout");

        assert!(matches!(
            tools
                .invoke(
                    "connector_tracker_search",
                    ToolInvocation {
                        call_id: "call-logout".to_owned(),
                        arguments: json!({}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }

    #[test]
    fn operational_errors_and_logs_never_project_secrets() {
        let resources = OperationalResources::new("2.23.1");
        resources
            .record_diagnostic(
                "connector_auth",
                "Authorization: Bearer super-secret",
                "connector",
            )
            .expect("diagnostic");
        resources
            .record_log(1, "error", "request failed token=secret")
            .expect("log");
        assert_eq!(
            resources.diagnostics().expect("diagnostics")[0].message,
            "[redacted sensitive error]"
        );
        assert_eq!(
            resources.logs(0, 10).expect("logs")[0].message,
            "[redacted sensitive error]"
        );
        assert_eq!(
            redact("https://alice:password@example.test/path"),
            "[redacted sensitive error]"
        );
        assert_eq!(
            redact("refresh_token=never-project-this"),
            "[redacted sensitive error]"
        );
    }
}
