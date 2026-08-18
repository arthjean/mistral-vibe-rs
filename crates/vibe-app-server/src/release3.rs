//! The methods the parity plan groups as release 3: configuration, saved
//! sessions, agents, skills and workspace prompts.
//!
//! The name is a delivery scope from `docs/parity.md`, not a domain. What the
//! module actually owns is stated by its parts: [`config`] answers the layered
//! configuration document and the writes against it, [`sessions`] the saved
//! transcript and everything a client does to one, and [`agents`] the profiles,
//! skills and prompts a session runs under. [`Release3Service`] is the shared
//! state all three read: the paths, the store, the configuration and the
//! extension catalog.
//!
//! `RELEASE3_METHODS` is the inventory the router advertises, and every method
//! in it is answered by exactly one of the three parts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod agents;
mod config;
mod sessions;

use crate::builtin_agents;
use crate::host::now_millis;
use crate::params::{self, required_string, usize_param};
use crate::vocabulary::{AccountActionKind, AccountStatus, AgentSafety};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use toml::{Table, Value as TomlValue};
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::{
    ConfigDiscovery, ConfigMutation, ConfigPatchOp, ConfigPaths, ConfigSnapshot, ConfigTarget,
    ConfigWrite, DotenvValues, JsonPointer, LayeredConfig, PatchOperation, ProxyEnvironmentStore,
    ProxyKey,
};
use vibe_core::continuity::SessionContinuity;
use vibe_core::events::ModelMessage;
use vibe_core::extensions::{
    AgentKind, AgentProfile, AgentRegistry, DiscoveryRoots, ExtensionCatalog, ExtensionSource,
    discover_extensions,
};
use vibe_core::mcp::McpServerConfig;
use vibe_core::middleware::CompactionSettings;
use vibe_core::policy::AllowlistPersistence;
use vibe_core::prompt::{
    InstructionLoader, PromptComposition, PromptResolver, SkillSummary, SubagentSummary,
    UserResource, prepare_user_resources,
};
use vibe_core::skills::{SearchInputs, SkillDiscovery, search_paths, skill_summary};
use vibe_core::storage::{HydratedSession, SessionStore, StorageError};
use vibe_core::tools::config::ToolConfigResolver;

/// The variable the reference reads the web-search credential from, and the one
/// [`crate::server::AppServer`] resolves it through.
const MISTRAL_KEY: &str = "MISTRAL_API_KEY";

/// The levels `config/thinking/write` accepts, as the wire literal declares
/// them.
const THINKING_LEVELS: [&str; 5] = ["off", "low", "medium", "high", "max"];

pub const RELEASE3_METHODS: &[&str] = &[
    "agents/install",
    "agents/list",
    "agents/uninstall",
    "config/batchWrite",
    "config/fields/read",
    "config/patch",
    "config/proxy/read",
    "config/proxy/write",
    "config/read",
    "config/reload",
    "config/schema",
    "config/thinking/write",
    "history/list",
    "session/agent/update",
    "session/continue",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/list",
    "session/log/read",
    "session/resume",
    "session/rewind",
    "session/rewind/read",
    "session/title/update",
    "skills/list",
    "workspace/prompt/prepare",
];

#[derive(Debug, Clone)]
pub struct Release3Paths {
    pub vibe_home: PathBuf,
    pub working_directory: PathBuf,
    pub session_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeAttachment {
    pub id: String,
    pub working_directory: String,
    pub parent_session_id: Option<String>,
    pub agent: Option<String>,
    pub agent_profile: Option<AgentProfile>,
    pub hydrated: HydratedSession,
}

/// The part of `RuntimeSnapshot` this service owns, already projected into the
/// wire shapes its census declares.
#[derive(Debug, Clone)]
pub struct RuntimeProjection {
    /// A `ConfigView`.
    pub config: Value,
    /// A `ConfigView`.
    pub base_config: Value,
    /// An `AgentSummary`.
    pub active_agent: Value,
    /// `AgentSummary` entries.
    pub agents: Vec<Value>,
    /// `SkillSummary` entries.
    pub skills: Vec<Value>,
    /// `ConfigIssue` entries raised while the extensions were discovered.
    pub issues: Vec<Value>,
    pub hooks_count: usize,
}

/// One agent profile as `AgentSummary` declares it.
///
/// `safety` is normalized into the four values the wire vocabulary carries: a
/// profile file may declare anything, and an unknown word is published as
/// `neutral` rather than as a value no client can render.
pub(crate) fn agent_summary(profile: &AgentProfile) -> Value {
    json!({
        "name": profile.name,
        "displayName": profile.display_name,
        "description": profile.description,
        "safety": AgentSafety::parse(&profile.safety),
        "agentType": profile.kind,
    })
}

#[derive(Debug, Clone)]
pub struct Release3Dispatch {
    pub result: BTreeMap<String, Value>,
    pub attachment: Option<RuntimeAttachment>,
}

impl Release3Dispatch {
    fn result(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Self {
        Self {
            result: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
            attachment: None,
        }
    }
}

#[derive(Clone)]
pub struct Release3Service {
    paths: Release3Paths,
    defaults: Table,
    config: LayeredConfig,
    /// The per-tool configuration every session's tools read through, kept
    /// current by a subscription on the `tools` key.
    tool_config: ToolConfigResolver,
    store: SessionStore,
    continuity: SessionContinuity,
    discovery_roots: DiscoveryRoots,
    project_trusted: bool,
    allowed_roots: Vec<PathBuf>,
    agents: Arc<Mutex<AgentRegistry>>,
    next_session: Arc<AtomicU64>,
    persist_runtime_sessions: bool,
}

/// The `VIBE_*` variables the environment layer composes: the process
/// environment with `{vibe_home}/.env` filling in what it does not set.
///
/// `VIBE_HOME` is excluded because it selects the home the file was read from
/// and is not a configuration field.
fn vibe_environment(vibe_home: &Path) -> BTreeMap<String, String> {
    DotenvValues::global(vibe_home)
        .environment()
        .into_iter()
        .filter(|(key, _)| key != "VIBE_HOME" && key.starts_with("VIBE_"))
        .collect()
}

/// The extension roots the open project directories contribute.
///
/// Reference `HarnessFilesManager.project_roots` resolves and deduplicates the
/// open directories; each one carries its own `.vibe` directory. An untrusted
/// workspace contributes none, which is the filter [`DiscoveryRoots`] applies
/// anyway.
/// The project directories skill discovery walks, which are the roots
/// themselves rather than their `.vibe` subdirectory: a project contributes
/// both `.vibe/skills` and `.agents/skills`, and only the root names both.
fn project_skill_roots(config: &LayeredConfig, project_trusted: bool) -> Vec<PathBuf> {
    if !project_trusted {
        return Vec::new();
    }
    config.harness_files().project_roots()
}

fn project_discovery_roots(config: &LayeredConfig, project_trusted: bool) -> Vec<PathBuf> {
    if !project_trusted {
        return Vec::new();
    }
    config
        .harness_files()
        .project_roots()
        .into_iter()
        .map(|root| root.join(".vibe"))
        .collect()
}

/// The runtime discovery pass behind the Discovered configuration layer.
///
/// Every tool declares its own settings, and the layer is where those settings
/// become addressable rather than buried in the binary:
/// `tools.grep.default_max_matches` in a file wins over the declaration in the
/// effective document, and the handler reads the merged value at its next call.
/// Reference `create_default_config` fills the same table from
/// `discover_tool_defaults`, which enumerates declarations rather than the
/// surface a host publishes, so the Windows-only families appear here on a
/// POSIX host too.
fn tool_discovery(resolver: ToolConfigResolver) -> ConfigDiscovery {
    Arc::new(move || Ok(resolver.discovered_document()))
}

impl Default for Release3Service {
    fn default() -> Self {
        let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vibe_home = crate::host::vibe_home();
        Self::build(
            Release3Paths {
                session_root: vibe_home.join("sessions"),
                vibe_home,
                working_directory,
            },
            false,
        )
    }
}

impl Release3Service {
    pub fn new(paths: Release3Paths, project_trusted: bool) -> Result<Self, Release3Error> {
        Ok(Self::build(paths, project_trusted))
    }

    /// The home this service reads its configuration and credentials under, and
    /// the one the log file belongs to.
    #[must_use]
    pub fn vibe_home(&self) -> &Path {
        &self.paths.vibe_home
    }

    fn build(paths: Release3Paths, project_trusted: bool) -> Self {
        // The Defaults layer is the shipped document at every construction
        // site: a service built without it composes a configuration the
        // reference could never produce.
        let defaults = vibe_core::config::registry::default_document();
        let config = LayeredConfig::new(
            ConfigPaths {
                vibe_home: paths.vibe_home.clone(),
                working_directory: paths.working_directory.clone(),
            },
            defaults.clone(),
        )
        .with_environment(vibe_environment(&paths.vibe_home))
        .with_discovery(tool_discovery(ToolConfigResolver::new()))
        .with_project_trusted(project_trusted);
        // The resolver reads the effective document, which the discovery pass
        // above has already filled with the declared defaults, and keeps
        // reading it: the subscription refreshes the cache whenever a write
        // moves anything under `tools`, so a session between two turns sees the
        // change without re-registering its surface.
        let tool_config = ToolConfigResolver::new();
        tool_config.follow(&config);
        let user_extensions = paths.vibe_home.join("extensions");
        let discovery_roots = DiscoveryRoots {
            configured: Vec::new(),
            project: project_discovery_roots(&config, project_trusted),
            user: vec![user_extensions.clone()],
            project_trusted,
            // The skill roots are resolved per catalog build rather than
            // stored, so a `skill_paths` written between two builds changes
            // what the next one publishes.
            skills: SkillDiscovery::default(),
        };
        let mut registry = AgentRegistry::with_initial(
            builtin_agents::default_profile(),
            user_extensions.join("agents"),
        );
        for profile in builtin_agents::profiles(&paths.vibe_home) {
            registry.register_builtin(profile);
        }
        let store = SessionStore::new(&paths.session_root);
        Self {
            config,
            tool_config,
            store: store.clone(),
            continuity: SessionContinuity::new(store),
            defaults,
            discovery_roots,
            project_trusted,
            allowed_roots: vec![paths.working_directory.clone()],
            paths,
            agents: Arc::new(Mutex::new(registry)),
            next_session: Arc::new(AtomicU64::new(1)),
            persist_runtime_sessions: false,
        }
    }

    #[must_use]
    pub fn layered_config(&self) -> LayeredConfig {
        self.config.clone()
    }

    /// The store a session's transcript and metadata live in.
    #[must_use]
    pub fn session_store(&self) -> SessionStore {
        self.store.clone()
    }

    /// Whether the open workspace is trusted, which is what decides the project
    /// half of every discovery.
    #[must_use]
    pub const fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// The project directories a prompt is resolved from, which are none in an
    /// untrusted workspace.
    #[must_use]
    pub fn project_prompt_roots(&self) -> Vec<PathBuf> {
        if self.project_trusted {
            vec![self.paths.working_directory.join(".vibe/prompts")]
        } else {
            Vec::new()
        }
    }

    /// The per-tool configuration a session's tools resolve through.
    #[must_use]
    pub fn tool_config(&self) -> ToolConfigResolver {
        self.tool_config.clone()
    }

    /// Where a permanent approval writes the patterns it grants.
    ///
    /// Reference `approve_always(save_permanently=True)` merges them into
    /// `tools.<name>.allowlist` through the configuration orchestrator, which is
    /// the same table the tool reads back on the next session; the merge is a
    /// sorted union so a repeated approval writes nothing new.
    #[must_use]
    pub fn allowlist_persistence(&self) -> AllowlistPersistence {
        let config = self.config.clone();
        Arc::new(move |tool: &str, patterns: &[String]| {
            let snapshot = config.load().map_err(|error| error.to_string())?;
            let mut merged = snapshot
                .effective
                .get("tools")
                .and_then(toml::Value::as_table)
                .and_then(|tools| tools.get(tool))
                .and_then(toml::Value::as_table)
                .and_then(|settings| settings.get("allowlist"))
                .and_then(toml::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let before = merged.len();
            merged.extend(patterns.iter().cloned());
            merged.sort();
            merged.dedup();
            if merged.len() == before {
                return Ok(());
            }
            let operation = ConfigPatchOp {
                mutation: ConfigMutation::set(
                    ["tools", tool, "allowlist"],
                    toml::Value::Array(merged.into_iter().map(toml::Value::String).collect()),
                ),
                target: None,
            };
            config
                .apply_patch(&[operation], "permanent tool approval")
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    /// The active model's compaction threshold, or zero when none declares one.
    ///
    /// A client renders context pressure against this number, so an unknown
    /// threshold is published as zero rather than guessed: the reference reports
    /// zero for the same case.
    ///
    /// It is the same number the compaction policy fires on, read through the
    /// same resolver, so what a client renders and what triggers a compaction
    /// can never be two readings of one key. The reader this replaced looked the
    /// active model up in the persisted list form, which a merged configuration
    /// never carries, so it published zero for every real configuration.
    #[must_use]
    pub fn context_window(&self) -> u64 {
        self.compaction_settings().auto_compact_threshold
    }

    /// The five compaction keys, read once for a session that is opening.
    ///
    /// A configuration that cannot be loaded compacts on nothing rather than on
    /// a guess, which is the same answer [`Release3Service::context_window`]
    /// gives for the threshold it publishes.
    #[must_use]
    pub fn compaction_settings(&self) -> CompactionSettings {
        self.config
            .load()
            .map(|snapshot| snapshot.compaction_settings())
            .unwrap_or_default()
    }

    /// The three compaction texts `compaction_prompt_id` resolves to, or the
    /// message saying why the identifier named nothing.
    ///
    /// The chain is the reference's: a `.md` file in a project prompt directory
    /// wins, then one in the user prompt directory, then the built-in. A failure
    /// is carried rather than raised, because the reference resolves lazily and
    /// the operator meets the error when a compaction runs.
    #[must_use]
    pub fn compaction_prompts(&self) -> CompactionPromptResolution {
        CompactionPromptResolution::resolve(
            &self.compaction_settings().compaction_prompt_id,
            &self.config.harness_files().prompts_dirs(),
        )
    }

    /// Brings the configuration files this session reads forward, once.
    ///
    /// A binary calls this at startup, before its first read, which is where
    /// the reference runs `migrate_config_layers`. Constructing the service
    /// does not migrate: a fixture or a test that only composes a
    /// configuration must never rewrite the operator's files.
    ///
    /// The returned warnings name the files a migration could not write; they
    /// also reach every configuration snapshot afterward.
    pub fn migrate_configuration(&self) -> Result<Vec<String>, Release3Error> {
        self.config.migrate_sources().map_err(config_error)
    }

    #[must_use]
    pub fn with_runtime_session_persistence(mut self) -> Self {
        self.persist_runtime_sessions = true;
        self
    }

    #[must_use]
    pub fn for_runtime_session_root(
        session_root: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
    ) -> Self {
        let session_root = session_root.into();
        let vibe_home = session_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| session_root.clone());
        Self::build(
            Release3Paths {
                vibe_home,
                working_directory: working_directory.into(),
                session_root,
            },
            false,
        )
        .with_runtime_session_persistence()
    }

    #[must_use]
    pub const fn persists_runtime_sessions(&self) -> bool {
        self.persist_runtime_sessions
    }

    /// The account as `AccountView` declares it, classified from the
    /// configuration the session runs under.
    ///
    /// Three of the four statuses are decidable locally: a model served by a
    /// backend other than Mistral has no Mistral account behind it
    /// (`unavailable`), a Mistral model whose key variable resolves to nothing
    /// is `missing_key`, and one whose key resolves is `ready`. The fourth,
    /// `unauthorized`, is a console verdict on the key; this port has no client
    /// for that endpoint, so it never claims it.
    ///
    /// The key resolves the way the reference's `resolve_api_key` resolves it:
    /// the environment the dotenv load leaves behind first, then the OS
    /// keyring under the shared service names, so a key stored only in the
    /// keyring reads as `ready` here as it does upstream.
    pub fn account_view(&self) -> Value {
        let upgrade = json!({
            "kind": AccountActionKind::UpgradeToPro,
            "url": format!("{}/code/extensions?focus=key", self.vibe_base_url().trim_end_matches('/')),
        });
        let unavailable = json!({
            "status": AccountStatus::Unavailable,
            "plan": null,
            "planOffer": null,
            "rateLimitAction": null,
            "teleportEligible": false,
            "teleportAction": upgrade,
        });
        let Ok(snapshot) = self.config.load() else {
            return unavailable;
        };
        let Some(provider) = snapshot.active_provider() else {
            return unavailable;
        };
        let mistral = provider
            .get("backend")
            .and_then(TomlValue::as_str)
            .unwrap_or("mistral")
            == "mistral";
        if !mistral {
            return unavailable;
        }
        let variable = provider
            .get("api_key_env_var")
            .and_then(TomlValue::as_str)
            .unwrap_or(MISTRAL_KEY);
        // The credential is resolved the way every other reader resolves one:
        // the process environment with the vibe home's dotenv filling in what it
        // does not set, then the OS keyring. `vibe_environment` cannot serve
        // this, because it keeps only the `VIBE_*` keys the configuration layer
        // is built from.
        let environ = DotenvValues::global(&self.paths.vibe_home).environment();
        let store = vibe_core::auth::KeyringStore::native();
        let configured = vibe_core::auth::resolve_api_key(variable, &environ, &store)
            .is_some_and(|key| !key.is_empty());
        json!({
            "status": if configured { AccountStatus::Ready } else { AccountStatus::MissingKey },
            "plan": null,
            "planOffer": null,
            "rateLimitAction": null,
            "teleportEligible": false,
            "teleportAction": upgrade,
        })
    }

    /// Whether the model new turns run on reads images.
    ///
    /// A configuration that will not load is read as reading them, so a broken
    /// file never makes a client report that a transcript lost its attachments.
    pub fn active_model_supports_images(&self) -> bool {
        self.config
            .load()
            .ok()
            .map(|snapshot| snapshot.config_view()["activeModel"]["supportsImages"] == json!(true))
            .unwrap_or(true)
    }

    /// The base URL an account action points a browser at.
    fn vibe_base_url(&self) -> String {
        self.config
            .load()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .effective
                    .get("vibe_base_url")?
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default()
    }

    /// Whether the configuration asks for sessions to be written to disk.
    ///
    /// A configuration that will not load is read as enabled, matching the
    /// shipped default: a session that is being written is the normal state, and
    /// reporting otherwise would tell a client its transcript is being dropped.
    pub fn session_logging_enabled(&self) -> bool {
        self.config
            .load()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .effective
                    .get("session_logging")?
                    .as_table()?
                    .get("enabled")?
                    .as_bool()
            })
            .unwrap_or(true)
    }

    /// Whether the configuration lets a client-recorded event be kept and
    /// shipped.
    ///
    /// `telemetry/record` is the only caller: the reference hands the event to
    /// the agent loop's telemetry client, which drops it when the same key is
    /// off. An absent key is the shipped default of true; a configuration that
    /// will not load at all reads as false, which is what the reference's
    /// `_is_enabled` answers when its getter raises.
    pub fn telemetry_enabled(&self) -> bool {
        let Ok(snapshot) = self.config.load() else {
            return false;
        };
        snapshot
            .effective
            .get("enable_telemetry")
            .and_then(TomlValue::as_bool)
            .unwrap_or(true)
    }

    /// The provider entry `name` resolves to in the effective configuration,
    /// which is what the setup flow starts from before persisting it back.
    pub fn effective_provider(&self, name: &str) -> Result<Option<toml::Table>, Release3Error> {
        self.config.effective_provider(name).map_err(config_error)
    }

    /// Upserts one provider entry keyed by name, answering whether a write
    /// happened: a provider identical to what the configuration already
    /// resolves is not written at all.
    pub fn persist_provider(&self, provider: &toml::Table) -> Result<bool, Release3Error> {
        self.config
            .persist_provider(provider)
            .map(|written| written.is_some())
            .map_err(config_error)
    }

    /// Opens `roots` alongside the working directory.
    ///
    /// The added directories are project roots in their own right: the
    /// configuration store deduplicates them and extension discovery reads
    /// each one's `.vibe` directory, which is what `--add-dir` does upstream.
    #[must_use]
    pub fn with_allowed_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.allowed_roots.extend(roots);
        self.config = self
            .config
            .clone()
            .with_additional_roots(self.allowed_roots.clone());
        self.discovery_roots.project = project_discovery_roots(&self.config, self.project_trusted);
        self
    }

    pub fn dispatch(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        match method {
            // `load` always reads from disk, so reading and reloading are the
            // same operation. They answer differently: a read publishes the two
            // configuration views, a reload publishes the runtime the server
            // fills in around this dispatch.
            "config/read" => self.config_read(),
            "config/reload" => Ok(Release3Dispatch::result([] as [(&str, Value); 0])),
            // Reference `config_schema_response`: the version token lets a
            // client cache the surface instead of refetching it.
            "config/schema" => Ok(Release3Dispatch::result([
                (
                    "configSchemaVersion",
                    Value::from(LayeredConfig::schema_version()),
                ),
                ("schema", LayeredConfig::schema()),
            ])),
            // Reference `_config_patch`: the client addresses a field by
            // pointer and never names the file behind it.
            "config/patch" => self.config_patch(params),
            "config/fields/read" => self.config_fields_read(),
            // Retained as a local alias over the same patch core so the callers
            // that predate `config/patch` keep working. Recorded as a
            // divergence in `tasks/prd-config-parity.md`.
            "config/batchWrite" => self.config_batch_write(params),
            "config/thinking/write" => self.thinking_write(params),
            "config/proxy/write" => self.proxy_write(params),
            "config/proxy/read" => self.proxy_read(),
            "session/list" => self.session_list(params),
            "history/list" => self.history_list(params),
            "session/log/read" => self.session_log(params),
            "session/resume" => self.resume(params),
            "session/continue" => self.continue_session(params),
            "session/fork" => self.fork(params),
            "session/title/update" => self.title_update(params),
            "session/delete" => self.delete(params),
            "session/rewind" => self.rewind(params),
            "session/rewind/read" => self.rewind_read(params),
            "session/history/clear" => self.history_clear(params),
            "agents/list" => self.agents_list(),
            "agents/install" => self.agent_install(params),
            "agents/uninstall" => self.agent_uninstall(params),
            "session/agent/update" => self.agent_update(params),
            "skills/list" => self.skills_list(),
            "workspace/prompt/prepare" => self.prompt_prepare(params),
            _ => Err(Release3Error::MethodNotFound(method.to_owned())),
        }
    }

    pub fn dispatch_scoped(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
        working_directory: PathBuf,
        project_trusted: bool,
    ) -> Result<Release3Dispatch, Release3Error> {
        let mut scoped = self.clone();
        scoped.config = self
            .config
            .scoped_to_working_directory(working_directory, project_trusted);
        scoped.dispatch(method, params)
    }

    pub fn mcp_servers_for_session(
        &self,
        working_directory: &Path,
        project_trusted: bool,
        runtime_servers: &[Value],
    ) -> Result<Vec<McpServerConfig>, Release3Error> {
        if !project_trusted && !runtime_servers.is_empty() {
            return Err(Release3Error::InvalidParams(
                "runtime MCP servers require a trusted workspace".to_owned(),
            ));
        }
        let mut runtime = Table::new();
        if !runtime_servers.is_empty() {
            runtime.insert(
                "mcp_servers".to_owned(),
                TomlValue::try_from(Value::Array(runtime_servers.to_vec()))
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?,
            );
        }
        let snapshot = LayeredConfig::new(
            ConfigPaths {
                vibe_home: self.paths.vibe_home.clone(),
                working_directory: working_directory.to_path_buf(),
            },
            self.defaults.clone(),
        )
        .with_environment(vibe_environment(&self.paths.vibe_home))
        .with_runtime_overrides(runtime)
        .with_project_trusted(project_trusted)
        .load()
        .map_err(config_error)?;
        snapshot
            .mcp_servers(working_directory)
            .map_err(config_error)
    }

    /// The `enabled_tools` and `disabled_tools` lists the configuration carries
    /// for a session opened in `working_directory`, in that order.
    ///
    /// The reference reads them from the same layered configuration the session
    /// runs under, so the project file of the directory being opened counts,
    /// not the one the service was constructed with.
    pub fn tool_filters_for_session(
        &self,
        working_directory: &Path,
        project_trusted: bool,
    ) -> Result<(Vec<String>, Vec<String>), Release3Error> {
        let snapshot = self
            .config
            .clone()
            .scoped_to_working_directory(working_directory.to_path_buf(), project_trusted)
            .load()
            .map_err(config_error)?;
        Ok((snapshot.enabled_tools(), snapshot.disabled_tools()))
    }
}

fn config_error(error: vibe_core::config::ConfigError) -> Release3Error {
    Release3Error::Config(error.to_string())
}

fn storage_error(error: vibe_core::storage::StorageError) -> Release3Error {
    let message = error.to_string();
    match error {
        vibe_core::storage::StorageError::NoSessions
        | vibe_core::storage::StorageError::SessionNotFound(_)
        | vibe_core::storage::StorageError::AmbiguousSession(_) => Release3Error::NotFound(message),
        _ => Release3Error::Storage(message),
    }
}

impl From<params::ParamError> for Release3Error {
    fn from(error: params::ParamError) -> Self {
        Self::InvalidParams(error.message())
    }
}

#[derive(Debug, Error)]
pub enum Release3Error {
    #[error("unknown release-3 method `{0}`")]
    MethodNotFound(String),
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("configuration failed: {0}")]
    Config(String),
    #[error("session storage failed: {0}")]
    Storage(String),
    #[error("extension failed: {0}")]
    Extension(String),
    #[error("prompt preparation failed: {0}")]
    Prompt(String),
    #[error("release-3 state lock is poisoned")]
    StatePoisoned,
    #[error("JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod release3_tests;
