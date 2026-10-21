//! The catalogs a session's plugins publish: the `plugin/info` projection,
//! the `/plugins` catalog, the flat component list both are built from, and
//! the order of the skill catalogue.
//!
//! Reference `plugin_info` and `plugin_components_by_owner`
//! (`vibe/app_server/_plugins.py`) and `catalog`
//! (`vibe/app_server/plugin_catalog.py`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};
use vibe_core::plugins::compatibility::{PluginMcpHttpAuth, PluginMcpServer};
use vibe_core::plugins::native::PLUGIN_AGENT_TYPE;
use vibe_core::plugins::paths::{PluginPathRef, resolve_plugin_path};
use vibe_core::plugins::redaction::{redact_argv, redact_names, redact_url};

use super::SessionPlugins;
use super::issues::plugin_issues;

/// The skill catalogue a unified session publishes: its plugins' model
/// invocable skills, plugin by plugin in name order, then their other skills,
/// then every row `rows` holds that no plugin published. Reference
/// `project_core_skills` over `core_plugins`.
#[must_use]
pub fn order_skill_rows(plugins: &SessionPlugins, rows: Vec<Value>) -> Vec<Value> {
    let skills = &plugins.runtime_skills;
    let owners = &plugins.runtime_owners;
    let mut names: Vec<&String> = owners.values().collect();
    names.sort_unstable();
    names.dedup();
    let mut catalogue = Vec::new();
    for plugin in names {
        catalogue.extend(
            skills
                .iter()
                .filter(|(alias, skill)| {
                    skill.model_invocable
                        && owners.get(alias.split(':').next().unwrap_or_default()) == Some(plugin)
                })
                .map(|(_, skill)| vibe_core::skills::skill_summary(skill)),
        );
    }
    catalogue.extend(
        skills
            .iter()
            .filter(|(_, skill)| !skill.model_invocable)
            .map(|(_, skill)| vibe_core::skills::skill_summary(skill)),
    );
    catalogue.extend(rows.into_iter().filter(|row| row["source"] != "plugin"));
    catalogue
}

/// One entry of the flat component list.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginComponent {
    pub kind: String,
    pub name: String,
    pub source_path: Option<String>,
    pub config: Map<String, Value>,
}

impl PluginComponent {
    fn to_wire(&self) -> Value {
        json!({
            "kind": self.kind,
            "name": self.name,
            "sourcePath": self.source_path,
            "config": self.config,
        })
    }
}

/// Every plugin's components, kept attributed to their owner, in snapshot
/// order. Reference `plugin_components_by_owner`.
#[must_use]
pub fn plugin_components_by_owner(plugins: &SessionPlugins) -> Vec<(String, Vec<PluginComponent>)> {
    let resolution = &plugins.materialized.resolution;
    let roots: BTreeMap<&str, &Path> = resolution
        .plugins
        .iter()
        .map(|plugin| (plugin.name.as_str(), plugin.root.as_path()))
        .collect();
    let source_path = |reference: &PluginPathRef| -> Option<String> {
        let root = roots.get(reference.plugin.as_str())?;
        Some(
            resolve_plugin_path(reference, root)
                .to_string_lossy()
                .into_owned(),
        )
    };
    let mut listed: BTreeSet<(String, String)> = plugins.materialized.connected_mcp_sources.clone();
    listed.extend(
        plugins
            .snapshot
            .tool_routes
            .iter()
            .filter(|route| route.source_kind == "mcp")
            .map(|route| (route.plugin_name.clone(), route.source_id.clone())),
    );
    let mut mcp_servers: Vec<_> = resolution
        .mcp_servers
        .iter()
        .filter(|definition| {
            listed.contains(&(definition.plugin_name.clone(), definition.source_id.clone()))
        })
        .collect();
    mcp_servers.sort_by(|left, right| {
        (&left.plugin_name, &left.source_id).cmp(&(&right.plugin_name, &right.source_id))
    });
    let snapshot = &plugins.snapshot;
    let component = |kind: &str, name: &str, path: Option<&PluginPathRef>| PluginComponent {
        kind: kind.to_owned(),
        name: name.to_owned(),
        source_path: path.and_then(source_path),
        config: Map::new(),
    };
    snapshot
        .plugins
        .iter()
        .map(|entry| {
            let owner = entry.name.as_str();
            let mut components = Vec::new();
            for skill in snapshot
                .skills
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                components.push(component("skill", &skill.name, Some(&skill.path)));
            }
            for folder in snapshot
                .knowledge
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                components.push(component("knowledge", &folder.name, Some(&folder.path)));
            }
            for agent in snapshot
                .agents
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                let kind = if resolution
                    .agents
                    .iter()
                    .any(|definition| definition.name == agent.name)
                {
                    PLUGIN_AGENT_TYPE
                } else {
                    "agent"
                };
                components.push(component(kind, &agent.name, Some(&agent.path)));
            }
            for library in snapshot
                .libraries
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                components.push(component(
                    "library",
                    &library.alias,
                    Some(&library.source_path),
                ));
            }
            for hook in snapshot
                .hooks
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                components.push(component(
                    "hook",
                    &hook.declared_name,
                    hook.config_file.as_ref(),
                ));
            }
            for definition in mcp_servers.iter().filter(|item| item.plugin_name == owner) {
                components.push(PluginComponent {
                    kind: "mcp_server".to_owned(),
                    name: definition.source_id.clone(),
                    source_path: Some(definition.config_file.to_string_lossy().into_owned()),
                    config: mcp_server_config(&definition.server),
                });
            }
            for connector in snapshot
                .connectors
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                components.push(component("connector", &connector.source_id, None));
            }
            for group in snapshot
                .tool_groups
                .iter()
                .filter(|item| item.plugin_name == owner)
            {
                for tool in &group.tools {
                    components.push(component(
                        "tool",
                        &format!("{}.{}", group.name, tool.name),
                        None,
                    ));
                }
            }
            (entry.name.clone(), components)
        })
        .collect()
}

/// The redacted configuration `plugin/info` publishes for an MCP server.
/// Reference `_mcp_server_config`.
fn mcp_server_config(server: &PluginMcpServer) -> Map<String, Value> {
    let mut config = Map::new();
    match server {
        PluginMcpServer::Stdio { env, cwd, .. } => {
            config.insert("transport".to_owned(), json!("stdio"));
            config.insert("argv".to_owned(), json!(redact_argv(&server.argv())));
            config.insert("env".to_owned(), json!(redact_names(env.keys())));
            config.insert("cwd".to_owned(), json!(cwd));
        }
        PluginMcpServer::Http {
            transport,
            url,
            headers,
            ..
        } => {
            config.insert("transport".to_owned(), json!(transport));
            config.insert("url".to_owned(), json!(redact_url(url)));
            config.insert("headers".to_owned(), json!(redact_names(headers.keys())));
        }
        PluginMcpServer::AuthenticatedHttp {
            url, headers, auth, ..
        } => {
            let mut names: Vec<String> = headers.keys().cloned().collect();
            if let PluginMcpHttpAuth::BearerTokenEnv(variable) = auth
                && std::env::var(variable).is_ok_and(|token| !token.is_empty())
                && !names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
            {
                names.push("Authorization".to_owned());
            }
            config.insert("transport".to_owned(), json!("http"));
            config.insert("url".to_owned(), json!(redact_url(url)));
            config.insert("headers".to_owned(), json!(redact_names(names.iter())));
        }
    }
    config
}

/// The `plugin/info` catalog. Reference `plugin_info`.
#[must_use]
pub fn plugin_info(plugins: &SessionPlugins) -> Value {
    let pins: BTreeMap<&str, &str> = plugins
        .materialized
        .resolution
        .plugins
        .iter()
        .map(|plugin| (plugin.name.as_str(), plugin.content_digest.as_str()))
        .collect();
    let components: Vec<Value> = plugin_components_by_owner(plugins)
        .iter()
        .flat_map(|(_, components)| components.iter().map(PluginComponent::to_wire))
        .collect();
    let mut raw_plugins = Map::new();
    for entry in &plugins.snapshot.plugins {
        raw_plugins.insert(
            entry.name.clone(),
            json!({
                "manifestDigest": entry.manifest_digest,
                "contentSha256": pins.get(entry.name.as_str()),
            }),
        );
    }
    let mut routes = Map::new();
    for ((group, function), status) in &plugins.routes {
        if *status != "live" {
            routes.insert(format!("{group}.{function}"), json!(status));
        }
    }
    json!({
        "workdir": plugins.workdir.to_string_lossy(),
        "components": components,
        "raw": {
            "version": plugins.snapshot.version,
            "plugins": raw_plugins,
            "routes": routes,
        },
    })
}

/// The `/plugins` catalog. Reference `catalog` in
/// `vibe/app_server/plugin_catalog.py`.
#[must_use]
pub fn plugin_catalog(plugins: &SessionPlugins) -> Value {
    let resolution = &plugins.materialized.resolution;
    let owners: BTreeMap<&str, &str> = plugins
        .snapshot
        .tool_groups
        .iter()
        .map(|group| (group.name.as_str(), group.plugin_name.as_str()))
        .collect();
    let mut drift: BTreeMap<&str, BTreeMap<String, &str>> = BTreeMap::new();
    for ((group, function), status) in &plugins.routes {
        let Some(owner) = owners.get(group.as_str()) else {
            continue;
        };
        if *status == "live" {
            continue;
        }
        drift
            .entry(owner)
            .or_default()
            .insert(format!("{group}.{function}"), status);
    }
    let components = plugin_components_by_owner(plugins);
    let entries: Vec<Value> = plugins
        .snapshot
        .plugins
        .iter()
        .map(|entry| {
            let descriptor = resolution
                .plugins
                .iter()
                .find(|plugin| plugin.name == entry.name);
            let statuses = drift.get(entry.name.as_str());
            let owned = components
                .iter()
                .find(|(owner, _)| *owner == entry.name)
                .map(|(_, components)| components.as_slice())
                .unwrap_or_default();
            json!({
                "name": entry.name,
                "version": entry.version,
                "sourceFormat": entry.source_format,
                "manifestDigest": entry.manifest_digest,
                "description": descriptor.map_or("", |plugin| plugin.description.as_str()),
                "author": descriptor.and_then(|plugin| plugin.author.as_deref()),
                "scope": descriptor.map(|plugin| plugin.scope),
                "contentSha256": descriptor.map(|plugin| plugin.content_digest.as_str()),
                "pinnedRoot": descriptor.map(|plugin| plugin.root.to_string_lossy().into_owned()),
                "installedRoot": plugins
                    .installed_roots
                    .get(&entry.name)
                    .map(|root| root.to_string_lossy().into_owned()),
                "components": owned
                    .iter()
                    .map(|component| json!({
                        "kind": component.kind,
                        "name": component.name,
                        "status": if component.kind == "tool" {
                            statuses.and_then(|statuses| statuses.get(&component.name)).copied()
                        } else {
                            None
                        },
                    }))
                    .collect::<Vec<_>>(),
                "drifted": statuses.map_or(0, BTreeMap::len),
            })
        })
        .collect();
    json!({
        "plugins": entries,
        "dropped": plugin_issues(plugins),
    })
}
