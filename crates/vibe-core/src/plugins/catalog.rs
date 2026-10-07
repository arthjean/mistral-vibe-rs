//! Connecting a plugin set's declared sources and cataloging their tools.
//!
//! Reference `vibe/core/plugins/_catalog.py`. Each plugin-owned MCP server is
//! discovered and each managed connector looked up; the tools they answer with
//! become tool groups and execution routes, with group names settled across
//! the whole set and colliding function names dropped and reported.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::{Value, json};

use super::canonical::{canonical_json, canonical_json_digest};
use super::compatibility::PluginToolOverride;
use super::diagnostics::{self as codes, PluginConfigIssue, Severity};
use super::materialize::{
    PluginToolCatalog, PluginToolDefinition, PluginToolGroup, PluginToolRoute,
};
use super::naming::{
    ToolGroupIdentity, plugin_mcp_group_name, resolve_tool_group_names, tool_function_name,
};
use super::native::{PluginDescriptor, PluginMcpServerDefinition, ResolvedPluginSet, python_repr};
use super::redaction::redact_failure;
use crate::hooks::python_json_dumps;
use crate::mcp::RemoteTool;

/// Why a plugin-owned MCP server published no catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMcpDiscoveryFailure {
    /// The server lists nothing until it is authorized.
    AuthorizationRequired,
    /// The server did not answer; the text is the client's.
    Failed(String),
}

/// A boxed discovery in flight.
pub type DiscoveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<RemoteTool>, PluginMcpDiscoveryFailure>> + Send + 'a>>;

/// Connects one plugin-owned MCP server and reports its tools. Reference
/// `PluginMCPDiscovery`.
pub trait PluginMcpDiscovery: Send + Sync {
    fn discover<'a>(&'a self, definition: &'a PluginMcpServerDefinition) -> DiscoveryFuture<'a>;
}

/// One tool a managed connector offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// The managed connectors available to the account. Reference
/// `PluginConnectorCatalog`.
pub trait PluginConnectorCatalog: Send + Sync {
    /// `None` when the connector is not available to this account.
    fn tools(&self, source_id: &str) -> Option<Vec<ConnectorTool>>;
}

#[derive(Debug, Clone)]
struct Candidate {
    kind: &'static str,
    plugin_name: String,
    group_name: String,
    function_name: String,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    exposure: String,
    source_id: String,
    source_tool_name: String,
    execution_name: String,
    schema_fingerprint: String,
    config_file: PathBuf,
}

impl Candidate {
    fn source(&self) -> (String, String) {
        (self.plugin_name.clone(), self.source_id.clone())
    }

    fn qualified_name(&self) -> String {
        format!("{}.{}", self.group_name, self.function_name)
    }
}

/// Reference `build_tool_catalog`.
pub async fn build_tool_catalog(
    resolution: &ResolvedPluginSet,
    mcp_discovery: Option<&dyn PluginMcpDiscovery>,
    connector_catalog: Option<&dyn PluginConnectorCatalog>,
    issues: &mut Vec<PluginConfigIssue>,
) -> PluginToolCatalog {
    let plugins: BTreeMap<&str, &PluginDescriptor> = resolution
        .plugins
        .iter()
        .map(|plugin| (plugin.name.as_str(), plugin))
        .collect();
    let connected = connect_servers(&resolution.mcp_servers, &plugins, mcp_discovery, issues).await;
    let mcp_sources: BTreeSet<(String, String)> = connected
        .iter()
        .map(|(definition, _)| (definition.plugin_name.clone(), definition.source_id.clone()))
        .collect();
    let mut candidates = mcp_candidates(&connected, &plugins, issues);
    let (connector_candidates, connector_sources) =
        connector_candidates(resolution, &plugins, connector_catalog, issues);
    candidates.extend(connector_candidates);
    let candidates = apply_group_names(candidates, resolution);
    let selected = drop_name_collisions(candidates, &plugins, issues);
    let answered: BTreeSet<(String, String)> =
        mcp_sources.union(&connector_sources).cloned().collect();
    report_unused_overrides(&resolution.plugins, &answered, &selected, issues);

    let mut by_group: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
    for candidate in selected {
        by_group
            .entry((candidate.plugin_name.clone(), candidate.group_name.clone()))
            .or_default()
            .push(candidate);
    }
    let mut catalog = PluginToolCatalog {
        connected_mcp_sources: mcp_sources,
        connected_connector_sources: connector_sources,
        ..PluginToolCatalog::default()
    };
    for ((plugin_name, group_name), mut members) in by_group {
        members.sort_by(|left, right| left.function_name.cmp(&right.function_name));
        catalog.tool_groups.push(PluginToolGroup {
            plugin_name: plugin_name.clone(),
            name: group_name.clone(),
            description: plugins
                .get(plugin_name.as_str())
                .map(|plugin| plugin.description.clone())
                .unwrap_or_default(),
            tools: members
                .iter()
                .map(|candidate| PluginToolDefinition {
                    name: candidate.function_name.clone(),
                    description: candidate.description.clone(),
                    input_schema: candidate.input_schema.clone(),
                    output_schema: candidate.output_schema.clone(),
                    exposure: candidate.exposure.clone(),
                })
                .collect(),
        });
        for candidate in members {
            catalog.tool_routes.insert(
                (group_name.clone(), candidate.function_name.clone()),
                PluginToolRoute {
                    plugin_name: plugin_name.clone(),
                    group_name: group_name.clone(),
                    function_name: candidate.function_name,
                    source_kind: candidate.kind.to_owned(),
                    source_id: candidate.source_id,
                    source_tool_name: candidate.source_tool_name,
                    execution_name: candidate.execution_name,
                    schema_fingerprint: candidate.schema_fingerprint,
                },
            );
        }
    }
    catalog
}

async fn connect_servers(
    definitions: &[PluginMcpServerDefinition],
    plugins: &BTreeMap<&str, &PluginDescriptor>,
    discovery: Option<&dyn PluginMcpDiscovery>,
    issues: &mut Vec<PluginConfigIssue>,
) -> Vec<(PluginMcpServerDefinition, Vec<RemoteTool>)> {
    let owned: Vec<&PluginMcpServerDefinition> = definitions
        .iter()
        .filter(|definition| plugins.contains_key(definition.plugin_name.as_str()))
        .collect();
    let Some(discovery) = discovery.filter(|_| !owned.is_empty()) else {
        return Vec::new();
    };
    let results = futures_util::future::join_all(
        owned
            .iter()
            .map(|definition| discovery.discover(definition)),
    )
    .await;
    let mut connected = Vec::new();
    for (definition, result) in owned.into_iter().zip(results) {
        let Some(plugin) = plugins.get(definition.plugin_name.as_str()) else {
            continue;
        };
        match result {
            Ok(tools) => connected.push((definition.clone(), tools)),
            Err(PluginMcpDiscoveryFailure::AuthorizationRequired) => issues.push(issue(
                plugin,
                definition.config_file.clone(),
                codes::MCP_AUTHORIZATION_REQUIRED,
                format!(
                    "The MCP server {} publishes no tools until it is authorized.",
                    python_repr(&definition.source_id)
                ),
                "mcp_server",
            )),
            Err(PluginMcpDiscoveryFailure::Failed(message)) => {
                let secrets = definition.server.secrets();
                let detail = redact_failure(&message, secrets.iter().map(String::as_str));
                issues.push(issue(
                    plugin,
                    definition.config_file.clone(),
                    codes::MCP_CONNECTION_FAILED,
                    format!(
                        "The MCP server {} could not connect: {detail}",
                        python_repr(&definition.source_id)
                    ),
                    "mcp_server",
                ));
            }
        }
    }
    connected
}

fn mcp_candidates(
    connected: &[(PluginMcpServerDefinition, Vec<RemoteTool>)],
    plugins: &BTreeMap<&str, &PluginDescriptor>,
    issues: &mut Vec<PluginConfigIssue>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for (definition, tools) in connected {
        let Some(plugin) = plugins.get(definition.plugin_name.as_str()) else {
            continue;
        };
        let mut tools: Vec<&RemoteTool> = tools.iter().collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        for tool in tools {
            let Some((input_schema, output_schema)) =
                digestible_schemas(&tool.input_schema, tool.output_schema.as_ref())
            else {
                issues.push(issue(
                    plugin,
                    definition.config_file.clone(),
                    codes::MCP_TOOL_SCHEMA_INVALID,
                    format!(
                        "The MCP tool {} published an invalid JSON schema.",
                        dumps(&tool.name)
                    ),
                    "tool",
                ));
                continue;
            };
            let override_ = plugin
                .tool_overrides
                .get(&format!("{}/{}", definition.source_id, tool.name));
            candidates.push(Candidate {
                kind: "mcp",
                plugin_name: plugin.name.clone(),
                group_name: plugin_mcp_group_name(
                    &definition.plugin_namespace,
                    &definition.source_id,
                ),
                function_name: tool_function_name(
                    &tool.name,
                    override_.and_then(|item| item.name.as_deref()),
                ),
                description: match tool.description.as_deref() {
                    Some(description) if !description.is_empty() => description.to_owned(),
                    _ => format!(
                        "MCP tool {} from {}.",
                        dumps(&tool.name),
                        definition.source_id
                    ),
                },
                schema_fingerprint: schema_fingerprint(
                    &tool.name,
                    &input_schema,
                    output_schema.as_ref(),
                ),
                input_schema,
                output_schema,
                exposure: exposure(override_),
                source_id: definition.source_id.clone(),
                source_tool_name: tool.name.clone(),
                execution_name: format!("{}/{}/{}", plugin.name, definition.source_id, tool.name),
                config_file: definition.config_file.clone(),
            });
        }
    }
    candidates
}

fn connector_candidates(
    resolution: &ResolvedPluginSet,
    plugins: &BTreeMap<&str, &PluginDescriptor>,
    catalog: Option<&dyn PluginConnectorCatalog>,
    issues: &mut Vec<PluginConfigIssue>,
) -> (Vec<Candidate>, BTreeSet<(String, String)>) {
    let mut candidates = Vec::new();
    let mut answered = BTreeSet::new();
    for definition in &resolution.connectors {
        let Some(plugin) = plugins.get(definition.plugin_name.as_str()) else {
            continue;
        };
        let Some(catalog) = catalog else {
            issues.push(issue(
                plugin,
                definition.config_file.clone(),
                codes::CONNECTOR_RUNTIME_UNAVAILABLE,
                format!(
                    "The managed connector {} cannot load because this session has no connector registry.",
                    dumps(&definition.source_id)
                ),
                "connector",
            ));
            continue;
        };
        let Some(available) = catalog.tools(&definition.source_id) else {
            issues.push(issue(
                plugin,
                definition.config_file.clone(),
                codes::CONNECTOR_UNAVAILABLE,
                format!(
                    "The managed connector {} is not available to this account.",
                    dumps(&definition.source_id)
                ),
                "connector",
            ));
            continue;
        };
        answered.insert((definition.plugin_name.clone(), definition.source_id.clone()));
        let by_name: BTreeMap<&str, &ConnectorTool> = available
            .iter()
            .map(|tool| (tool.name.as_str(), tool))
            .collect();
        for source_tool_name in &definition.tools {
            let Some(tool) = by_name.get(source_tool_name.as_str()) else {
                issues.push(issue(
                    plugin,
                    definition.config_file.clone(),
                    codes::CONNECTOR_TOOL_UNAVAILABLE,
                    format!(
                        "The managed connector {} offers no tool {}.",
                        dumps(&definition.source_id),
                        dumps(source_tool_name)
                    ),
                    "connector",
                ));
                continue;
            };
            let Some((input_schema, output_schema)) = digestible_schemas(&tool.input_schema, None)
            else {
                issues.push(issue(
                    plugin,
                    definition.config_file.clone(),
                    codes::CONNECTOR_TOOL_SCHEMA_INVALID,
                    format!(
                        "The managed connector tool {} published an invalid JSON schema.",
                        dumps(source_tool_name)
                    ),
                    "connector",
                ));
                continue;
            };
            let override_ = plugin
                .tool_overrides
                .get(&format!("{}/{source_tool_name}", definition.source_id));
            candidates.push(Candidate {
                kind: "connector",
                plugin_name: plugin.name.clone(),
                group_name: plugin.namespace.clone(),
                function_name: tool_function_name(
                    source_tool_name,
                    override_.and_then(|item| item.name.as_deref()),
                ),
                description: match tool.description.as_deref() {
                    Some(description) if !description.is_empty() => description.to_owned(),
                    _ => format!(
                        "Connector tool {} from {}.",
                        dumps(source_tool_name),
                        definition.source_id
                    ),
                },
                schema_fingerprint: schema_fingerprint(
                    source_tool_name,
                    &input_schema,
                    output_schema.as_ref(),
                ),
                input_schema,
                output_schema,
                exposure: exposure(override_),
                source_id: definition.source_id.clone(),
                source_tool_name: source_tool_name.clone(),
                execution_name: format!(
                    "{}/connector/{}/{source_tool_name}",
                    plugin.name, definition.source_id
                ),
                config_file: definition.config_file.clone(),
            });
        }
    }
    (candidates, answered)
}

fn apply_group_names(candidates: Vec<Candidate>, resolution: &ResolvedPluginSet) -> Vec<Candidate> {
    let mut identities: BTreeMap<(String, String), ToolGroupIdentity> = BTreeMap::new();
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.kind == "mcp")
    {
        identities.insert(
            candidate.source(),
            ToolGroupIdentity {
                plugin_name: candidate.plugin_name.clone(),
                base_name: candidate.group_name.clone(),
                source_id: candidate.source_id.clone(),
            },
        );
    }
    let resolved = resolve_tool_group_names(
        identities.values(),
        resolution
            .plugins
            .iter()
            .map(|plugin| plugin.namespace.clone()),
    );
    candidates
        .into_iter()
        .map(|mut candidate| {
            if candidate.kind == "mcp"
                && let Some(name) = identities
                    .get(&candidate.source())
                    .and_then(|identity| resolved.get(identity))
            {
                candidate.group_name.clone_from(name);
            }
            candidate
        })
        .collect()
}

fn drop_name_collisions(
    candidates: Vec<Candidate>,
    plugins: &BTreeMap<&str, &PluginDescriptor>,
    issues: &mut Vec<PluginConfigIssue>,
) -> Vec<Candidate> {
    let mut by_name: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates {
        by_name
            .entry((
                candidate.group_name.clone(),
                candidate.function_name.clone(),
            ))
            .or_default()
            .push(candidate);
    }
    let mut selected = Vec::new();
    for ((group_name, function_name), mut matches) in by_name {
        if matches.len() == 1 {
            selected.append(&mut matches);
            continue;
        }
        let Some(plugin) = plugins.get(matches[0].plugin_name.as_str()) else {
            continue;
        };
        let mut sources: Vec<String> = matches
            .iter()
            .map(|item| format!("{}/{}", item.source_id, item.source_tool_name))
            .collect();
        sources.sort();
        issues.push(issue(
            plugin,
            matches[0].config_file.clone(),
            codes::TOOL_NAME_COLLISION,
            format!(
                "The tools {} of plugin {} share the name {} in group {}; declare toolOverrides to separate them.",
                sources.join(", "),
                dumps(&plugin.name),
                dumps(&function_name),
                dumps(&group_name)
            ),
            "tool",
        ));
    }
    selected.sort_by_key(Candidate::qualified_name);
    selected
}

fn report_unused_overrides(
    descriptors: &[PluginDescriptor],
    answered: &BTreeSet<(String, String)>,
    selected: &[Candidate],
    issues: &mut Vec<PluginConfigIssue>,
) {
    let discovered: BTreeSet<(&str, &str, &str)> = selected
        .iter()
        .map(|candidate| {
            (
                candidate.plugin_name.as_str(),
                candidate.source_id.as_str(),
                candidate.source_tool_name.as_str(),
            )
        })
        .collect();
    for plugin in descriptors {
        for key in plugin.tool_overrides.keys() {
            let (source_id, source_tool_name) = key.split_once('/').unwrap_or((key.as_str(), ""));
            if source_id.is_empty()
                || !answered.contains(&(plugin.name.clone(), source_id.to_owned()))
            {
                continue;
            }
            if discovered.contains(&(plugin.name.as_str(), source_id, source_tool_name)) {
                continue;
            }
            issues.push(issue(
                plugin,
                plugin.manifest_path.clone(),
                codes::TOOL_OVERRIDE_UNUSED,
                format!("The tool override {} matches no tool.", dumps(key)),
                "tool",
            ));
        }
    }
}

fn exposure(override_: Option<&PluginToolOverride>) -> String {
    override_
        .and_then(|item| item.exposure.clone())
        .unwrap_or_else(|| "programmatic".to_owned())
}

/// Both schemas as JSON objects canonical JSON can carry, or `None`.
/// Reference `_digestible_schemas`.
fn digestible_schemas(input: &Value, output: Option<&Value>) -> Option<(Value, Option<Value>)> {
    if !input.is_object() || output.is_some_and(|output| !output.is_object() && !output.is_null()) {
        return None;
    }
    canonical_json(input).ok()?;
    let output = output.filter(|value| !value.is_null()).cloned();
    if let Some(output) = &output {
        canonical_json(output).ok()?;
    }
    Some((input.clone(), output))
}

/// Reference `_schema_fingerprint`.
#[must_use]
pub fn schema_fingerprint(
    name: &str,
    input_schema: &Value,
    output_schema: Option<&Value>,
) -> String {
    canonical_json_digest(&json!({
        "name": name,
        "inputSchema": input_schema,
        "outputSchema": output_schema.cloned().unwrap_or(Value::Null),
    }))
    .unwrap_or_default()
}

fn dumps(text: &str) -> String {
    python_json_dumps(&Value::String(text.to_owned()))
}

fn issue(
    plugin: &PluginDescriptor,
    file: PathBuf,
    code: &str,
    message: String,
    component: &str,
) -> PluginConfigIssue {
    PluginConfigIssue::coded(file, message, code, Some(plugin.source_format), component)
        .severity(Severity::Warning)
}
