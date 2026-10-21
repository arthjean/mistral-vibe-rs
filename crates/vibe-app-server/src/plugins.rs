//! The plugins a unified session resolves, pins and runs.
//!
//! Reference `vibe/app_server/_plugins.py` (`resolve_session_plugins`,
//! `UnifiedPluginProvider`, the `plugin/info` projection),
//! `vibe/app_server/plugin_catalog.py` (the `/plugins` catalog) and the
//! package store of the reference harness
//! (`harness/runtimes/python/.../vibe/plugins/_store.py`). A session's plugins
//! are resolved from the installed roots, each package is copied into a
//! content-addressed checkout under the session storage root, and the set the
//! session runs is resolved again from those checkouts, so a session outlives
//! the plugin being edited or uninstalled.

mod catalog;
mod issues;
mod mcp;
mod package_store;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use vibe_core::platform::resolve_lenient;
use vibe_core::plugins::catalog::{PluginConnectorCatalog, PluginMcpDiscovery};
use vibe_core::plugins::compatibility::plugin_runtime_state_names;
use vibe_core::plugins::drift::{
    PluginRouteKey, PluginRouteStatus, reconcile_plugin_routes, retain_pinned_tools,
};
use vibe_core::plugins::materialize::{MaterializedPluginSet, PluginMaterializer};
use vibe_core::plugins::snapshot::{ResolvedPluginSnapshot, build_snapshot};
use vibe_core::plugins::{PluginConfigIssue, PluginResolver, ResolvedPluginSet};
use vibe_core::skills::SkillScope;

pub use catalog::{
    PluginComponent, order_skill_rows, plugin_catalog, plugin_components_by_owner, plugin_info,
};
pub use issues::{plugin_issues, plugin_reload_notices, runtime_plugin_issues};
pub(crate) use mcp::plugin_server_configs;
pub use mcp::{McpDiscoveryRecord, RecordingMcpDiscovery, plugin_mcp_sources};
use package_store::PackageStore;

/// Where one session looks for plugins and keeps their runtime files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSources {
    /// Every open project's `.vibe/plugins` directory that exists.
    pub project_roots: Vec<PathBuf>,
    /// `{vibe_home}/plugins` when the user source is enabled and it exists.
    pub user_roots: Vec<PathBuf>,
    pub vibe_home: PathBuf,
    /// The session storage root, under which `plugins/` holds the package
    /// checkouts and each session's `${PLUGIN_DATA}`.
    pub storage_root: PathBuf,
    /// Where discovery was rooted, published as `plugin/info`'s `workdir`.
    pub workdir: PathBuf,
    /// The MCP server names the configuration holds, which a plugin server
    /// may not reuse.
    pub configured_mcp_names: BTreeSet<String>,
}

/// The ports that connect a plugin set's remote sources. Either may be
/// absent, which leaves the tool catalog empty.
#[derive(Clone, Copy, Default)]
pub struct PluginPorts<'a> {
    pub mcp_discovery: Option<&'a dyn PluginMcpDiscovery>,
    pub connector_catalog: Option<&'a dyn PluginConnectorCatalog>,
}

/// Everything one session knows about its plugins. Reference `SessionPlugins`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPlugins {
    pub materialized: MaterializedPluginSet,
    /// The catalog the session runs.
    pub snapshot: ResolvedPluginSnapshot,
    pub workdir: PathBuf,
    /// Per-route status after a re-pin; a route absent here is live.
    pub routes: BTreeMap<PluginRouteKey, PluginRouteStatus>,
    /// Where each plugin was installed as of the last scan.
    pub installed_roots: BTreeMap<String, PathBuf>,
    /// The scope each installed plugin resolved at, which the checkouts are
    /// resolved under again.
    pub installed_scopes: BTreeMap<String, SkillScope>,
    /// What resolving the installed tree reported when the session started,
    /// which the runtime snapshot lists: the reference derives the session's
    /// runtime from that resolve, ahead of the pin, and a reload does not
    /// derive it again.
    pub installed_issues: Vec<PluginConfigIssue>,
    /// The plugin skills that same derivation listed, by alias, which the
    /// runtime snapshot keeps publishing across reloads.
    pub runtime_skills: Vec<(String, vibe_core::extensions::SkillDefinition)>,
    /// The plugin owning each namespace in that derivation.
    pub runtime_owners: BTreeMap<String, String>,
    /// The library search paths that derivation hands the commands a session
    /// runs (reference `_runtime.py`, the unified harness `env`).
    pub runtime_environment: BTreeMap<String, String>,
    /// The plugin MCP servers `/mcp` lists, filled in by
    /// [`plugin_mcp_sources`] once discovery recorded what each answered.
    pub mcp_sources: Vec<crate::resources::PluginMcpSource>,
}

impl SessionPlugins {
    #[must_use]
    pub fn issues(&self) -> &[PluginConfigIssue] {
        &self.materialized.issues
    }
}

/// Resolves and materializes the installed plugin tree. Reference
/// `resolve_session_plugins` and `_resolve_installed`.
pub async fn resolve_installed(
    sources: &PluginSources,
    ports: PluginPorts<'_>,
) -> MaterializedPluginSet {
    let mut builtin_issue = None;
    let builtin_roots =
        match vibe_core::plugins::builtin::materialize_builtin_plugins(&sources.vibe_home) {
            Ok(root) => vec![root],
            Err(error) => {
                builtin_issue = Some(PluginConfigIssue::plain(
                    sources
                        .vibe_home
                        .join(vibe_core::plugins::builtin::BUILTIN_PLUGINS_DIRECTORY),
                    format!("Could not write the built-in plugins: {error}"),
                ));
                Vec::new()
            }
        };
    let mut resolution = PluginResolver {
        project_roots: sources.project_roots.clone(),
        user_roots: sources.user_roots.clone(),
        builtin_roots,
        data_root_base: Some(sources.vibe_home.join("plugin-data")),
        configured_mcp_names: Some(sources.configured_mcp_names.clone()),
        vibe_home: sources.vibe_home.clone(),
        ..PluginResolver::default()
    }
    .resolve();
    resolution.issues.extend(builtin_issue);
    materialize(resolution, ports).await
}

async fn materialize(
    resolution: ResolvedPluginSet,
    ports: PluginPorts<'_>,
) -> MaterializedPluginSet {
    PluginMaterializer {
        mcp_discovery: ports.mcp_discovery,
        connector_catalog: ports.connector_catalog,
    }
    .materialize(resolution)
    .await
}

/// Resolves, pins and binds a new session's plugins through a
/// [`RecordingMcpDiscovery`], run to completion on a thread and runtime of its
/// own so a caller on the request loop can wait for it, as the reference's
/// `session/start` waits for its plugin discovery. The session's own
/// connection to each plugin server follows, as the reference hands them to
/// its runtime. `None` when that runtime could not be built.
#[must_use]
pub fn start_discovering(sources: &PluginSources, session_id: &str) -> Option<SessionPlugins> {
    bind_discovering(sources, session_id, None)
}

/// Rescans the installed roots and re-pins the session to what it finds in
/// place of `previous`, run as [`start_discovering`] runs a new session's.
#[must_use]
pub fn reload_discovering(
    sources: &PluginSources,
    session_id: &str,
    previous: &SessionPlugins,
) -> Option<SessionPlugins> {
    bind_discovering(sources, session_id, Some(previous))
}

/// [`bind`] as [`start_discovering`] and [`reload_discovering`] run it.
fn bind_discovering(
    sources: &PluginSources,
    session_id: &str,
    previous: Option<&SessionPlugins>,
) -> Option<SessionPlugins> {
    let discovery = RecordingMcpDiscovery::default();
    let ports = PluginPorts {
        mcp_discovery: Some(&discovery),
        connector_catalog: None,
    };
    let (mut plugins, connections) = block_on_own_runtime(async {
        let plugins = bind(sources, session_id, previous, ports).await;
        let connections =
            mcp::connect_plugin_servers(&plugins, &sources.configured_mcp_names).await;
        (plugins, connections)
    })?;
    plugins.mcp_sources = plugin_mcp_sources(&plugins, &discovery.records(), &connections);
    Some(plugins)
}

pub(crate) fn block_on_own_runtime<T: Send>(
    future: impl std::future::Future<Output = T> + Send,
) -> Option<T> {
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()
                    .map(|runtime| runtime.block_on(future))
            })
            .join()
            .ok()
            .flatten()
    })
}

/// Resolves the installed roots, pins each package into its checkout and
/// binds the session to the set resolved from those checkouts.
///
/// A new session runs the catalog that set derives. A reload, which names the
/// set it replaces as `previous`, is reference `UnifiedPluginProvider.rescan`
/// followed by the re-pin `config/write` performs: the tools `previous`
/// published stay pinned, each route reports whether its source still offers
/// it, and the runtime derived when the session started is kept.
async fn bind(
    sources: &PluginSources,
    session_id: &str,
    previous: Option<&SessionPlugins>,
    ports: PluginPorts<'_>,
) -> SessionPlugins {
    let installed = resolve_installed(sources, ports).await;
    let store = PackageStore::new(&sources.storage_root);
    let mut checkouts = BTreeMap::new();
    let mut pin_issues = Vec::new();
    let installed_roots: BTreeMap<String, PathBuf> = installed
        .resolution
        .plugins
        .iter()
        .map(|plugin| (plugin.name.clone(), plugin.root.clone()))
        .collect();
    let installed_scopes: BTreeMap<String, SkillScope> = installed
        .resolution
        .plugins
        .iter()
        .map(|plugin| (plugin.name.clone(), plugin.scope))
        .collect();
    for plugin in &installed.resolution.plugins {
        let ignored = plugin_runtime_state_names(plugin.source_format);
        match store.checkout(&plugin.root, &plugin.content_digest, &ignored) {
            Ok(checkout) => {
                checkouts.insert(plugin.name.clone(), checkout);
            }
            Err(error) => pin_issues.push(PluginConfigIssue::plain(
                &plugin.root,
                format!("Could not pin the plugin package: {error}"),
            )),
        }
    }
    let data_root = store.data_root(session_id);
    let plugin_dirs: Vec<PathBuf> = checkouts.values().cloned().collect();
    let plugin_scopes = checkouts
        .iter()
        .map(|(name, checkout)| {
            (
                resolve_lenient(checkout),
                installed_scopes
                    .get(name)
                    .copied()
                    .unwrap_or(SkillScope::Project),
            )
        })
        .collect();
    let mut resolution = PluginResolver {
        plugin_dirs,
        plugin_scopes,
        data_root_base: Some(data_root),
        configured_mcp_names: Some(sources.configured_mcp_names.clone()),
        vibe_home: sources.vibe_home.clone(),
        ..PluginResolver::default()
    }
    .resolve();
    for plugin in &resolution.plugins {
        // `${PLUGIN_DATA}` is handed out as writable, so it has to exist.
        let _ = fs::create_dir_all(&plugin.data_root);
    }
    resolution.issues.extend(pin_issues);
    let materialized = materialize(resolution, ports).await;
    let derived = build_snapshot(&materialized);
    let (snapshot, routes) = match previous {
        None => (derived, BTreeMap::new()),
        Some(previous) => {
            let published = retain_pinned_tools(&derived, &previous.snapshot);
            let routes = if published == derived {
                BTreeMap::new()
            } else {
                reconcile_plugin_routes(&published, &materialized)
            };
            (published, routes)
        }
    };
    SessionPlugins {
        materialized,
        snapshot,
        workdir: sources.workdir.clone(),
        routes,
        installed_roots,
        installed_scopes,
        installed_issues: previous.map_or_else(
            || installed.issues.clone(),
            |previous| previous.installed_issues.clone(),
        ),
        runtime_skills: previous.map_or_else(
            || installed.resolution.skills.clone(),
            |previous| previous.runtime_skills.clone(),
        ),
        runtime_owners: previous.map_or_else(
            || {
                installed
                    .resolution
                    .plugins
                    .iter()
                    .map(|plugin| (plugin.namespace.clone(), plugin.name.clone()))
                    .collect()
            },
            |previous| previous.runtime_owners.clone(),
        ),
        runtime_environment: previous.map_or_else(
            || {
                installed
                    .process_environment
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            },
            |previous| previous.runtime_environment.clone(),
        ),
        mcp_sources: Vec::new(),
    }
}

#[cfg(test)]
mod plugins_tests;
