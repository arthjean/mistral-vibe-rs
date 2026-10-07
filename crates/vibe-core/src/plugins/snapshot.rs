//! The portable snapshot a resolved plugin set projects to.
//!
//! Reference `vibe/core/plugins/_snapshot.py` for the model and
//! `vibe/core/plugins/_adapter.py` for the projection. Only identity and the
//! catalog travel: host paths become [`PluginPathRef`]s, and late-bound values
//! (hook environment values, MCP definitions, credentials) stay out, so two
//! resolves of one tree produce one byte string.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canonical::{
    CanonicalJsonError, canonical_json, normalize_json, normalize_nfc, sha256_hex,
};
use super::compatibility::DetectedPluginFormat;
use super::materialize::MaterializedPluginSet;
use super::native::{PluginDescriptor, ResolvedPluginSet};
use super::paths::{PluginPathRef, plugin_path_ref};

/// The hook environment names the host injects, which a snapshot omits.
pub const HOST_HOOK_ENVIRONMENT: [&str; 2] = ["PLUGIN_ROOT", "PLUGIN_DATA"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSnapshotEntry {
    pub name: String,
    pub namespace: String,
    #[serde(default)]
    pub version: Option<String>,
    pub source_format: String,
    pub manifest_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSkillSnapshot {
    pub plugin_name: String,
    pub name: String,
    pub description: String,
    pub path: PluginPathRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginKnowledgeSnapshot {
    pub plugin_name: String,
    pub name: String,
    pub source_name: String,
    pub description: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub path: PluginPathRef,
    pub entrypoint: PluginPathRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAgentSnapshot {
    pub plugin_name: String,
    pub name: String,
    pub source_name: String,
    pub display_name: String,
    pub description: String,
    pub path: PluginPathRef,
    pub safety: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub overrides: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHookSnapshot {
    pub plugin_name: String,
    pub declared_name: String,
    pub source: String,
    pub protocol: String,
    pub visibility: String,
    pub order: usize,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub config_file: Option<PluginPathRef>,
    #[serde(default)]
    pub cwd: Option<PluginPathRef>,
    #[serde(default)]
    pub environment_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginLibrarySnapshot {
    pub plugin_name: String,
    pub language: String,
    pub alias: String,
    pub source_path: PluginPathRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConnectorSnapshot {
    pub plugin_name: String,
    pub source_id: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolSnapshot {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Value,
    pub exposure: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolGroupSnapshot {
    pub plugin_name: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Vec<PluginToolSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolRouteSnapshot {
    pub plugin_name: String,
    pub group_name: String,
    pub function_name: String,
    /// `mcp` or `connector`.
    pub source_kind: String,
    pub source_id: String,
    pub source_tool_name: String,
    pub execution_name: String,
    pub schema_fingerprint: String,
}

impl PluginToolRouteSnapshot {
    /// The `(group, function)` pair routes are keyed by.
    #[must_use]
    pub fn key(&self) -> (String, String) {
        (self.group_name.clone(), self.function_name.clone())
    }
}

/// Reference `ResolvedPluginSnapshot`, version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPluginSnapshot {
    pub version: u8,
    #[serde(default)]
    pub plugins: Vec<PluginSnapshotEntry>,
    #[serde(default)]
    pub skills: Vec<PluginSkillSnapshot>,
    #[serde(default)]
    pub knowledge: Vec<PluginKnowledgeSnapshot>,
    #[serde(default)]
    pub agents: Vec<PluginAgentSnapshot>,
    #[serde(default)]
    pub hooks: Vec<PluginHookSnapshot>,
    #[serde(default)]
    pub libraries: Vec<PluginLibrarySnapshot>,
    #[serde(default)]
    pub connectors: Vec<PluginConnectorSnapshot>,
    #[serde(default)]
    pub tool_groups: Vec<PluginToolGroupSnapshot>,
    #[serde(default)]
    pub tool_routes: Vec<PluginToolRouteSnapshot>,
}

impl Default for ResolvedPluginSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            plugins: Vec::new(),
            skills: Vec::new(),
            knowledge: Vec::new(),
            agents: Vec::new(),
            hooks: Vec::new(),
            libraries: Vec::new(),
            connectors: Vec::new(),
            tool_groups: Vec::new(),
            tool_routes: Vec::new(),
        }
    }
}

/// A component or path naming a plugin the snapshot does not list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("snapshot references unknown plugins: {names:?}")]
pub struct PluginSnapshotIdentityError {
    pub names: Vec<String>,
}

/// Orders every collection canonically. Reference `build_plugin_snapshot`.
///
/// # Errors
///
/// A component that names a plugin `entries` does not hold.
pub fn build_plugin_snapshot(
    mut snapshot: ResolvedPluginSnapshot,
) -> Result<ResolvedPluginSnapshot, PluginSnapshotIdentityError> {
    snapshot.version = 1;
    snapshot
        .plugins
        .sort_by(|left, right| left.name.cmp(&right.name));
    snapshot
        .skills
        .sort_by(|left, right| left.name.cmp(&right.name));
    snapshot
        .knowledge
        .sort_by(|left, right| left.name.cmp(&right.name));
    snapshot
        .agents
        .sort_by(|left, right| left.name.cmp(&right.name));
    snapshot
        .libraries
        .sort_by(|left, right| (&left.language, &left.alias).cmp(&(&right.language, &right.alias)));
    snapshot.connectors.sort_by(|left, right| {
        (&left.plugin_name, &left.source_id).cmp(&(&right.plugin_name, &right.source_id))
    });
    snapshot
        .tool_groups
        .sort_by(|left, right| left.name.cmp(&right.name));
    for group in &mut snapshot.tool_groups {
        group
            .tools
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
    snapshot
        .tool_routes
        .sort_by(|left, right| route_sort_key(left).cmp(&route_sort_key(right)));
    validate_resolved_plugin_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn route_sort_key(route: &PluginToolRouteSnapshot) -> [&str; 5] {
    [
        &route.plugin_name,
        &route.group_name,
        &route.function_name,
        &route.source_id,
        &route.source_tool_name,
    ]
}

/// Every component, and every path it points at, names a listed plugin.
/// Reference `validate_resolved_plugin_snapshot`.
///
/// # Errors
///
/// The unknown names, sorted.
pub fn validate_resolved_plugin_snapshot(
    snapshot: &ResolvedPluginSnapshot,
) -> Result<(), PluginSnapshotIdentityError> {
    let known: BTreeSet<&str> = snapshot
        .plugins
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let mut referenced: Vec<&str> = Vec::new();
    for skill in &snapshot.skills {
        referenced.extend([skill.plugin_name.as_str(), skill.path.plugin.as_str()]);
    }
    for folder in &snapshot.knowledge {
        referenced.extend([
            folder.plugin_name.as_str(),
            folder.path.plugin.as_str(),
            folder.entrypoint.plugin.as_str(),
        ]);
    }
    for agent in &snapshot.agents {
        referenced.extend([agent.plugin_name.as_str(), agent.path.plugin.as_str()]);
    }
    for hook in &snapshot.hooks {
        referenced.push(&hook.plugin_name);
        referenced.extend(hook.config_file.iter().map(|path| path.plugin.as_str()));
        referenced.extend(hook.cwd.iter().map(|path| path.plugin.as_str()));
    }
    for library in &snapshot.libraries {
        referenced.extend([
            library.plugin_name.as_str(),
            library.source_path.plugin.as_str(),
        ]);
    }
    referenced.extend(
        snapshot
            .connectors
            .iter()
            .map(|item| item.plugin_name.as_str()),
    );
    referenced.extend(
        snapshot
            .tool_groups
            .iter()
            .map(|item| item.plugin_name.as_str()),
    );
    referenced.extend(
        snapshot
            .tool_routes
            .iter()
            .map(|item| item.plugin_name.as_str()),
    );
    let unknown: BTreeSet<String> = referenced
        .into_iter()
        .filter(|name| !known.contains(name))
        .map(str::to_owned)
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(PluginSnapshotIdentityError {
            names: unknown.into_iter().collect(),
        })
    }
}

/// The bytes a pinned session stores. Reference `snapshot_bytes`.
///
/// # Errors
///
/// A value canonical JSON cannot carry.
pub fn snapshot_bytes(snapshot: &ResolvedPluginSnapshot) -> Result<Vec<u8>, CanonicalJsonError> {
    let value = serde_json::to_value(snapshot).unwrap_or(Value::Null);
    canonical_json(&value)
}

/// Reference `snapshot_digest`.
///
/// # Errors
///
/// As [`snapshot_bytes`].
pub fn snapshot_digest(snapshot: &ResolvedPluginSnapshot) -> Result<String, CanonicalJsonError> {
    snapshot_bytes(snapshot).map(|bytes| sha256_hex(&bytes))
}

/// The snapshot spelling of a source format, `None` for one no surviving
/// plugin can have. Reference `_SOURCE_FORMATS`.
#[must_use]
pub fn snapshot_source_format(format: DetectedPluginFormat) -> Option<&'static str> {
    match format {
        DetectedPluginFormat::AgentPlugins10
        | DetectedPluginFormat::ClaudeCode
        | DetectedPluginFormat::Codex
        | DetectedPluginFormat::KimiCode
        | DetectedPluginFormat::OpenCode => Some(format.as_str()),
        _ => None,
    }
}

/// The identity entry of one plugin. Reference `plugin_identity`.
#[must_use]
pub fn plugin_identity(plugin: &PluginDescriptor) -> PluginSnapshotEntry {
    PluginSnapshotEntry {
        name: normalize_nfc(&plugin.name),
        namespace: normalize_nfc(&plugin.namespace),
        version: plugin.version.as_deref().map(normalize_nfc),
        source_format: snapshot_source_format(plugin.source_format)
            .unwrap_or(plugin.source_format.as_str())
            .to_owned(),
        manifest_digest: plugin.manifest_digest.clone(),
    }
}

/// Projects a materialized plugin set into its snapshot. Reference
/// `build_snapshot`.
#[must_use]
pub fn build_snapshot(materialized: &MaterializedPluginSet) -> ResolvedPluginSnapshot {
    let resolution = &materialized.resolution;
    let snapshot = ResolvedPluginSnapshot {
        version: 1,
        plugins: resolution.plugins.iter().map(plugin_identity).collect(),
        skills: skills(resolution),
        knowledge: knowledge(resolution),
        agents: agents(resolution),
        hooks: hooks(resolution),
        libraries: libraries(resolution),
        connectors: resolution
            .connectors
            .iter()
            .map(|definition| PluginConnectorSnapshot {
                plugin_name: normalize_nfc(&definition.plugin_name),
                source_id: normalize_nfc(&definition.source_id),
                tools: definition
                    .tools
                    .iter()
                    .map(|tool| normalize_nfc(tool))
                    .collect(),
            })
            .collect(),
        tool_groups: materialized
            .tool_groups
            .iter()
            .map(|group| PluginToolGroupSnapshot {
                plugin_name: normalize_nfc(&group.plugin_name),
                name: normalize_nfc(&group.name),
                description: normalize_nfc(&group.description),
                tools: group
                    .tools
                    .iter()
                    .map(|tool| PluginToolSnapshot {
                        name: normalize_nfc(&tool.name),
                        description: normalize_nfc(&tool.description),
                        input_schema: normalize_json(&tool.input_schema),
                        output_schema: tool
                            .output_schema
                            .as_ref()
                            .map_or(Value::Null, normalize_json),
                        exposure: tool.exposure.clone(),
                    })
                    .collect(),
            })
            .collect(),
        tool_routes: materialized
            .tool_routes
            .values()
            .map(|route| PluginToolRouteSnapshot {
                plugin_name: normalize_nfc(&route.plugin_name),
                group_name: normalize_nfc(&route.group_name),
                function_name: normalize_nfc(&route.function_name),
                source_kind: route.source_kind.clone(),
                source_id: normalize_nfc(&route.source_id),
                source_tool_name: normalize_nfc(&route.source_tool_name),
                execution_name: normalize_nfc(&route.execution_name),
                schema_fingerprint: route.schema_fingerprint.clone(),
            })
            .collect(),
    };
    // Every component above is owned by a resolved plugin, so the identity
    // check cannot fail; the unordered projection is the fallback regardless.
    build_plugin_snapshot(snapshot.clone()).unwrap_or(snapshot)
}

fn descriptor<'a>(resolution: &'a ResolvedPluginSet, name: &str) -> Option<&'a PluginDescriptor> {
    resolution.plugins.iter().find(|plugin| plugin.name == name)
}

fn path_ref(plugin: &PluginDescriptor, target: &Path) -> Option<PluginPathRef> {
    plugin_path_ref(&plugin.name, &plugin.root, target).ok()
}

fn skills(resolution: &ResolvedPluginSet) -> Vec<PluginSkillSnapshot> {
    resolution
        .skills
        .iter()
        .filter_map(|(alias, skill)| {
            let namespace = alias.split(':').next().unwrap_or_default();
            let plugin = resolution
                .plugins
                .iter()
                .find(|plugin| plugin.namespace == namespace)?;
            let path = path_ref(plugin, skill.path.as_deref()?)?;
            Some(PluginSkillSnapshot {
                plugin_name: normalize_nfc(&plugin.name),
                name: normalize_nfc(alias),
                description: normalize_nfc(&skill.description),
                path,
            })
        })
        .collect()
}

fn knowledge(resolution: &ResolvedPluginSet) -> Vec<PluginKnowledgeSnapshot> {
    resolution
        .knowledge
        .iter()
        .filter_map(|definition| {
            let plugin = descriptor(resolution, &definition.plugin_name)?;
            Some(PluginKnowledgeSnapshot {
                plugin_name: normalize_nfc(&definition.plugin_name),
                name: normalize_nfc(&definition.name),
                source_name: normalize_nfc(&definition.source_name),
                description: normalize_nfc(&definition.description),
                display_name: definition.display_name.as_deref().map(normalize_nfc),
                icon: definition.icon.as_deref().map(normalize_nfc),
                path: path_ref(plugin, &definition.source_root)?,
                entrypoint: path_ref(plugin, &definition.source_entrypoint)?,
            })
        })
        .collect()
}

fn agents(resolution: &ResolvedPluginSet) -> Vec<PluginAgentSnapshot> {
    resolution
        .agents
        .iter()
        .filter_map(|definition| {
            let plugin = descriptor(resolution, &definition.plugin_name)?;
            Some(PluginAgentSnapshot {
                plugin_name: normalize_nfc(&definition.plugin_name),
                name: normalize_nfc(&definition.name),
                source_name: normalize_nfc(&definition.source_name),
                display_name: normalize_nfc(&definition.display_name),
                description: normalize_nfc(&definition.description),
                path: path_ref(plugin, &definition.source_file)?,
                safety: definition.safety.clone(),
                instructions: definition.instructions.as_deref().map(normalize_nfc),
                overrides: normalize_json(&Value::Object(definition.overrides.clone())),
            })
        })
        .collect()
}

fn hooks(resolution: &ResolvedPluginSet) -> Vec<PluginHookSnapshot> {
    resolution
        .runtime_hooks
        .iter()
        .filter_map(|definition| {
            let plugin = descriptor(resolution, &definition.plugin_name)?;
            let config = serde_json::to_value(&definition.config).unwrap_or(Value::Null);
            Some(PluginHookSnapshot {
                plugin_name: normalize_nfc(&plugin.name),
                declared_name: normalize_nfc(&definition.declared_name),
                source: definition.source.clone(),
                protocol: definition.protocol.clone(),
                visibility: definition.visibility.clone(),
                order: definition.order,
                config: normalize_json(&config),
                config_file: path_ref(plugin, &definition.config_file),
                cwd: definition
                    .cwd
                    .as_deref()
                    .and_then(|cwd| path_ref(plugin, cwd)),
                environment_names: hook_environment_names(definition.environment.keys()),
            })
        })
        .collect()
}

/// The environment names a hook snapshot records: everything the host does
/// not inject, sorted. Reference `hook_environment_names`.
#[must_use]
pub fn hook_environment_names<'a>(names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut names: Vec<String> = names
        .into_iter()
        .filter(|name| !HOST_HOOK_ENVIRONMENT.contains(&name.as_str()))
        .map(|name| normalize_nfc(name))
        .collect();
    names.sort();
    names
}

fn libraries(resolution: &ResolvedPluginSet) -> Vec<PluginLibrarySnapshot> {
    resolution
        .libraries
        .iter()
        .filter_map(|definition| {
            let plugin = descriptor(resolution, &definition.plugin_name)?;
            Some(PluginLibrarySnapshot {
                plugin_name: normalize_nfc(&definition.plugin_name),
                language: definition.language.clone(),
                alias: normalize_nfc(&definition.alias),
                source_path: path_ref(plugin, &definition.source_path)?,
            })
        })
        .collect()
}
