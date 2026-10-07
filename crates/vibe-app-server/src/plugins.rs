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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use vibe_core::plugins::catalog::{PluginConnectorCatalog, PluginMcpDiscovery};
use vibe_core::plugins::compatibility::{
    PluginMcpHttpAuth, PluginMcpServer, plugin_runtime_state_names,
};
use vibe_core::plugins::content::digest_plugin_tree;
use vibe_core::plugins::drift::{
    PluginRouteKey, PluginRouteStatus, reconcile_plugin_routes, retain_pinned_tools,
};
use vibe_core::plugins::materialize::{MaterializedPluginSet, PluginMaterializer};
use vibe_core::plugins::paths::{PluginPathRef, resolve_lax, resolve_plugin_path};
use vibe_core::plugins::redaction::{redact_argv, redact_names, redact_url};
use vibe_core::plugins::snapshot::{ResolvedPluginSnapshot, build_snapshot};
use vibe_core::plugins::{PluginConfigIssue, PluginResolver, ResolvedPluginSet};
use vibe_core::skills::SkillScope;

/// The reload notice codes: a source a rescan cannot repair. Reference
/// `_RELOAD_NOTICE_CODES`.
const RELOAD_NOTICE_CODES: [&str; 3] = [
    "plugin.mcp.connection_failed",
    "plugin.connector.unavailable",
    "plugin.connector.runtime_unavailable",
];

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

/// Resolves, pins and binds a new session's plugins.
pub async fn start(
    sources: &PluginSources,
    session_id: &str,
    ports: PluginPorts<'_>,
) -> SessionPlugins {
    let installed = resolve_installed(sources, ports).await;
    pin_and_bind(sources, session_id, &installed, ports, None).await
}

/// [`start`] with a [`RecordingMcpDiscovery`], run to completion on a thread
/// and runtime of its own so a caller on the request loop can wait for it, as
/// the reference's `session/start` waits for its plugin discovery. `None`
/// when that runtime could not be built.
#[must_use]
pub fn start_discovering(sources: &PluginSources, session_id: &str) -> Option<SessionPlugins> {
    let discovery = RecordingMcpDiscovery::default();
    let ports = PluginPorts {
        mcp_discovery: Some(&discovery),
        connector_catalog: None,
    };
    let mut plugins = block_on_own_runtime(start(sources, session_id, ports))?;
    plugins.mcp_sources = plugin_mcp_sources(&plugins, &discovery.records());
    Some(plugins)
}

/// [`reload`] as [`start_discovering`] runs [`start`].
#[must_use]
pub fn reload_discovering(
    sources: &PluginSources,
    session_id: &str,
    previous: &SessionPlugins,
) -> Option<SessionPlugins> {
    let discovery = RecordingMcpDiscovery::default();
    let ports = PluginPorts {
        mcp_discovery: Some(&discovery),
        connector_catalog: None,
    };
    let mut plugins = block_on_own_runtime(reload(sources, session_id, previous, ports))?;
    plugins.mcp_sources = plugin_mcp_sources(&plugins, &discovery.records());
    Some(plugins)
}

fn block_on_own_runtime<T: Send>(future: impl std::future::Future<Output = T> + Send) -> Option<T> {
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

/// Rescans the installed roots and re-pins the session to what it finds.
/// Reference `UnifiedPluginProvider.rescan` followed by the re-pin
/// `config/write` performs.
pub async fn reload(
    sources: &PluginSources,
    session_id: &str,
    previous: &SessionPlugins,
    ports: PluginPorts<'_>,
) -> SessionPlugins {
    let installed = resolve_installed(sources, ports).await;
    pin_and_bind(sources, session_id, &installed, ports, Some(previous)).await
}

async fn pin_and_bind(
    sources: &PluginSources,
    session_id: &str,
    installed: &MaterializedPluginSet,
    ports: PluginPorts<'_>,
    previous: Option<&SessionPlugins>,
) -> SessionPlugins {
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
                resolve_lax(checkout),
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

/// The content-addressed package checkouts. Reference `PluginPackageStore`,
/// whose blob and manifest layers this port does not keep (row 36 owns the
/// unified session storage that reads them back).
struct PackageStore {
    root: PathBuf,
}

impl PackageStore {
    fn new(storage_root: &Path) -> Self {
        Self {
            root: resolve_lax(storage_root).join("plugins"),
        }
    }

    fn data_root(&self, session_id: &str) -> PathBuf {
        self.root.join("data").join(session_id)
    }

    /// The checkout of `source` at `packages/<ab>/<digest>`, copied the first
    /// time and verified against `digest`.
    fn checkout(
        &self,
        source: &Path,
        digest: &str,
        ignored: &BTreeSet<String>,
    ) -> io::Result<PathBuf> {
        let shard = digest.get(..2).unwrap_or(digest);
        let target = self.root.join("packages").join(shard).join(digest);
        if target.is_dir() {
            return Ok(target);
        }
        let temporary_root = self.root.join("tmp");
        fs::create_dir_all(&temporary_root)?;
        let staging = temporary_root.join(format!(
            "package-{}-{}",
            digest.get(..16).unwrap_or(digest),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&staging);
        let outcome = copy_package(source, &staging, ignored).and_then(|()| {
            let rebuilt = digest_plugin_tree(&staging, &BTreeSet::new())?;
            if rebuilt != digest {
                return Err(io::Error::other(format!(
                    "the copied package digests to {rebuilt}, not {digest}"
                )));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            match fs::rename(&staging, &target) {
                Err(_) if target.is_dir() => Ok(()),
                Err(error) => Err(error),
                Ok(()) => harden(&target),
            }
        });
        let _ = fs::remove_dir_all(&staging);
        outcome.map(|()| target)
    }
}

/// Leaves a checkout readable by its owner alone and writable by nobody:
/// files `0400`, directories `0500`, as the reference's store leaves them.
/// Nothing is executable, so a stdio server a plugin ships as a script in its
/// tree cannot be launched from the checkout.
#[cfg(unix)]
fn harden(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            harden(&entry.path())?;
        } else if kind.is_file() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o400))?;
        }
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o500))
}

#[cfg(not(unix))]
fn harden(_root: &Path) -> io::Result<()> {
    Ok(())
}

/// Copies a package tree, keeping symbolic links as links and leaving out
/// every entry an ignored name covers.
fn copy_package(source: &Path, target: &Path, ignored: &BTreeSet<String>) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let mut entries: Vec<_> = fs::read_dir(source)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if ignored.contains(name.to_string_lossy().as_ref()) {
            continue;
        }
        let path = entry.path();
        let destination = target.join(&name);
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            symlink(&fs::read_link(&path)?, &destination)?;
        } else if kind.is_dir() {
            copy_package(&path, &destination, ignored)?;
        } else if kind.is_file() {
            fs::copy(&path, &destination)?;
        } else {
            return Err(io::Error::other(format!(
                "{} is neither a file nor a symbolic link",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

/// The resolution diagnostics as `ConfigIssue`s, sorted. Reference
/// `plugin_issues`.
#[must_use]
pub fn plugin_issues(plugins: &SessionPlugins) -> Vec<Value> {
    sorted_issues(plugins.issues())
}

/// [`plugin_issues`] over the installed resolve, which is what the runtime
/// snapshot lists.
#[must_use]
pub fn runtime_plugin_issues(plugins: &SessionPlugins) -> Vec<Value> {
    sorted_issues(&plugins.installed_issues)
}

fn sorted_issues(issues: &[PluginConfigIssue]) -> Vec<Value> {
    let mut issues: Vec<&PluginConfigIssue> = issues.iter().collect();
    issues.sort_by(|left, right| {
        (
            left.file.as_os_str(),
            left.code.as_deref().unwrap_or(""),
            &left.message,
        )
            .cmp(&(
                right.file.as_os_str(),
                right.code.as_deref().unwrap_or(""),
                &right.message,
            ))
    });
    issues
        .into_iter()
        .map(|issue| json!({"file": issue.file.to_string_lossy(), "message": issue.message}))
        .collect()
}

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

/// The MCP servers a session's plugins declare, as `/mcp` lists them: under
/// their catalog names, sorted by them, with what `records` holds for each. A
/// declared source id is its catalog name unless a second plugin declared it
/// too, when each takes a digest-suffixed form (reference
/// `PluginMCPCatalog.bind` over `_catalog_names`, and `sources`, which reports
/// a server never discovered as `unavailable`).
#[must_use]
pub fn plugin_mcp_sources(
    plugins: &SessionPlugins,
    records: &BTreeMap<(String, String), McpDiscoveryRecord>,
) -> Vec<crate::resources::PluginMcpSource> {
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
    let mut sources: Vec<crate::resources::PluginMcpSource> = definitions
        .iter()
        .zip(&identities)
        .filter_map(|(definition, identity)| {
            let record =
                records.get(&(definition.plugin_name.clone(), definition.source_id.clone()));
            Some(crate::resources::PluginMcpSource {
                name: names.get(identity)?.clone(),
                plugin_name: definition.plugin_name.clone(),
                transport: plugin_mcp_transport(&definition.server).to_owned(),
                status: record.map_or("unavailable", |record| record.status),
                tools: record
                    .map(|record| record.tools.clone())
                    .unwrap_or_default(),
            })
        })
        .collect();
    sources.sort_by(|left, right| left.name.cmp(&right.name));
    sources
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
    records: std::sync::Mutex<BTreeMap<(String, String), McpDiscoveryRecord>>,
}

impl RecordingMcpDiscovery {
    #[must_use]
    pub fn records(&self) -> BTreeMap<(String, String), McpDiscoveryRecord> {
        self.records
            .lock()
            .map(|records| records.clone())
            .unwrap_or_default()
    }

    fn record(
        &self,
        definition: &vibe_core::plugins::native::PluginMcpServerDefinition,
        record: McpDiscoveryRecord,
    ) {
        if let Ok(mut records) = self.records.lock() {
            records.insert(
                (definition.plugin_name.clone(), definition.source_id.clone()),
                record,
            );
        }
    }
}

impl PluginMcpDiscovery for RecordingMcpDiscovery {
    fn discover<'a>(
        &'a self,
        definition: &'a vibe_core::plugins::native::PluginMcpServerDefinition,
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

/// The remarks a successful reload still has to make. Reference
/// `plugin_reload_notices`.
#[must_use]
pub fn plugin_reload_notices(plugins: &SessionPlugins) -> Vec<String> {
    let mut issues: Vec<&PluginConfigIssue> = plugins.issues().iter().collect();
    issues.sort_by(|left, right| left.message.cmp(&right.message));
    let mut notices: Vec<String> = issues
        .into_iter()
        .filter(|issue| {
            issue
                .code
                .as_deref()
                .is_some_and(|code| RELOAD_NOTICE_CODES.contains(&code))
        })
        .map(|issue| issue.message.clone())
        .collect();
    let mut unavailable: Vec<String> = plugins
        .routes
        .iter()
        .filter(|(_, status)| **status == "unavailable")
        .map(|((group, function), _)| format!("{group}.{function}"))
        .collect();
    unavailable.sort();
    if !unavailable.is_empty() {
        notices.push(format!(
            "These plugin tools are no longer offered by their source and calling one will fail: {}.",
            unavailable.join(", ")
        ));
    }
    notices
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
                let kind = resolution
                    .agents
                    .iter()
                    .find(|definition| definition.name == agent.name)
                    .map_or("agent", |definition| {
                        if definition.agent_type == "subagent" {
                            "subagent"
                        } else {
                            "agent"
                        }
                    });
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

#[cfg(test)]
mod plugins_tests;
