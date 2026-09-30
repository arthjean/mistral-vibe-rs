//! The tools a session's accepted connector catalog publishes, and the
//! gateway call behind each one.
//!
//! Reference `ConnectorRegistry._accept_catalog` and
//! `create_connector_proxy_tool_class`
//! (`vibe/core/tools/connectors/connector_registry.py`): every tool of a ready
//! connector is published as `connector_{alias}_{tool}`, described by the
//! connector's alias and the tool's own description, and runs one `tools/call`
//! through `{server}/v1/connectors-gateway/{id}/mcp` with the account's key.

use std::sync::Arc;

use serde_json::{Value, json};
use url::Url;
use vibe_core::events::RemoteToolOrigin;
use vibe_core::integrations::redact;
use vibe_core::mcp::{GatewayFailure, call_connector_gateway};
use vibe_core::policy::{PermissionContext, PolicyGuardedTool};
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolHandler, ToolInvocation,
    ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

use super::{CatalogProvider, ResolvedConnector, ResolvedTool, SessionConnectors};

impl SessionConnectors {
    /// Publishes the accepted catalog's tools into `tools`, replacing what an
    /// earlier catalog published.
    ///
    /// Reference `integrate_connectors_async` removes every connector tool
    /// and adds the registry's again, so they close the surface in catalog
    /// order. A tool the selection disables is not published, which is what
    /// reference `ToolManager` filtering leaves of it; a connector that is not
    /// ready publishes nothing.
    pub(crate) fn publish_tools(&self, tools: &ToolRegistry) -> Result<(), ToolError> {
        tools.retract_source(ToolSource::Connector)?;
        let (Some((catalog, selection)), Some(account), Some(guard)) =
            (&self.accepted, &self.account, tools.guard())
        else {
            return Ok(());
        };
        for connector in catalog
            .connectors
            .iter()
            .filter(|connector| connector.ready)
        {
            for tool in &connector.tools {
                if !selection.tool_enabled(&connector.alias, &tool.raw_name) {
                    continue;
                }
                let Some(handler) = gateway_handler(account, connector, tool) else {
                    continue;
                };
                let name = format!("connector_{}_{}", connector.alias, tool.raw_name);
                let guarded = Arc::new(PolicyGuardedTool::new(
                    name.clone(),
                    guard.policy.clone(),
                    guard.approval.clone(),
                    // Like an MCP tool, a connector tool declares no
                    // `resolve_permission`: its configured permission decides.
                    Arc::new(|_invocation| Ok(PermissionContext::deferred())),
                    handler,
                ));
                let spec = ToolSpec {
                    name,
                    description: format!(
                        "[{}] {}",
                        connector.alias,
                        tool.description
                            .clone()
                            .unwrap_or_else(|| format!("Connector tool '{}'", tool.raw_name))
                    ),
                    input_schema: if tool.input_schema.is_empty() {
                        json!({"type": "object", "properties": {}})
                    } else {
                        Value::Object(tool.input_schema.clone())
                    },
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Connector,
                    source: ToolSource::Connector,
                    selection_priority: 50,
                };
                // A tool the registry cannot publish is skipped, as reference
                // `_build_tools_for_connector` logs and skips one it cannot
                // build.
                let _ = tools.register_exclusive(
                    spec,
                    guarded,
                    format!("connector `{}` tool `{}`", connector.alias, tool.raw_name),
                    Some(RemoteToolOrigin::connector(&tool.raw_name)),
                );
            }
        }
        Ok(())
    }
}

/// The call one connector tool makes, or `None` when its gateway address does
/// not parse.
fn gateway_handler(
    account: &CatalogProvider,
    connector: &ResolvedConnector,
    tool: &ResolvedTool,
) -> Option<Arc<dyn ToolHandler>> {
    let url = Url::parse(&format!(
        "{}/v1/connectors-gateway/{}/mcp",
        account.base_url, connector.raw_id
    ))
    .ok()?;
    let api_key = account.api_key.clone();
    let remote = tool.raw_name.clone();
    let display_name = connector.display_name.clone();
    let connector_id = connector.raw_id.clone();
    Some(Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let url = url.clone();
            let api_key = api_key.clone();
            let remote = remote.clone();
            let display_name = display_name.clone();
            let connector_id = connector_id.clone();
            // Reference `_OpenArgs.model_dump(exclude_none=True)`.
            let arguments = match invocation.arguments.clone() {
                Value::Object(fields) => Value::Object(
                    fields
                        .into_iter()
                        .filter(|(_, value)| !value.is_null())
                        .collect(),
                ),
                other => other,
            };
            Box::pin(async move {
                call_connector_gateway(&url, &api_key, &remote, arguments)
                    .await
                    .map_err(|failure| {
                        ToolError::Execution(failure_message(
                            &failure,
                            &display_name,
                            &connector_id,
                        ))
                    })
            })
        },
    ))
}

/// What a failed gateway call tells the model, in this port's words for the
/// cases reference `_connector_error_message` distinguishes.
fn failure_message(failure: &GatewayFailure, display_name: &str, connector_id: &str) -> String {
    match failure {
        GatewayFailure::Status(status @ (401 | 403)) => format!(
            "The gateway refused the credentials for connector {display_name} \
             ({connector_id}) with HTTP {status}; verify the Mistral API key."
        ),
        GatewayFailure::Status(404) => format!(
            "The gateway has no connector {display_name} ({connector_id}) (HTTP 404); \
             it was removed or this account cannot reach it."
        ),
        GatewayFailure::Status(status) => format!(
            "The gateway failed the call to connector {display_name} ({connector_id}) \
             with HTTP {status}."
        ),
        GatewayFailure::Timeout => format!(
            "Connector {display_name} did not answer in time; the service behind it may be \
             slow or down."
        ),
        GatewayFailure::Unreachable(_) => format!(
            "The connector gateway could not be reached for {display_name}; verify the \
             network connection."
        ),
        GatewayFailure::Failed(message) => redact(&format!(
            "The call to connector {display_name} failed: {message}"
        )),
    }
}
