//! The MCP servers a session's plugins declare: their catalog names, how a
//! session discovers and connects to each, and what `/mcp` lists about them.
//!
//! Reference `PluginMCPCatalog` and `_RecordingDiscovery`
//! (`vibe/app_server/_plugin_mcp.py`), `RegistryMCPDiscovery`
//! (`vibe/core/plugins/_catalog.py`) and `merge_plugin_mcp_into_catalog`
//! (`vibe/app_server/_runtime.py`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;
use vibe_core::plugins::catalog::PluginMcpDiscovery;
use vibe_core::plugins::compatibility::{PluginMcpHttpAuth, PluginMcpServer};
use vibe_core::plugins::native::PluginMcpServerDefinition;

use super::SessionPlugins;
use vibe_core::sync::lock;

/// The MCP servers a session's plugins declare, as `/mcp` lists them: under
/// their catalog names, sorted by them, with what `records` holds for each and
/// the session's connection `connections` holds. A declared source id is its
/// catalog name unless a second plugin declared it too, when each takes a
/// digest-suffixed form (reference `PluginMCPCatalog.bind` over
/// `_catalog_names`, and `sources`, which reports a server never discovered
/// as `unavailable`).
#[must_use]
pub fn plugin_mcp_sources(
    plugins: &SessionPlugins,
    records: &BTreeMap<(String, String), McpDiscoveryRecord>,
    connections: &BTreeMap<(String, String), crate::resources::PluginMcpConnection>,
) -> Vec<crate::resources::PluginMcpSource> {
    let mut sources: Vec<crate::resources::PluginMcpSource> = catalog_names(plugins)
        .into_iter()
        .filter_map(|(definition, name)| {
            let key = (definition.plugin_name.clone(), definition.source_id.clone());
            let record = records.get(&key);
            Some(crate::resources::PluginMcpSource {
                name: name?,
                plugin_name: definition.plugin_name.clone(),
                transport: plugin_mcp_transport(&definition.server).to_owned(),
                status: record.map_or("unavailable", |record| record.status),
                tools: record
                    .map(|record| record.tools.clone())
                    .unwrap_or_default(),
                connection: connections.get(&key).cloned(),
            })
        })
        .collect();
    sources.sort_by(|left, right| left.name.cmp(&right.name));
    sources
}

/// The configuration a session connects to each plugin MCP server with, by
/// the catalog name a unified session's provided group is named after
/// (reference `merge_plugin_mcp_into_catalog`, which runs each under that
/// name).
pub(crate) fn plugin_server_configs(
    plugins: &SessionPlugins,
) -> BTreeMap<String, vibe_core::mcp::McpServerConfig> {
    catalog_names(plugins)
        .into_iter()
        .filter_map(|(definition, name)| {
            Some((
                name?,
                plugin_mcp_config(&definition.private_alias, &definition.server)?,
            ))
        })
        .collect()
}

/// Each plugin MCP server definition with its catalog name.
fn catalog_names(plugins: &SessionPlugins) -> Vec<(&PluginMcpServerDefinition, Option<String>)> {
    use vibe_core::plugins::naming::{ToolGroupIdentity, resolve_tool_group_names};
    let definitions = &plugins.materialized.resolution.mcp_servers;
    let identities: Vec<ToolGroupIdentity> = definitions
        .iter()
        .map(|definition| ToolGroupIdentity {
            plugin_name: definition.plugin_name.clone(),
            base_name: definition.source_id.clone(),
            source_id: definition.source_id.clone(),
        })
        .collect();
    let names = resolve_tool_group_names(identities.iter(), Vec::<String>::new());
    definitions
        .iter()
        .zip(&identities)
        .map(|(definition, identity)| (definition, names.get(identity).cloned()))
        .collect()
}

/// The session's own connection to every plugin MCP server whose catalog name
/// no configured server holds, keyed by plugin and source id. Reference
/// `merge_plugin_mcp_into_catalog` hands those servers to the unified
/// harness, whose runtime discovers each through every page of `tools/list`
/// and reports it `enabled`, or `unavailable` with the code of its failure
/// (`_resolve_server` in
/// `harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_mcp_runtime.py`).
/// An OAuth server awaits its authorization, as one without credentials does
/// there: this port keeps no plugin credentials to send it.
pub(super) async fn connect_plugin_servers(
    plugins: &SessionPlugins,
    configured: &BTreeSet<String>,
) -> BTreeMap<(String, String), crate::resources::PluginMcpConnection> {
    use crate::resources::PluginMcpConnection;
    use vibe_core::mcp::{McpAuthConfig, RuntimeDiscoveryFailure, discover_every_page};
    let pending = catalog_names(plugins)
        .into_iter()
        .filter(|(_, name)| name.as_ref().is_some_and(|name| !configured.contains(name)))
        .map(|(definition, _)| async move {
            let key = (definition.plugin_name.clone(), definition.source_id.clone());
            let unavailable = |error| PluginMcpConnection {
                status: "unavailable",
                tools: Vec::new(),
                schemas: BTreeMap::new(),
                error: Some(error),
            };
            let Some(config) = plugin_mcp_config(&definition.private_alias, &definition.server)
            else {
                return (key, unavailable("mcp_transport_failed"));
            };
            if matches!(config.auth, McpAuthConfig::Oauth(_)) {
                return (
                    key,
                    PluginMcpConnection {
                        status: "needs_auth",
                        tools: Vec::new(),
                        schemas: BTreeMap::new(),
                        error: None,
                    },
                );
            }
            let headers = vibe_core::mcp::authorization::http_headers(&config);
            let connection = match discover_every_page(&config, &headers).await {
                Ok(tools) => PluginMcpConnection {
                    status: "enabled",
                    schemas: tools
                        .iter()
                        .map(|tool| (tool.name.clone(), tool.input_schema.clone()))
                        .collect(),
                    tools: tools
                        .into_iter()
                        .map(|tool| (tool.name, tool.description))
                        .collect(),
                    error: None,
                },
                Err(RuntimeDiscoveryFailure::Unauthorized) => {
                    unavailable("authentication was rejected")
                }
                Err(RuntimeDiscoveryFailure::Failed(code)) => unavailable(code),
            };
            (key, connection)
        });
    futures_util::future::join_all(pending)
        .await
        .into_iter()
        .collect()
}

fn plugin_mcp_transport(server: &PluginMcpServer) -> &str {
    match server {
        PluginMcpServer::Stdio { .. } => "stdio",
        PluginMcpServer::Http { transport, .. } => transport,
        PluginMcpServer::AuthenticatedHttp { .. } => "http",
    }
}

/// What discovering one plugin MCP server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDiscoveryRecord {
    /// `connected`, `needs_auth` or `unavailable`.
    pub status: &'static str,
    pub tools: Vec<(String, Option<String>)>,
}

/// Discovers a plugin MCP server by connecting to it once and listing its
/// tools, recording each outcome by plugin and source id (reference
/// `RegistryMCPDiscovery` wrapped in `_RecordingDiscovery`). An OAuth server
/// is reported as awaiting authorization: this port keeps no plugin
/// credentials to send it.
#[derive(Default)]
pub struct RecordingMcpDiscovery {
    factory: vibe_core::mcp::DefaultMcpPeerFactory,
    records: Mutex<BTreeMap<(String, String), McpDiscoveryRecord>>,
}

impl RecordingMcpDiscovery {
    #[must_use]
    pub fn records(&self) -> BTreeMap<(String, String), McpDiscoveryRecord> {
        lock(&self.records).clone()
    }

    fn record(&self, definition: &PluginMcpServerDefinition, record: McpDiscoveryRecord) {
        lock(&self.records).insert(
            (definition.plugin_name.clone(), definition.source_id.clone()),
            record,
        );
    }
}

impl PluginMcpDiscovery for RecordingMcpDiscovery {
    fn discover<'a>(
        &'a self,
        definition: &'a PluginMcpServerDefinition,
    ) -> vibe_core::plugins::catalog::DiscoveryFuture<'a> {
        use vibe_core::mcp::McpPeerFactory;
        use vibe_core::plugins::catalog::PluginMcpDiscoveryFailure;
        Box::pin(async move {
            let unavailable = McpDiscoveryRecord {
                status: "unavailable",
                tools: Vec::new(),
            };
            let Some(config) = plugin_mcp_config(&definition.private_alias, &definition.server)
            else {
                self.record(definition, unavailable);
                return Err(PluginMcpDiscoveryFailure::Failed(
                    "the server URL is not valid".to_owned(),
                ));
            };
            if matches!(config.auth, vibe_core::mcp::McpAuthConfig::Oauth(_)) {
                self.record(
                    definition,
                    McpDiscoveryRecord {
                        status: "needs_auth",
                        tools: Vec::new(),
                    },
                );
                return Err(PluginMcpDiscoveryFailure::AuthorizationRequired);
            }
            let headers = vibe_core::mcp::authorization::http_headers(&config);
            let listed = match self.factory.connect(&config).await {
                Ok(peer) => peer.discover(&headers).await,
                Err(error) => Err(error),
            };
            match listed {
                Ok(tools) => {
                    self.record(
                        definition,
                        McpDiscoveryRecord {
                            status: "connected",
                            tools: tools
                                .iter()
                                .map(|tool| (tool.name.clone(), tool.description.clone()))
                                .collect(),
                        },
                    );
                    Ok(tools)
                }
                Err(error) => {
                    self.record(definition, unavailable);
                    Err(PluginMcpDiscoveryFailure::Failed(error.to_string()))
                }
            }
        })
    }
}

fn plugin_mcp_config(
    name: &str,
    server: &PluginMcpServer,
) -> Option<vibe_core::mcp::McpServerConfig> {
    use vibe_core::mcp::{McpAuthConfig, McpStaticAuth, McpTransportConfig};
    let (transport, auth) = match server {
        PluginMcpServer::Stdio {
            command,
            args,
            env,
            cwd,
            ..
        } => {
            let (program, leading) = command.split_first()?;
            let mut arguments = leading.to_vec();
            arguments.extend(args.iter().cloned());
            (
                McpTransportConfig::Stdio {
                    command: program.clone(),
                    arguments,
                    environment: env.clone(),
                    working_directory: cwd.as_ref().map(PathBuf::from),
                },
                McpAuthConfig::default(),
            )
        }
        PluginMcpServer::Http {
            transport,
            url,
            headers,
            ..
        } => {
            let url = url.parse().ok()?;
            let headers = headers.clone();
            let transport = if transport == "http" {
                McpTransportConfig::Http { url, headers }
            } else {
                McpTransportConfig::StreamableHttp { url, headers }
            };
            (transport, McpAuthConfig::default())
        }
        PluginMcpServer::AuthenticatedHttp {
            url, headers, auth, ..
        } => {
            let auth = match auth {
                PluginMcpHttpAuth::BearerTokenEnv(variable) => {
                    McpAuthConfig::Static(McpStaticAuth {
                        api_key_env: variable.clone(),
                        ..McpStaticAuth::default()
                    })
                }
                PluginMcpHttpAuth::OAuth => {
                    McpAuthConfig::Oauth(serde_json::from_value(json!({})).ok()?)
                }
            };
            (
                McpTransportConfig::Http {
                    url: url.parse().ok()?,
                    headers: headers.clone(),
                },
                auth,
            )
        }
    };
    Some(vibe_core::mcp::McpServerConfig {
        alias: name.to_owned(),
        transport,
        enabled: true,
        disabled_tools: BTreeSet::new(),
        startup_timeout_ms: vibe_core::mcp::DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        tool_timeout_ms: vibe_core::mcp::DEFAULT_MCP_TOOL_TIMEOUT_MS,
        auth,
        prompt: None,
        sampling_enabled: true,
        declared: None,
    })
}
