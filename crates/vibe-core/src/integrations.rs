use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock};
use url::Url;

mod operational;
mod shared;

pub use operational::{
    AccountView, Diagnostic, LogRecord, OperationalResources, OperationalStats, RuntimeView,
};
pub use shared::{IntegrationError, redact};
use shared::{
    connector_availability_updates, connector_tool_spec, normalize_alias,
    validate_connector_tool_specs, validate_definition,
};

use crate::policy::{ApprovalAgent, PermissionRequirement, PermissionStore, PolicyGuardedTool};
use crate::remote_tools::{public_tool_name, set_all};
use crate::text::canonical_url;
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolRegistry, ToolSource,
};

const CONNECTOR_CACHE_TTL_MS: u64 = 600_000;
const MAX_CONNECTORS: usize = 256;
const MAX_CONNECTOR_TOOLS: usize = 256;
const CONNECTOR_CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub type ConnectorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, IntegrationError>> + Send + 'a>>;

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
    #[serde(default = "default_connector_enabled")]
    pub enabled: bool,
    pub tool_names: Vec<String>,
    #[serde(default)]
    pub disabled_tools: BTreeSet<String>,
    #[serde(default)]
    pub diagnostic: Option<String>,
}

const fn default_connector_enabled() -> bool {
    true
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
        let (previous, old_gates, old_tools, old_names) = {
            let state = self
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
                state
                    .views
                    .iter()
                    .map(|(id, view)| {
                        (
                            id.clone(),
                            (
                                view.auth_kind,
                                view.auth_state,
                                view.enabled,
                                view.disabled_tools.clone(),
                                view.diagnostic.clone(),
                            ),
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
                state.auth_gates.values().cloned().collect::<Vec<_>>(),
                state.tools.clone(),
                old_names,
            )
        };
        let mut aliases = BTreeSet::new();
        let mut resources = BTreeSet::new();
        let mut views = BTreeMap::new();
        let mut next_definitions = BTreeMap::new();
        let mut auth_gates = BTreeMap::new();
        for definition in definitions {
            validate_definition(&definition)?;
            if next_definitions.contains_key(&definition.id) {
                return Err(IntegrationError::InvalidConnector(format!(
                    "connector ID `{}` appears more than once",
                    definition.id
                )));
            }
            let base_alias = normalize_alias(&definition.name);
            if !aliases.insert(base_alias.clone()) {
                return Err(IntegrationError::InvalidConnector(format!(
                    "connector alias `{base_alias}` appears more than once"
                )));
            }
            if !resources.insert(canonical_url(&definition.base_url)) {
                return Err(IntegrationError::InvalidConnector(
                    "connector URL appears more than once".to_owned(),
                ));
            }
            let alias = base_alias;
            let default_auth_state = match definition.auth_kind {
                ConnectorAuthKind::None => ConnectorAuthState::NotRequired,
                ConnectorAuthKind::OAuth => ConnectorAuthState::Disconnected,
                ConnectorAuthKind::CredentialSetup => ConnectorAuthState::SetupRequired,
            };
            let auth_state = previous
                .get(&definition.id)
                .filter(|(auth_kind, _, _, _, _)| *auth_kind == definition.auth_kind)
                .map(|(_, auth_state, _, _, _)| *auth_state)
                .unwrap_or(default_auth_state);
            let mut tool_names = definition
                .tools
                .iter()
                .map(|tool| public_tool_name(ToolSource::Connector, &alias, &tool.name))
                .collect::<Vec<_>>();
            tool_names.sort();
            let disabled_tools = previous
                .get(&definition.id)
                .map(|(_, _, _, tools, _)| {
                    tools
                        .iter()
                        .filter(|tool| tool_names.contains(tool))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let view = ConnectorView {
                id: definition.id.clone(),
                alias,
                name: definition.name.clone(),
                base_url: definition.base_url.clone(),
                auth_kind: definition.auth_kind,
                auth_state,
                enabled: previous
                    .get(&definition.id)
                    .map(|(_, _, enabled, _, _)| *enabled)
                    .unwrap_or(true),
                tool_names,
                disabled_tools,
                diagnostic: previous
                    .get(&definition.id)
                    .and_then(|(_, _, _, _, diagnostic)| diagnostic.clone()),
            };
            validate_connector_tool_specs(&view, &definition)?;
            views.insert(definition.id.clone(), view);
            auth_gates.insert(
                definition.id.clone(),
                Arc::new(AsyncRwLock::new(auth_state)),
            );
            next_definitions.insert(definition.id.clone(), definition);
        }
        if let Some(tools) = old_tools {
            let reconciled = set_all(
                &tools,
                ToolSource::Connector,
                &old_names,
                ToolAvailability::Unavailable,
            )
            .map_err(|error| IntegrationError::Tool(error.to_string()))?;
            if !reconciled {
                return Err(IntegrationError::Tool(
                    "connector tool registry is out of sync with the live catalog".to_owned(),
                ));
            }
        }
        let result = views.values().cloned().collect();
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
            state.views = views;
            state.definitions = next_definitions;
            state.auth_gates = auth_gates;
            state.cache_key = Some(cache_key);
            state.cached_at = now;
        }
        for gate in old_gates {
            *gate.write().await = ConnectorAuthState::Disconnected;
        }
        Ok(result)
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

    pub fn invalidate_cache(&self) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        state.cache_key = None;
        state.cached_at = 0;
        Ok(())
    }

    pub async fn toggle(
        &self,
        connector_id: &str,
        tool_name: Option<&str>,
        enabled: bool,
    ) -> Result<ConnectorView, IntegrationError> {
        let _mutation = self.mutation.lock().await;
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        let tools = state.tools.clone();
        let mut next = state
            .views
            .get(connector_id)
            .cloned()
            .ok_or_else(|| IntegrationError::ConnectorNotFound(connector_id.to_owned()))?;
        let names = if let Some(tool_name) = tool_name {
            if !next.tool_names.iter().any(|name| name == tool_name) {
                return Err(IntegrationError::Tool(format!(
                    "connector `{connector_id}` has no tool `{tool_name}`"
                )));
            }
            if enabled {
                next.disabled_tools.remove(tool_name);
            } else {
                next.disabled_tools.insert(tool_name.to_owned());
            }
            vec![tool_name.to_owned()]
        } else {
            next.enabled = enabled;
            next.tool_names.clone()
        };
        if let Some(tools) = tools {
            let updates = connector_availability_updates(&next, names);
            if !tools
                .set_availabilities(ToolSource::Connector, &updates)
                .map_err(|error| IntegrationError::Tool(error.to_string()))?
            {
                return Err(IntegrationError::Tool(format!(
                    "connector `{connector_id}` tools are no longer registered"
                )));
            }
        }
        state.views.insert(connector_id.to_owned(), next.clone());
        Ok(next)
    }

    pub async fn set_auth(
        &self,
        connector_id: &str,
        connected: bool,
    ) -> Result<ConnectorView, IntegrationError> {
        let _mutation = self.mutation.lock().await;
        let (previous_auth, next, gate, tools) =
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
                let mut next = view.clone();
                next.auth_state = auth_state;
                (
                    view.auth_state,
                    next,
                    state.auth_gates.get(connector_id).cloned().ok_or_else(|| {
                        IntegrationError::ConnectorNotFound(connector_id.to_owned())
                    })?,
                    state.tools.clone(),
                )
            };
        let updates = connector_availability_updates(&next, next.tool_names.clone());
        let becoming_available = matches!(
            next.auth_state,
            ConnectorAuthState::Connected | ConnectorAuthState::NotRequired
        );
        if becoming_available {
            *gate.write().await = next.auth_state;
        }
        if let Some(tools) = tools {
            let updated = tools
                .set_availabilities(ToolSource::Connector, &updates)
                .map_err(|error| IntegrationError::Tool(error.to_string()))?;
            if !updated {
                if becoming_available {
                    *gate.write().await = previous_auth;
                }
                return Err(IntegrationError::Tool(format!(
                    "connector `{connector_id}` tools are no longer registered"
                )));
            }
        }
        if !becoming_available {
            *gate.write().await = next.auth_state;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        state.views.insert(connector_id.to_owned(), next.clone());
        Ok(next)
    }

    pub async fn fail_auth(
        &self,
        connector_id: &str,
        error: &str,
    ) -> Result<ConnectorView, IntegrationError> {
        let _mutation = self.mutation.lock().await;
        let (mut next, gate, tools) =
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
                    view.clone(),
                    state.auth_gates.get(connector_id).cloned().ok_or_else(|| {
                        IntegrationError::ConnectorNotFound(connector_id.to_owned())
                    })?,
                    state.tools.clone(),
                )
            };
        next.auth_state = ConnectorAuthState::Failed;
        next.diagnostic = Some(redact(error));
        if let Some(tools) = tools {
            let updates = connector_availability_updates(&next, next.tool_names.clone());
            if !tools
                .set_availabilities(ToolSource::Connector, &updates)
                .map_err(|tool_error| IntegrationError::Tool(tool_error.to_string()))?
            {
                return Err(IntegrationError::Tool(format!(
                    "connector `{connector_id}` tools are no longer registered"
                )));
            }
        }
        *gate.write().await = ConnectorAuthState::Failed;
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("connectors"))?;
        state.views.insert(connector_id.to_owned(), next.clone());
        Ok(next)
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
                let spec = connector_tool_spec(view, remote);
                let public_name = spec.name.clone();
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
                            tokio::time::timeout(
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
                            .map_err(|error| ToolError::Execution(error.to_string()))
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
                    .register(spec, guarded)
                    .map_err(|error| IntegrationError::Tool(error.to_string()))?;
                registered.push(public_name);
            }
        }
        registered.sort();
        Ok(registered)
    }
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
                let output = ToolExecutionOutput::text(format!("{tool} complete"));
                // A real backend rejects the payload against the wire budget.
                if output.model_text.len() > max_response_bytes {
                    return Err(IntegrationError::Tool(
                        "connector response exceeded budget".to_owned(),
                    ));
                }
                Ok(output)
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
    async fn discovery_cache_keys_credentials_and_rejects_identity_collisions() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        assert!(matches!(
            registry
            .discover(
                vec![
                    definition("one", "Issue Tracker"),
                    definition("two", "Issue Tracker"),
                ],
                "credential-a",
                &base,
                100,
            )
            .await,
            Err(IntegrationError::InvalidConnector(message))
                if message.contains("alias `issue_tracker`")
        ));
        assert!(registry.views().expect("unchanged registry").is_empty());

        let mut duplicate_url = definition("two", "Calendar");
        duplicate_url.base_url = Url::parse("https://connectors.example/").expect("URL");
        assert!(matches!(
            registry
                .discover(
                    vec![definition("one", "Issue Tracker"), duplicate_url],
                    "credential-a",
                    &base,
                    100,
                )
                .await,
            Err(IntegrationError::InvalidConnector(message))
                if message.contains("URL appears more than once")
        ));
        assert!(registry.views().expect("unchanged registry").is_empty());

        let views = registry
            .discover(
                vec![definition("one", "Issue Tracker")],
                "credential-a",
                &base,
                100,
            )
            .await
            .expect("discover");
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
    async fn invalid_catalog_refresh_preserves_the_live_registry() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        let initial = registry
            .discover(vec![definition("one", "Tracker")], "credential", &base, 1)
            .await
            .expect("initial catalog");
        let tools = ToolRegistry::default();
        registry
            .register_tools(
                &tools,
                Arc::new(FakeBackend),
                PermissionStore::default(),
                Arc::new(Approve),
            )
            .expect("register tools");
        registry.invalidate_cache().expect("invalidate cache");
        let mut invalid = definition("two", "Broken");
        invalid.tools[0].input_schema = json!({"type": "object", "properties": []});

        assert!(matches!(
            registry
                .discover(vec![invalid], "credential", &base, 2)
                .await,
            Err(IntegrationError::InvalidConnector(_))
        ));
        assert_eq!(registry.views().expect("preserved views"), initial);
        tools
            .invoke(
                "connector_tracker_search",
                ToolInvocation {
                    call_id: "after-invalid-refresh".to_owned(),
                    arguments: json!({}),
                },
            )
            .await
            .expect("existing tool remains live");
    }

    #[tokio::test]
    async fn desynchronized_tool_registry_aborts_catalog_swap() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        let initial = registry
            .discover(vec![definition("one", "Tracker")], "credential", &base, 1)
            .await
            .expect("initial catalog");
        registry
            .register_tools(
                &ToolRegistry::default(),
                Arc::new(FakeBackend),
                PermissionStore::default(),
                Arc::new(Approve),
            )
            .expect("register tools");
        registry.state.lock().expect("connector state").tools = Some(ToolRegistry::default());
        registry.invalidate_cache().expect("invalidate cache");

        assert!(matches!(
            registry
                .discover(vec![definition("two", "Calendar")], "credential", &base, 2)
                .await,
            Err(IntegrationError::Tool(message)) if message.contains("out of sync")
        ));
        assert_eq!(registry.views().expect("preserved views"), initial);
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
    async fn connector_tool_toggle_survives_catalog_refresh() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        registry
            .discover(vec![definition("one", "Tracker")], "credential", &base, 1)
            .await
            .expect("discover");
        let tools = ToolRegistry::default();
        registry
            .register_tools(
                &tools,
                Arc::new(FakeBackend),
                PermissionStore::default(),
                Arc::new(Approve),
            )
            .expect("register");

        let disabled = registry
            .toggle("one", Some("connector_tracker_search"), false)
            .await
            .expect("disable tool");
        assert!(disabled.disabled_tools.contains("connector_tracker_search"));

        registry.invalidate_cache().expect("invalidate");
        let refreshed = registry
            .discover(vec![definition("one", "Tracker")], "credential", &base, 2)
            .await
            .expect("refresh");
        assert!(
            refreshed[0]
                .disabled_tools
                .contains("connector_tracker_search")
        );
        registry
            .register_tools(
                &tools,
                Arc::new(FakeBackend),
                PermissionStore::default(),
                Arc::new(Approve),
            )
            .expect("reregister");
        assert!(matches!(
            tools
                .invoke(
                    "connector_tracker_search",
                    ToolInvocation {
                        call_id: "disabled".to_owned(),
                        arguments: json!({}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn disconnected_connector_toggle_preserves_unavailable_state() {
        let registry = ConnectorRegistry::default();
        let base = Url::parse("https://api.example").expect("url");
        let mut connector = definition("one", "Tracker");
        connector.auth_kind = ConnectorAuthKind::OAuth;
        registry
            .discover(vec![connector], "credential", &base, 1)
            .await
            .expect("discover");
        let tools = ToolRegistry::default();
        registry
            .register_tools(
                &tools,
                Arc::new(FakeBackend),
                PermissionStore::default(),
                Arc::new(Approve),
            )
            .expect("register");

        registry
            .toggle("one", Some("connector_tracker_search"), false)
            .await
            .expect("disable");
        registry
            .toggle("one", Some("connector_tracker_search"), true)
            .await
            .expect("enable while disconnected");

        let tool = tools
            .list()
            .expect("tools")
            .into_iter()
            .find(|tool| tool.name == "connector_tracker_search")
            .expect("connector tool");
        assert_eq!(tool.availability, ToolAvailability::Unavailable);
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
