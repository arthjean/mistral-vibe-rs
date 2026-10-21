//! The tools a healthy server publishes, and the handler each one runs through.

use crate::policy::{PermissionContext, PolicyGuardedTool};
use crate::remote_tools::public_tool_name;
use crate::tools::{
    ToolError, ToolExecutionOutput, ToolHandler, ToolInvocation, ToolOutputSink,
    ToolPresentationKind, ToolSpec,
};

use super::credentials::{Authorized, authorized, publish_required};
use super::*;

#[cfg(test)]
mod tools_tests;

/// Registers a healthy server's tools and builds the view the host reads of
/// it, with the diagnostics the registration produced.
pub(super) fn healthy_view(
    state: &Arc<Mutex<McpRegistryState>>,
    config: &McpServerConfig,
    peer: Arc<dyn McpPeer>,
    descriptors: Vec<RemoteTool>,
    runtime: &McpRuntime,
    published: &mut Vec<String>,
) -> (McpServerView, Vec<String>) {
    let alias = config.alias.as_str();
    let (published_tools, mut diagnostics) =
        register_remote_tools(state, config, peer, descriptors, runtime);
    let registered = published_tools
        .iter()
        .map(|(public, _)| public.clone())
        .collect::<Vec<_>>();
    published.extend(registered.iter().cloned());
    let requested = normalize_disabled_tools(alias, &registered, &config.disabled_tools);
    let disabled_tools = match reconcile_disabled_tools(&runtime.tools, &registered, requested) {
        Ok(disabled) => disabled,
        Err(error) => {
            diagnostics.push(canonical_diagnostic(alias, &error));
            BTreeSet::new()
        }
    };
    let view = McpServerView {
        alias: alias.to_owned(),
        transport: transport_name(&config.transport).to_owned(),
        enabled: true,
        status: McpServerStatus::Healthy,
        tools: registered,
        disabled_tools,
        diagnostic: diagnostics.first().cloned(),
        remote_names: published_tools.into_iter().collect(),
    };
    (view, diagnostics)
}

fn reconcile_disabled_tools(
    tools: &ToolRegistry,
    registered: &[String],
    disabled: BTreeSet<String>,
) -> Result<BTreeSet<String>, McpError> {
    let disabled = disabled
        .into_iter()
        .filter(|tool| registered.contains(tool))
        .collect::<BTreeSet<_>>();
    let names = disabled.iter().cloned().collect::<Vec<_>>();
    if !set_all(tools, ToolSource::Mcp, &names, ToolAvailability::Disabled) {
        return Err(McpError::Tool(
            "MCP tool registry changed during reconciliation".to_owned(),
        ));
    }
    Ok(disabled)
}

/// Resolves persisted per-tool disable entries onto the names published today.
///
/// An entry names a remote tool, as reference `ToolManager._is_source_disabled`
/// reads it (`vibe/core/tools/manager.py`), and that reading wins. Only an
/// entry naming no remote tool of the server falls back to this port's own
/// spellings: the published name its toggle persists, then the
/// `mcp_{alias}_{tool}` name it published before it adopted the reference
/// rule, so a preference an older build wrote keeps disabling its tool.
fn normalize_disabled_tools(
    alias: &str,
    registered: &[String],
    configured: &BTreeSet<String>,
) -> BTreeSet<String> {
    configured
        .iter()
        .filter_map(|tool| {
            let public = public_tool_name(ToolSource::Mcp, alias, tool);
            if registered.contains(&public) {
                return Some(public);
            }
            if registered.contains(tool) {
                return Some(tool.clone());
            }
            tool.strip_prefix("mcp_")
                .filter(|migrated| registered.contains(&(*migrated).to_owned()))
                .map(str::to_owned)
        })
        .collect()
}

/// The description a published tool carries, reference
/// `create_mcp_http_proxy_tool_class` and `create_mcp_stdio_proxy_tool_class`:
/// the server's name in brackets, the tool's own description or a sentence
/// naming where it comes from, and the server's usage hint.
fn published_description(config: &McpServerConfig, remote: &RemoteTool) -> String {
    let description = remote
        .description
        .clone()
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| match &config.transport {
            McpTransportConfig::Stdio {
                command, arguments, ..
            } => {
                let mut argv = vec![command.as_str()];
                argv.extend(arguments.iter().map(String::as_str));
                format!(
                    "MCP tool '{}' from stdio command: {}",
                    remote.name,
                    argv.join(" ")
                )
            }
            McpTransportConfig::Http { .. } | McpTransportConfig::StreamableHttp { .. } => {
                format!("MCP tool '{}' from {}", remote.name, config.declared_url())
            }
        });
    let hint = config
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
        .map(|prompt| format!("\nHint: {prompt}"))
        .unwrap_or_default();
    format!("[{}] {description}{hint}", config.alias)
}

/// Registers every valid tool `remote_tools` lists: the public and remote
/// name of each one registered, and a diagnostic for each one refused.
fn register_remote_tools(
    state: &Arc<Mutex<McpRegistryState>>,
    config: &McpServerConfig,
    peer: Arc<dyn McpPeer>,
    remote_tools: Vec<RemoteTool>,
    runtime: &McpRuntime,
) -> (Vec<(String, String)>, Vec<String>) {
    let alias = config.alias.as_str();
    let mut registered = Vec::new();
    let mut diagnostics = Vec::new();
    for remote in remote_tools {
        let public_name = public_tool_name(ToolSource::Mcp, alias, &remote.name);
        if let Err(error) = validate_remote_tool(&remote, &config.transport) {
            diagnostics.push(canonical_diagnostic(alias, &error));
            continue;
        }
        let description = published_description(config, &remote);
        let handler: Arc<dyn ToolHandler> = Arc::new(McpToolHandler {
            state: state.clone(),
            alias: alias.to_owned(),
            remote_name: remote.name.clone(),
            config: config.clone(),
            peer: peer.clone(),
        });
        let origin = format!("MCP server `{alias}` tool `{}`", remote.name);
        let guarded = Arc::new(PolicyGuardedTool::new(
            public_name.clone(),
            runtime.policy.clone(),
            runtime.approval.clone(),
            // The reference publishes an MCP tool through the same base class as
            // a builtin and declares no `resolve_permission` for it, so its
            // configured permission is the whole decision and an approval for
            // the session grants the tool itself. There is no fifth scope to
            // name a server under, and inventing one would be a value the Python
            // client cannot read.
            Arc::new(|_invocation| Ok(PermissionContext::deferred())),
            handler,
        ));
        let spec = ToolSpec {
            name: public_name.clone(),
            description,
            input_schema: remote.input_schema,
            // The server's output schema is checked by the client session
            // against the structured content; what the tool returns here is
            // reference `MCPToolResult`, which that schema does not describe.
            output_schema: None,
            config: Value::Null,
            state: Value::Null,
            availability: ToolAvailability::Available,
            presentation: ToolPresentationKind::Mcp,
            source: ToolSource::Mcp,
            selection_priority: 50,
        };
        match runtime.tools.register_exclusive(
            spec,
            guarded,
            origin,
            Some(crate::events::RemoteToolOrigin::mcp(&remote.name).from_provider(alias)),
        ) {
            Ok(_) => registered.push((public_name, remote.name)),
            Err(error) => diagnostics.push(canonical_diagnostic(
                alias,
                &McpError::Tool(error.to_string()),
            )),
        }
    }
    (registered, diagnostics)
}

/// One published tool, reference `MCPHttpProxyTool` or `MCPStdioProxyTool`.
struct McpToolHandler {
    state: Arc<Mutex<McpRegistryState>>,
    alias: String,
    remote_name: String,
    config: McpServerConfig,
    peer: Arc<dyn McpPeer>,
}

impl ToolHandler for McpToolHandler {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        _output: ToolOutputSink,
    ) -> crate::tools::ToolHandlerFuture<'a> {
        Box::pin(async move {
            let (mut epoch, setup) = {
                let state = self.state.lock().await;
                let server = state
                    .servers
                    .get(&self.alias)
                    .filter(|server| {
                        server.published.as_ref().is_some_and(|published| {
                            published.view.enabled
                                && published.view.status == McpServerStatus::Healthy
                        }) && server
                            .peer
                            .as_ref()
                            .is_some_and(|active| Arc::ptr_eq(active, &self.peer))
                    })
                    .ok_or_else(|| {
                        ToolError::Unavailable(format!("MCP server `{}` is disabled", self.alias))
                    })?;
                (server.epoch.subscribe(), state.authorization.clone())
            };
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
            tokio::select! {
                biased;
                changed = epoch.changed() => {
                    let _ = changed;
                    Err(ToolError::Unavailable(format!(
                        "MCP server `{}` changed while the tool was running",
                        self.alias
                    )))
                }
                result = self.call(arguments, setup.as_ref()) => result,
            }
        })
    }
}

impl McpToolHandler {
    async fn call(
        &self,
        arguments: Value,
        setup: Option<&AuthorizationSetup>,
    ) -> Result<ToolExecutionOutput, ToolError> {
        let failed = |error: &McpError| {
            ToolError::Execution(crate::integrations::redact(&format!(
                "MCP server `{}` could not run `{}`: {error}",
                self.alias, self.remote_name
            )))
        };
        let reference = setup.and_then(|setup| setup.references.get(&self.alias));
        let (
            Some(setup),
            Some(reference),
            McpAuthorizationKind::Static | McpAuthorizationKind::Oauth,
        ) = (setup, reference, authorization_kind(&self.config))
        else {
            let headers = authorization::http_headers(&self.config);
            return self
                .peer
                .call(&self.remote_name, arguments, &headers)
                .await
                .map_err(|error| failed(&error));
        };
        // Reference `_call_authorized`.
        let snapshot = match setup.service.resolve(reference).await {
            McpAuthorization::Snapshot(snapshot) => snapshot,
            McpAuthorization::Required(required) => {
                publish_required(&self.state, Some(setup), &self.alias, &required).await;
                return Err(ToolError::Execution(format!(
                    "MCP server `{}` has to be signed in to again before its tools can run",
                    self.alias
                )));
            }
        };
        let call = |headers: BTreeMap<String, String>| -> McpFuture<'_, ToolExecutionOutput> {
            let arguments = arguments.clone();
            Box::pin(async move { self.peer.call(&self.remote_name, arguments, &headers).await })
        };
        match authorized(Some(setup), reference, snapshot, call).await {
            Authorized::Done(output, _) => Ok(output),
            Authorized::Failed(error) => Err(failed(&error)),
            Authorized::Refused(required) => {
                publish_required(&self.state, Some(setup), &self.alias, &required).await;
                Err(ToolError::Execution(format!(
                    "MCP server `{}` refused the credentials it was given",
                    self.alias
                )))
            }
        }
    }
}
