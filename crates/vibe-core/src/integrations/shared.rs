use std::collections::BTreeSet;

use serde_json::Value;
use thiserror::Error;

use super::{
    ConnectorAuthState, ConnectorDefinition, ConnectorTool, ConnectorView, MAX_CONNECTOR_TOOLS,
};
pub(super) use crate::remote_tools::normalize_alias;
use crate::remote_tools::{ProviderReach, public_tool_name, tool_availability};
use crate::text::truncate_utf8;
use crate::tools::{ToolAvailability, ToolPresentationKind, ToolSource, ToolSpec};

const MAX_PUBLIC_LOG_MESSAGE: usize = 2_048;

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

pub fn redact(message: &str) -> String {
    let bounded = truncate_utf8(message, MAX_PUBLIC_LOG_MESSAGE);
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
    bounded.to_owned()
}

pub(super) fn push_bounded<T>(values: &mut Vec<T>, limit: usize, value: T) {
    if values.len() == limit {
        values.remove(0);
    }
    values.push(value);
}

pub(super) fn validate_definition(
    definition: &ConnectorDefinition,
) -> Result<(), IntegrationError> {
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
            || tool
                .input_schema
                .get("type")
                .and_then(serde_json::Value::as_str)
                != Some("object")
    }) {
        return Err(IntegrationError::InvalidConnector(
            "tool names and object schemas are required".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn connector_tool_spec(view: &ConnectorView, remote: &ConnectorTool) -> ToolSpec {
    let name = public_tool_name(ToolSource::Connector, &view.alias, &remote.name);
    ToolSpec {
        availability: connector_tool_availability(view, &name),
        name,
        description: remote.description.clone(),
        input_schema: remote.input_schema.clone(),
        output_schema: remote.output_schema.clone(),
        config: Value::Null,
        state: Value::Null,
        presentation: ToolPresentationKind::Connector,
        source: ToolSource::Connector,
        selection_priority: 40,
    }
}

pub(super) fn validate_connector_tool_specs(
    view: &ConnectorView,
    definition: &ConnectorDefinition,
) -> Result<(), IntegrationError> {
    let mut names = BTreeSet::new();
    for remote in &definition.tools {
        let spec = connector_tool_spec(view, remote);
        if !names.insert(spec.name.clone()) {
            return Err(IntegrationError::InvalidConnector(format!(
                "connector `{}` has duplicate normalized tool names",
                definition.id
            )));
        }
        spec.validate()
            .map_err(|error| IntegrationError::InvalidConnector(error.to_string()))?;
    }
    Ok(())
}

pub(super) fn connector_availability_updates(
    view: &ConnectorView,
    names: Vec<String>,
) -> Vec<(String, ToolAvailability)> {
    names
        .into_iter()
        .map(|name| {
            let availability = connector_tool_availability(view, &name);
            (name, availability)
        })
        .collect()
}

pub(super) fn connector_tool_availability(view: &ConnectorView, name: &str) -> ToolAvailability {
    let reach = if matches!(
        view.auth_state,
        ConnectorAuthState::Connected | ConnectorAuthState::NotRequired
    ) {
        ProviderReach::Ready
    } else {
        ProviderReach::Unreachable
    };
    tool_availability(view.enabled, &view.disabled_tools, reach, name)
}
