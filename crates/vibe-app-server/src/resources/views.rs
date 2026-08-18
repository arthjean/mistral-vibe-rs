//! The answer shapes a resource dispatch publishes.
//!
//! A method either reads state or moves it, and the two publish different
//! shapes: a read carries the state alone, a mutation also carries what it
//! diagnosed and, for the six that move runtime state, the runtime the server
//! composes afterward. Building them here keeps the service's methods about
//! what they do rather than about how the answer is spelled.

use super::*;

pub(super) fn read_only<const N: usize>(entries: [(&str, Value); N]) -> ResourceDispatch {
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
pub(super) fn canonical_mutation(
    key: &str,
    state: Value,
    diagnostics: Vec<String>,
) -> ResourceDispatch {
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

pub(super) fn runtime_mutation<const N: usize>(
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

pub(super) fn mcp_view(
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
pub(super) fn server_status(enabled: bool, status: McpServerStatus) -> McpSourceStatus {
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
pub(super) fn connector_status(enabled: bool, state: ConnectorAuthState) -> McpSourceStatus {
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

pub(super) fn connector_counts_value(views: &[ConnectorView]) -> Value {
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

pub(super) fn validate_auth_url(url: String, source: &str) -> Result<String, ResourceError> {
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

pub(super) fn resolve_connector(
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
