use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::builtin_agents;
use crate::host::now_millis;
use crate::params;
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
    SkillDefinition, discover_extensions,
};
use vibe_core::mcp::McpServerConfig;
use vibe_core::middleware::CompactionSettings;
use vibe_core::policy::AllowlistPersistence;
use vibe_core::prompt::{
    InstructionLoader, PromptComposition, PromptResolver, SkillSummary, SubagentSummary,
    UserResource, prepare_user_resources,
};
use vibe_core::skills::skill_summary;
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
        // does not set. `vibe_environment` cannot serve this, because it keeps
        // only the `VIBE_*` keys the configuration layer is built from.
        let configured = DotenvValues::global(&self.paths.vibe_home)
            .environment()
            .get(variable)
            .is_some_and(|key| !key.trim().is_empty());
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

    /// Whether the configuration lets a client-recorded event be kept.
    ///
    /// `telemetry/record` is the only caller: the reference hands the event to
    /// the agent loop's telemetry client, which drops it when the same key is
    /// off. A configuration that will not load is read as enabled, matching the
    /// shipped default.
    pub fn telemetry_enabled(&self) -> bool {
        self.config
            .load()
            .ok()
            .and_then(|snapshot| snapshot.effective.get("enable_telemetry")?.as_bool())
            .unwrap_or(true)
    }

    pub(crate) fn message_count(&self, session_id: &str) -> Result<Option<usize>, Release3Error> {
        match self.store.load(session_id) {
            Ok(hydrated) => Ok(Some(hydrated.messages.len())),
            Err(StorageError::SessionNotFound(_)) => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }

    pub(crate) fn snapshot_session(
        &self,
        session_id: &str,
    ) -> Result<HydratedSession, Release3Error> {
        self.store.load(session_id).map_err(storage_error)
    }

    /// Where `entry_id` sits in the stored message list.
    ///
    /// # Errors
    ///
    /// Reports the session storage failure, and answers `NotFound` when no
    /// rewindable user entry carries the identifier.
    pub(crate) fn rewind_entry_index(
        &self,
        session_id: &str,
        entry_id: &str,
    ) -> Result<usize, Release3Error> {
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        rewind_entry_index(&hydrated.messages, entry_id)
    }

    pub(crate) fn rollback_rewind(
        &self,
        source: HydratedSession,
        result_session_id: &str,
    ) -> Result<(), Release3Error> {
        let mut failures = Vec::new();
        if result_session_id == source.metadata.id {
            let mut metadata = source.metadata.clone();
            match self.store.replace_messages(
                &mut metadata,
                &source.messages,
                source.metadata.updated_at_ms,
            ) {
                Ok(()) => {
                    if let Err(error) = self.store.update_metadata(&source.metadata) {
                        failures.push(error.to_string());
                    }
                }
                Err(error) => failures.push(error.to_string()),
            }
        } else {
            match self.store.delete(result_session_id) {
                Ok(()) | Err(StorageError::SessionNotFound(_)) => {}
                Err(error) => failures.push(error.to_string()),
            }
            if let Err(error) = self.continuity.remove(result_session_id) {
                failures.push(error.to_string());
            }
        }
        if let Err(error) = self.store.select_for_continue(&source.metadata.id) {
            failures.push(error.to_string());
        }
        if let Err(error) = self.continuity.refresh(source) {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Release3Error::Storage(failures.join("; ")))
        }
    }

    pub(crate) fn rewind_after_workspace_restore(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        self.rewind_impl(params, true)
    }

    pub fn update_runtime_settings(
        &self,
        session_id: &str,
        settings: &BTreeMap<String, Value>,
    ) -> Result<Option<HydratedSession>, Release3Error> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        let mut hydrated = self.store.load(session_id).map_err(storage_error)?;
        hydrated.metadata.config.extend(settings.clone());
        hydrated.metadata.updated_at_ms = now_millis();
        self.store
            .update_metadata(&hydrated.metadata)
            .map_err(storage_error)?;
        hydrated.current_config = hydrated.metadata.config.clone();
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(Some(hydrated))
    }

    pub fn create_runtime_session(
        &self,
        session_id: &str,
        working_directory: &str,
        now_ms: u64,
    ) -> Result<HydratedSession, Release3Error> {
        match self.store.load(session_id) {
            Ok(_) => {
                return Err(Release3Error::Storage(format!(
                    "session `{session_id}` already exists"
                )));
            }
            Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => return Err(storage_error(error)),
        }
        self.store
            .create(session_id, working_directory, None, now_ms)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated)
    }

    pub fn update_runtime_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<HydratedSession>, Release3Error> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        self.set_session_agent(session_id, name)
            .map(|(_, hydrated)| Some(hydrated))
    }

    pub(crate) fn agent_profile(&self, name: &str) -> Result<AgentProfile, Release3Error> {
        if name == "lean" && !self.installed_agent_names()?.contains(name) {
            return Err(Release3Error::InvalidParams(
                "agent `lean` must be installed with /leanstall first".to_owned(),
            ));
        }
        let profile = self.catalog().agents.remove(name).map_or_else(
            || {
                if self.persist_runtime_sessions {
                    Err(Release3Error::Extension(format!(
                        "agent `{name}` was not found"
                    )))
                } else {
                    Ok(AgentProfile {
                        name: name.to_owned(),
                        display_name: name.to_owned(),
                        description: "Externally supplied agent profile".to_owned(),
                        kind: AgentKind::Agent,
                        safety: "neutral".to_owned(),
                        overrides: Table::new(),
                        source: ExtensionSource::Configured,
                        path: None,
                    })
                }
            },
            Ok,
        )?;
        if profile.kind != AgentKind::Agent {
            return Err(Release3Error::Extension(format!(
                "subagent `{name}` cannot be selected as the primary agent"
            )));
        }
        if let Some(prompt_id) = profile.runtime_settings().system_prompt_id
            && builtin_agents::system_prompt(&prompt_id).is_none()
        {
            return Err(Release3Error::InvalidParams(format!(
                "agent `{name}` references unsupported system prompt `{prompt_id}`"
            )));
        }
        Ok(profile)
    }

    pub(crate) fn default_agent_name(&self) -> Result<String, Release3Error> {
        Ok(self
            .config
            .load()
            .map_err(config_error)?
            .effective
            .get("default_agent")
            .and_then(TomlValue::as_str)
            .unwrap_or("default")
            .to_owned())
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

    pub fn close_saved_session(&self, session_id: &str, now_ms: u64) -> Result<(), Release3Error> {
        match self.store.close(session_id, now_ms) {
            Ok(_) | Err(StorageError::SessionNotFound(_)) => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
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

    /// The two configuration views `ConfigReadResponse` declares.
    ///
    /// No agent overlay is applied to the published configuration here, so both
    /// views are the same document; the field stays on the wire because a
    /// client renders "changed from the base" from it.
    fn config_read(&self) -> Result<Release3Dispatch, Release3Error> {
        let view = self.config.load().map_err(config_error)?.config_view();
        Ok(Release3Dispatch::result([
            ("config", view.clone()),
            ("baseConfig", view),
        ]))
    }

    /// The whole configuration, with every layer and target it was composed
    /// from.
    ///
    /// This is not a wire shape: `ConfigReadResponse` publishes a narrower view
    /// and declares no room for the rest. It stays for the in-process readers
    /// that need the effective document, chiefly the settings screen.
    pub fn config_document(&self) -> Result<Value, Release3Error> {
        Ok(self.config.load().map_err(config_error)?.public_view())
    }

    /// Writes one or more addressed fields, routing each to the file the client
    /// named or, failing that, to the writable target the selection resolves to.
    ///
    /// The response splits the two ways a patch can fail the way
    /// `ConfigPatchResponse` splits them: `rejected` for a request the
    /// merged-configuration preflight refused, which leaves every file
    /// byte-identical, and `failures` for a target whose write did not land
    /// while another one did. The server fills in the runtime the patch
    /// produced, which is what a client reads the new values from.
    ///
    /// `reloadRuntime` is accepted and has no effect: `config/read` and
    /// `config/reload` both compose from disk on every call here, so there is no
    /// cached runtime a patch could leave stale.
    fn config_patch(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let raw = params
            .get("ops")
            .and_then(Value::as_array)
            .ok_or_else(|| Release3Error::InvalidParams("ops must be an array".to_owned()))?;
        let operations = raw
            .iter()
            .map(parse_config_patch_op)
            .collect::<Result<Vec<_>, _>>()?;
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("config screen edit");
        let outcome = match self.config.apply_patch(&operations, reason) {
            Ok(outcome) => outcome,
            Err(vibe_core::config::ConfigError::PatchRejected(_)) => {
                return Ok(Release3Dispatch::result([
                    ("rejected", Value::Bool(true)),
                    ("failures", json!([])),
                ]));
            }
            Err(error) => return Err(config_error(error)),
        };
        Ok(Release3Dispatch::result([
            ("rejected", Value::Bool(false)),
            ("failures", json!(outcome.failures)),
        ]))
    }

    /// Describes every published field so a settings screen renders without
    /// hard-coding the surface. Reference `_config_fields_read`.
    fn config_fields_read(&self) -> Result<Release3Dispatch, Release3Error> {
        let described = self.config.describe_fields().map_err(config_error)?;
        Ok(Release3Dispatch::result([
            ("fields", json!(described.fields)),
            ("targets", json!(described.targets)),
        ]))
    }

    fn config_batch_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let raw = params
            .get("writes")
            .and_then(Value::as_array)
            .ok_or_else(|| Release3Error::InvalidParams("writes must be an array".to_owned()))?;
        let mut writes = raw
            .iter()
            .map(parse_config_write)
            .collect::<Result<Vec<_>, _>>()?;
        // A caller that names no fingerprint means "write on top of what is on
        // disk now" rather than "the file must not exist", which is how the
        // addressed writes read an absent one too. The check still stands: the
        // fingerprint is taken here and compared inside the transaction.
        let snapshot = self.config.load().map_err(config_error)?;
        for write in &mut writes {
            if write.expected_fingerprint.is_none() {
                write.expected_fingerprint =
                    snapshot.fingerprints.get(&write.target).cloned().flatten();
            }
        }
        let snapshot = self.config.batch_write(&writes).map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "snapshot",
            snapshot.public_view(),
        )]))
    }

    /// Writes the thinking level, which the reference addresses by name rather
    /// than through the patch surface.
    ///
    /// The answer carries nothing of its own: the server publishes the runtime
    /// the write produced, which is what `ConfigMutationResponse` declares.
    fn thinking_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .ok_or_else(|| Release3Error::InvalidParams("level is required".to_owned()))?;
        if !THINKING_LEVELS.contains(&level) {
            return Err(Release3Error::InvalidParams(format!(
                "level must be one of {}",
                THINKING_LEVELS.join(", ")
            )));
        }
        let target = parse_target(
            params
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("user"),
        )?;
        let snapshot = self.config.load().map_err(config_error)?;
        let expected_fingerprint = params
            .get("expectedFingerprint")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| snapshot.fingerprints.get(&target).cloned().flatten());
        self.config
            .batch_write(&[ConfigWrite {
                target,
                expected_fingerprint,
                mutations: vec![ConfigMutation::set(
                    ["thinking"],
                    TomlValue::String(level.to_owned()),
                )],
            }])
            .map_err(config_error)?;
        Ok(Release3Dispatch::result([] as [(&str, Value); 0]))
    }

    fn proxy_read(&self) -> Result<Release3Dispatch, Release3Error> {
        let values = ProxyEnvironmentStore::new(&self.paths.vibe_home)
            .read()
            .map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "settings",
            json!({
                "values": ProxyKey::ALL.into_iter().map(|key| {
                    (key.as_str().to_owned(), values.get(&key).cloned().map(Value::String).unwrap_or(Value::Null))
                }).collect::<Map<_, _>>(),
                "descriptions": ProxyKey::ALL.into_iter().map(|key| {
                    (key.as_str().to_owned(), json!(key.description()))
                }).collect::<Map<_, _>>(),
            }),
        )]))
    }

    fn proxy_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let changes = params
            .get("changes")
            .and_then(Value::as_object)
            .ok_or_else(|| Release3Error::InvalidParams("changes must be an object".to_owned()))?;
        let mut parsed = BTreeMap::new();
        for (key, value) in changes {
            let key = ProxyKey::try_from(key.as_str())
                .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
            let value = match value {
                Value::Null => None,
                Value::String(value) if value.is_empty() => None,
                Value::String(value) if !value.contains(['\n', '\r', '\0']) => Some(value.clone()),
                Value::String(_) => {
                    return Err(Release3Error::InvalidParams(format!(
                        "proxy value for `{}` contains a forbidden control character",
                        key.as_str()
                    )));
                }
                _ => {
                    return Err(Release3Error::InvalidParams(format!(
                        "proxy value for `{}` must be a string or null",
                        key.as_str()
                    )));
                }
            };
            parsed.insert(key, value);
        }
        if !parsed.is_empty() {
            ProxyEnvironmentStore::new(&self.paths.vibe_home)
                .write(&parsed)
                .map_err(config_error)?;
        }
        Ok(Release3Dispatch::result([] as [(&str, Value); 0]))
    }

    fn session_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let offset = usize_param(params, "offset", 0)?;
        let limit = usize_param(params, "limit", 50)?;
        let cwd = params.get("cwd").and_then(Value::as_str);
        // The legacy migration still runs before the page is read, so a store
        // written by an older layout is listed; what it moved is not published,
        // because `SessionListResponse` declares the page and nothing else.
        self.store.migrate_legacy().map_err(storage_error)?;
        let page = self.store.list(cwd, offset, limit).map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "sessions",
            serde_json::to_value(page.sessions)?,
        )]))
    }

    fn history_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let history = self
            .store
            .history(
                session_id,
                usize_param(params, "offset", 0)?,
                usize_param(params, "limit", 100)?,
            )
            .map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "history",
            serde_json::to_value(history)?,
        )]))
    }

    fn session_log(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .load(required_string(params, "sessionId")?)
            .map_err(storage_error)?;
        Ok(hydrated_result(&hydrated, None))
    }

    fn resume(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .resume(
                required_string(params, "sessionId")?,
                optional_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn continue_session(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let cwd = optional_string(params, "cwd")
            .unwrap_or_else(|| self.paths.working_directory.to_string_lossy().into_owned());
        let hydrated = self
            .store
            .continue_session(
                &cwd,
                optional_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn fork(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        let source = required_string(params, "sessionId")?;
        let keep_messages = fork_keep_messages(params)?;
        let new_id = optional_string(params, "newSessionId").unwrap_or_else(|| {
            format!(
                "session-{}-{}",
                now_millis(),
                self.next_session.fetch_add(1, Ordering::Relaxed)
            )
        });
        let mut hydrated = self
            .store
            .fork(
                source,
                &new_id,
                &optional_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
                now_millis(),
            )
            .map_err(storage_error)?;
        if let Some(keep_messages) = keep_messages {
            hydrated = self
                .store
                .rewind(
                    &hydrated.metadata.id,
                    keep_messages,
                    hydrated.metadata.statistics.clone(),
                    now_millis(),
                )
                .map_err(storage_error)?;
        }
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn title_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let metadata = self
            .store
            .update_title(
                required_string(params, "sessionId")?,
                required_string(params, "title")?,
                now_millis(),
            )
            .map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "metadata",
            serde_json::to_value(metadata)?,
        )]))
    }

    fn delete(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let snapshot = match self.store.load(session_id) {
            Ok(snapshot) => Some(snapshot),
            Err(StorageError::SessionNotFound(_)) => None,
            Err(error) => return Err(storage_error(error)),
        };
        self.continuity
            .remove(session_id)
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        match self.store.delete(session_id) {
            Ok(()) | Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => {
                if let Some(snapshot) = snapshot
                    && let Err(rollback) = self.continuity.refresh(snapshot)
                {
                    return Err(Release3Error::Storage(format!(
                        "session delete failed ({error}); continuity rollback failed ({rollback})"
                    )));
                }
                return Err(storage_error(error));
            }
        }
        Ok(Release3Dispatch::result([("deleted", json!(true))]))
    }

    fn rewind(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        self.rewind_impl(params, false)
    }

    fn rewind_impl(
        &self,
        params: &BTreeMap<String, Value>,
        workspace_restore_handled: bool,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let entry_id = required_string(params, "entryId")?;
        let restore_files = params
            .get("restoreFiles")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    Release3Error::InvalidParams("restoreFiles must be a boolean".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(false);
        if restore_files && !workspace_restore_handled {
            return Err(Release3Error::InvalidParams(
                "this session has no restorable file checkpoint".to_owned(),
            ));
        }
        let inplace = params
            .get("inplace")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    Release3Error::InvalidParams("inplace must be a boolean".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(false);
        let source = self.store.load(session_id).map_err(storage_error)?;
        let keep_messages = rewind_entry_index(&source.messages, entry_id)?;
        let message = source
            .messages
            .get(keep_messages)
            .and_then(|message| match message {
                ModelMessage::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let requested_statistics = statistics_map(params.get("statistics"))?;
        let rewind_statistics = if requested_statistics.is_empty() {
            source.metadata.statistics.clone()
        } else {
            requested_statistics
        };
        let timestamp = now_millis();
        let hydrated = if inplace {
            self.store
                .rewind(session_id, keep_messages, rewind_statistics, timestamp)
                .map_err(storage_error)?
        } else {
            let new_id = format!(
                "session-{}-{}",
                timestamp,
                self.next_session.fetch_add(1, Ordering::Relaxed)
            );
            self.store
                .fork_rewound(
                    session_id,
                    &new_id,
                    keep_messages,
                    rewind_statistics,
                    timestamp,
                )
                .map_err(storage_error)?
        };
        if let Err(error) = self.continuity.refresh(hydrated.clone()) {
            if let Err(rollback) = self.rollback_rewind(source, &hydrated.metadata.id) {
                return Err(Release3Error::Storage(format!(
                    "continuity refresh failed ({error}); rewind rollback failed ({rollback})"
                )));
            }
            return Err(Release3Error::Storage(error.to_string()));
        }
        // `SessionRewindResponse` declares five fields and this service can
        // answer three of them: `state` and `sessionLog` are composed from the
        // live session the attachment below rebinds, which only the server
        // holds. The two lists are placeholders the workspace restore replaces.
        Ok(Release3Dispatch {
            result: [
                ("message".to_owned(), json!(message)),
                ("restoreErrors".to_owned(), json!([])),
                ("restoredPaths".to_owned(), json!([])),
            ]
            .into_iter()
            .collect(),
            attachment: Some(runtime_attachment(&hydrated)),
        })
    }

    /// Whether rewinding to one history entry would change files, and which.
    ///
    /// The entry is resolved here so an identifier no rewindable message
    /// carries is refused before anything reads a workspace. The two fields are
    /// answered empty: the paths come from the session's checkpoint log, which
    /// the server holds and fills in.
    fn rewind_read(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let entry_id = required_string(params, "entryId")?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        rewind_entry_index(&hydrated.messages, entry_id)?;
        Ok(Release3Dispatch::result([
            ("hasFileChanges", json!(false)),
            ("paths", json!([])),
        ]))
    }

    fn history_clear(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .rewind(
                required_string(params, "sessionId")?,
                0,
                BTreeMap::new(),
                now_millis(),
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    /// The agent catalog as `AgentsListResponse` declares it.
    ///
    /// `active` is the agent a fresh session would run, which the server
    /// replaces with the one the addressed session actually runs. It is always
    /// published: the field is required, and a client that cannot resolve it
    /// has no agent to render as selected.
    fn agents_list(&self) -> Result<Release3Dispatch, Release3Error> {
        let profiles = self.available_agents()?;
        let active = self
            .default_agent_name()
            .ok()
            .and_then(|name| {
                profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .cloned()
            })
            .or_else(|| profiles.first().cloned())
            .unwrap_or_else(builtin_agents::default_profile);
        Ok(Release3Dispatch::result([
            ("active", agent_summary(&active)),
            (
                "agents",
                Value::Array(profiles.iter().map(agent_summary).collect()),
            ),
        ]))
    }

    /// Every agent profile a session may run, with the uninstalled builtins
    /// filtered out.
    fn available_agents(&self) -> Result<Vec<AgentProfile>, Release3Error> {
        let installed = self.installed_agent_names()?;
        Ok(self
            .catalog()
            .agents
            .into_values()
            .filter(|profile| profile.name != "lean" || installed.contains("lean"))
            .collect())
    }

    /// The configuration, catalogs and diagnostics `RuntimeSnapshot` carries.
    ///
    /// The server owns the rest of the snapshot: the tool surface, the
    /// integrations and the session's own accounting. `active_agent` names the
    /// profile the session runs, which this service cannot know on its own.
    pub fn runtime_projection(&self, active_agent: Option<&str>) -> RuntimeProjection {
        let snapshot = self.config.load().ok();
        let view = snapshot
            .as_ref()
            .map_or_else(|| Value::Object(Map::new()), ConfigSnapshot::config_view);
        let catalog = self.catalog();
        let installed = self.installed_agent_names().unwrap_or_default();
        let profiles = catalog
            .agents
            .values()
            .filter(|profile| profile.name != "lean" || installed.contains("lean"))
            .cloned()
            .collect::<Vec<_>>();
        let active = active_agent
            .and_then(|name| {
                profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .cloned()
            })
            .or_else(|| {
                let default = self.default_agent_name().ok()?;
                profiles
                    .iter()
                    .find(|profile| profile.name == default)
                    .cloned()
            })
            .unwrap_or_else(builtin_agents::default_profile);
        RuntimeProjection {
            // No agent overlay is applied to the published configuration here,
            // so the two views are the same document, as the reference host
            // path also answers with.
            base_config: view.clone(),
            config: view,
            active_agent: agent_summary(&active),
            agents: profiles.iter().map(agent_summary).collect(),
            skills: catalog.skills.values().map(skill_summary).collect(),
            issues: catalog
                .issues
                .iter()
                .map(|issue| {
                    json!({
                        "file": issue.path.to_string_lossy(),
                        "message": issue.message,
                    })
                })
                .collect(),
            hooks_count: catalog.hooks.len(),
        }
    }

    fn agent_install(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, true)?;
            return self.agents_list();
        }
        let source = self.authorized_existing_path(Path::new(required_string(params, "path")?))?;
        self.agents
            .lock()
            .map_err(|_| Release3Error::StatePoisoned)?
            .install(&source)
            .map_err(|error| Release3Error::Extension(error.to_string()))?;
        // Both forms answer with the catalog the change produced, which is what
        // `AgentsListResponse` declares and what a client re-renders from.
        self.agents_list()
    }

    fn agent_uninstall(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, false)?;
            let mut dispatch = self.agents_list()?;
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str)
                && self
                    .store
                    .load(session_id)
                    .map_err(storage_error)?
                    .metadata
                    .agent_profile
                    .as_ref()
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            {
                // The session was running the agent that just went away, so it
                // falls back to the default and the answer names it as active.
                let (profile, hydrated) = self.set_session_agent(session_id, "default")?;
                dispatch
                    .result
                    .insert("active".to_owned(), agent_summary(&profile));
                dispatch.attachment = Some(runtime_attachment(&hydrated));
            }
            return Ok(dispatch);
        }
        self.agents
            .lock()
            .map_err(|_| Release3Error::StatePoisoned)?
            .uninstall(required_string(params, "name")?)
            .map_err(|error| Release3Error::Extension(error.to_string()))?;
        self.agents_list()
    }

    fn set_builtin_agent_installed(&self, name: &str, install: bool) -> Result<(), Release3Error> {
        if name != "lean" {
            return Err(Release3Error::InvalidParams(format!(
                "unknown installable built-in agent `{name}`"
            )));
        }
        let snapshot = self.config.load().map_err(config_error)?;
        let mut installed = snapshot
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        if install {
            installed.insert(name.to_owned());
        } else {
            installed.remove(name);
        }
        let expected_fingerprint = snapshot
            .fingerprints
            .get(&snapshot.selected_target)
            .cloned()
            .flatten();
        self.config
            .batch_write(&[ConfigWrite {
                target: snapshot.selected_target,
                expected_fingerprint,
                mutations: vec![ConfigMutation::set(
                    ["installed_agents"],
                    TomlValue::Array(installed.iter().cloned().map(TomlValue::String).collect()),
                )],
            }])
            .map_err(config_error)?;
        Ok(())
    }

    fn installed_agent_names(&self) -> Result<BTreeSet<String>, Release3Error> {
        Ok(self
            .config
            .load()
            .map_err(config_error)?
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect())
    }

    fn set_session_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(AgentProfile, HydratedSession), Release3Error> {
        let profile = self.agent_profile(name)?;
        let mut metadata = self.store.load(session_id).map_err(storage_error)?.metadata;
        metadata.agent_profile = Some(serde_json::to_value(&profile)?);
        self.store
            .update_metadata(&metadata)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok((profile, hydrated))
    }

    fn agent_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let name = required_string(params, "name")?;
        let (profile, hydrated) = self.set_session_agent(session_id, name)?;
        Ok(Release3Dispatch {
            result: [("agent".to_owned(), serde_json::to_value(profile)?)]
                .into_iter()
                .collect(),
            attachment: Some(runtime_attachment(&hydrated)),
        })
    }

    /// The skill catalog as `SkillsListResponse` declares it.
    ///
    /// The discovery issues the catalog also carries are published on
    /// `runtime/read` rather than here, which is where the reference reports
    /// them and the only shape this response accepts.
    fn skills_list(&self) -> Result<Release3Dispatch, Release3Error> {
        let catalog = self.catalog();
        Ok(Release3Dispatch::result([(
            "skills",
            Value::Array(catalog.skills.values().map(skill_summary).collect()),
        )]))
    }

    fn prompt_prepare(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let prompt: PromptParams = serde_json::from_value(Value::Object(
            params.clone().into_iter().collect::<Map<_, _>>(),
        ))
        .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
        let additional_directories = prompt
            .add_directories
            .iter()
            .map(|path| self.authorized_existing_path(path))
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = self.catalog();
        let user_home = self
            .paths
            .vibe_home
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.vibe_home.clone());
        let project_roots = if self.project_trusted {
            vec![(
                self.paths.working_directory.clone(),
                self.paths.working_directory.clone(),
            )]
        } else {
            Vec::new()
        };
        let loader = InstructionLoader::new(user_home, project_roots);
        let base = match prompt.prompt_id.as_deref() {
            Some(prompt_id) => {
                PromptResolver::new(
                    vec![self.paths.working_directory.join(".vibe/prompts")],
                    vec![self.paths.vibe_home.join("extensions/prompts")],
                    BTreeMap::new(),
                    self.project_trusted,
                )
                .resolve(prompt_id)
                .map_err(|error| Release3Error::Prompt(error.to_string()))?
                .content
            }
            None => prompt.base,
        };
        let composition = PromptComposition {
            base,
            headless: prompt.headless,
            commit_policy: prompt.commit_policy,
            model_info: prompt.model,
            os_tool_guidance: prompt.os_tool_guidance,
            skills: catalog
                .skills
                .values()
                .map(|skill| SkillSummary {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    path: skill.path.clone(),
                })
                .collect(),
            subagents: catalog
                .agents
                .values()
                .filter(|agent| agent.kind == AgentKind::Subagent)
                .map(|agent| SubagentSummary {
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                })
                .collect(),
            scratchpad: prompt.scratchpad,
            project_context: prompt.project_context,
            project_context_stale: prompt.project_context_stale,
            additional_directories: additional_directories.clone(),
            user_instructions: loader
                .user_document()
                .map_err(|error| Release3Error::Prompt(error.to_string()))?,
            project_instructions: loader
                .project_documents()
                .map_err(|error| Release3Error::Prompt(error.to_string()))?,
        }
        .compose();
        let mut roots = vec![self.paths.working_directory.clone()];
        roots.extend(additional_directories);
        let prepared = prepare_user_resources(&prompt.resources, &roots, prompt.supports_images)
            .map_err(|error| Release3Error::Prompt(error.to_string()))?;
        Ok(Release3Dispatch::result([
            ("prompt", serde_json::to_value(composition)?),
            ("user", serde_json::to_value(prepared)?),
            ("issues", serde_json::to_value(catalog.issues)?),
        ]))
    }

    fn authorized_existing_path(&self, path: &Path) -> Result<PathBuf, Release3Error> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.paths.working_directory.join(path)
        };
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            Release3Error::InvalidParams(format!(
                "authorized path `{}` cannot be resolved: {error}",
                candidate.display()
            ))
        })?;
        let authorized = self
            .allowed_roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|allowed| canonical.starts_with(allowed)));
        if !authorized {
            return Err(Release3Error::InvalidParams(format!(
                "path `{}` is outside server-authorized workspace roots",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    fn catalog(&self) -> ExtensionCatalog {
        let builtin_agents = self
            .agents
            .lock()
            .ok()
            .map(|agents| {
                agents
                    .list()
                    .into_iter()
                    .filter(|agent| agent.source == ExtensionSource::Builtin)
                    .map(|agent| (agent.name.clone(), agent.clone()))
                    .collect()
            })
            .unwrap_or_default();
        discover_extensions(
            &self.discovery_roots,
            builtin_agents,
            BTreeMap::<String, SkillDefinition>::new(),
            BTreeMap::new(),
        )
    }
}

fn fork_keep_messages(params: &BTreeMap<String, Value>) -> Result<Option<usize>, Release3Error> {
    let explicit = params
        .get("keepMessages")
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    Release3Error::InvalidParams(
                        "keepMessages must be a non-negative integer".to_owned(),
                    )
                })
        })
        .transpose()?;
    let anchored = params
        .get("messageId")
        .map(|value| {
            let message_id = value.as_str().ok_or_else(|| {
                Release3Error::InvalidParams("messageId must be a string".to_owned())
            })?;
            let index = message_id
                .strip_prefix("history-")
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| {
                    Release3Error::InvalidParams(
                        "messageId must use the stable `history-N` form".to_owned(),
                    )
                })?;
            index.checked_add(1).ok_or_else(|| {
                Release3Error::InvalidParams("messageId index is too large".to_owned())
            })
        })
        .transpose()?;
    match (explicit, anchored) {
        (Some(explicit), Some(anchored)) if explicit != anchored => {
            Err(Release3Error::InvalidParams(
                "keepMessages and messageId identify different fork anchors".to_owned(),
            ))
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptParams {
    #[serde(default)]
    base: String,
    #[serde(default)]
    prompt_id: Option<String>,
    #[serde(default)]
    headless: bool,
    #[serde(default)]
    commit_policy: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    os_tool_guidance: Option<String>,
    #[serde(default)]
    scratchpad: Option<PathBuf>,
    #[serde(default)]
    project_context: Option<String>,
    #[serde(default)]
    project_context_stale: bool,
    #[serde(default)]
    add_directories: Vec<PathBuf>,
    #[serde(default)]
    resources: Vec<UserResource>,
    #[serde(default)]
    supports_images: bool,
}

/// Reads one `ConfigPatchOpWire`: a `set` or `remove` verb, a JSON Pointer, the
/// value a `set` carries, and the file a client pinned the operation to.
fn parse_config_patch_op(value: &Value) -> Result<ConfigPatchOp, Release3Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Release3Error::InvalidParams("each op must be an object".to_owned()))?;
    let raw_path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Release3Error::InvalidParams("op.path must be a string".to_owned()))?;
    let pointer = JsonPointer::parse(raw_path)
        .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
    let target = object
        .get("targetLayer")
        .and_then(Value::as_str)
        .map(parse_target)
        .transpose()?;
    let operation = match object.get("op").and_then(Value::as_str) {
        Some("set") => {
            let raw = object.get("value").cloned().unwrap_or(Value::Null);
            PatchOperation::Set(
                TomlValue::try_from(raw)
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?,
            )
        }
        Some("remove") => PatchOperation::Remove,
        _ => {
            return Err(Release3Error::InvalidParams(
                "op.op must be set or remove".to_owned(),
            ));
        }
    };
    Ok(ConfigPatchOp {
        mutation: ConfigMutation::new(pointer, operation),
        target,
    })
}

fn parse_config_write(value: &Value) -> Result<ConfigWrite, Release3Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Release3Error::InvalidParams("write must be an object".to_owned()))?;
    let target = parse_target(
        object
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| Release3Error::InvalidParams("write.target is required".to_owned()))?,
    )?;
    let expected_fingerprint = object
        .get("expectedFingerprint")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mutations = object
        .get("mutations")
        .and_then(Value::as_array)
        .ok_or_else(|| Release3Error::InvalidParams("write.mutations must be an array".to_owned()))?
        .iter()
        .map(|mutation| {
            let mutation = mutation.as_object().ok_or_else(|| {
                Release3Error::InvalidParams("mutation must be an object".to_owned())
            })?;
            let path = mutation
                .get("path")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Release3Error::InvalidParams("mutation.path must be an array".to_owned())
                })?
                .iter()
                .map(|part| {
                    part.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        Release3Error::InvalidParams(
                            "mutation path parts must be strings".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if mutation
                .get("remove")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Ok::<ConfigMutation, Release3Error>(ConfigMutation::remove(path))
            } else {
                let raw = mutation.get("value").cloned().ok_or_else(|| {
                    Release3Error::InvalidParams("mutation.value is required".to_owned())
                })?;
                let value = TomlValue::try_from(raw)
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
                Ok::<ConfigMutation, Release3Error>(ConfigMutation::set(path, value))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConfigWrite {
        target,
        expected_fingerprint,
        mutations,
    })
}

fn parse_target(value: &str) -> Result<ConfigTarget, Release3Error> {
    match value {
        "user" => Ok(ConfigTarget::User),
        "project" => Ok(ConfigTarget::Project),
        _ => Err(Release3Error::InvalidParams(
            "target must be user or project".to_owned(),
        )),
    }
}

/// The identity a stored message is addressed by on the wire.
///
/// A stored message carries no identifier of its own here, so the identity is
/// the one the reference falls back to when a message has none: its position in
/// the list and its role. Mirrors `history_message_id`
/// (`vibe/app_server/_projection.py:607`).
pub(crate) fn history_entry_id(index: usize, role: &str) -> String {
    format!("history:{index}:{role}")
}

/// Which stored message `entry_id` names, among the ones a rewind may target.
///
/// Only a user message is rewindable, which is what makes the position the
/// rewind cuts at the position of the message the operator is about to edit.
/// Mirrors `history_user_message_index`
/// (`vibe/app_server/_projection.py:611`).
fn rewind_entry_index(messages: &[ModelMessage], entry_id: &str) -> Result<usize, Release3Error> {
    messages
        .iter()
        .enumerate()
        .find(|(index, message)| {
            matches!(message, ModelMessage::User { .. })
                && history_entry_id(*index, "user") == entry_id
        })
        .map(|(index, _message)| index)
        .ok_or_else(|| {
            Release3Error::NotFound(format!("Rewindable history entry not found: {entry_id}"))
        })
}

fn hydrated_result(
    hydrated: &HydratedSession,
    attachment: Option<RuntimeAttachment>,
) -> Release3Dispatch {
    Release3Dispatch {
        result: [
            ("metadata".to_owned(), json!(hydrated.metadata)),
            ("messages".to_owned(), json!(hydrated.messages)),
            ("currentConfig".to_owned(), json!(hydrated.current_config)),
        ]
        .into_iter()
        .collect(),
        attachment,
    }
}

fn runtime_attachment(hydrated: &HydratedSession) -> RuntimeAttachment {
    let agent_profile: Option<AgentProfile> = hydrated
        .metadata
        .agent_profile
        .as_ref()
        .and_then(|profile| serde_json::from_value(profile.clone()).ok());
    RuntimeAttachment {
        id: hydrated.metadata.id.clone(),
        working_directory: hydrated.metadata.working_directory.clone(),
        parent_session_id: hydrated.metadata.parent_session_id.clone(),
        agent: agent_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .or_else(|| {
                hydrated
                    .metadata
                    .agent_profile
                    .as_ref()
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        agent_profile,
        hydrated: hydrated.clone(),
    }
}

fn config_map(value: Option<&Value>) -> Result<BTreeMap<String, Value>, Release3Error> {
    value
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Release3Error::Json)
        .map(Option::unwrap_or_default)
}

fn statistics_map(value: Option<&Value>) -> Result<BTreeMap<String, Value>, Release3Error> {
    config_map(value)
}

fn invalid_params(error: params::ParamError) -> Release3Error {
    Release3Error::InvalidParams(error.message())
}

fn required_string<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, Release3Error> {
    params::required_string(values, key).map_err(invalid_params)
}

fn optional_string(values: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    params::optional_string(values, key)
        .ok()
        .flatten()
        .map(ToOwned::to_owned)
}

fn usize_param(
    values: &BTreeMap<String, Value>,
    key: &str,
    default: usize,
) -> Result<usize, Release3Error> {
    params::usize_param(values, key, default, 0, usize::MAX).map_err(invalid_params)
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
mod tests {
    use tempfile::tempdir;
    use vibe_core::events::ModelMessage;
    use vibe_protocol::{Envelope, TransportKind, decode_frame};

    use super::*;
    use crate::server::AppServer;

    fn service() -> (tempfile::TempDir, Release3Service) {
        let temporary = tempdir().expect("tempdir");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let service = Release3Service::new(
            Release3Paths {
                vibe_home: temporary.path().join("home"),
                working_directory: workspace,
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("service");
        (temporary, service)
    }

    /// The Discovered layer reaches a client through the method it reads the
    /// configuration by, carrying the settings every declared tool publishes,
    /// and a file the operator owns still wins over them.
    #[test]
    fn config_read_publishes_the_discovered_tool_settings_a_file_can_override() {
        let (temporary, service) = service();
        let home = temporary.path().join("home");
        fs::create_dir_all(&home).expect("home");
        fs::write(
            home.join("config.toml"),
            "[tools.web_fetch]\nmax_timeout = 7\n",
        )
        .expect("user fixture");

        let snapshot = service.config_document().expect("config reads");

        let discovered = snapshot["layerValues"]
            .as_array()
            .expect("layer values")
            .iter()
            .find(|layer| layer["layer"] == "discovered")
            .expect("the discovered layer is published under its own name");
        assert_eq!(
            snapshot["layers"][1], "discovered",
            "the layer composes between the defaults and the selected file"
        );
        assert!(
            discovered["values"]["tools"]["web_fetch"]["max_content_bytes"].is_number(),
            "{discovered}"
        );
        assert_eq!(
            discovered["values"]["tools"]["web_fetch"]["max_timeout"],
            120
        );
        // The file overrides the one option it names; the rest of the
        // discovered entry survives the deep merge.
        assert_eq!(snapshot["config"]["tools"]["web_fetch"]["max_timeout"], 7);
        assert!(
            snapshot["config"]["tools"]["web_fetch"]["max_content_bytes"].is_number(),
            "{snapshot}"
        );
        assert_eq!(snapshot["validationWarnings"], json!([]));

        // The resolver reads the same document, so the handler that runs next
        // waits the seven seconds the file asked for.
        let settings: vibe_core::tools::config::WebFetchConfig =
            service.tool_config().view("web_fetch");
        assert_eq!(settings.max_timeout, 7);
    }

    /// US-107: a permanent approval writes the patterns it granted into
    /// `tools.<name>.allowlist`, which is the half of an approval that outlives
    /// the session because the tool reads that list back on the next one.
    #[tokio::test]
    async fn a_permanent_approval_reaches_the_configured_allowlist() {
        let (_temporary, service) = service();
        let store = vibe_core::policy::PermissionStore::default()
            .with_tool_config(service.tool_config())
            .with_allowlist_persistence(service.allowlist_persistence());

        store
            .authorize(
                "bash",
                json!({"command": "cargo test"}),
                vibe_core::policy::PermissionContext::asking(vec![
                    vibe_core::policy::PermissionRequirement::command("cargo test"),
                ]),
                &AlwaysPermanently,
            )
            .await
            .expect("the operator approves permanently");

        // The merge is against the list the tool actually reads, which already
        // carries the reference defaults the Discovered layer publishes, so the
        // approval adds one entry rather than replacing the list.
        let allowlist = |snapshot: &Value| {
            snapshot["config"]["tools"]["bash"]["allowlist"]
                .as_array()
                .expect("the allowlist is an array")
                .iter()
                .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        };
        let snapshot = service.config_document().expect("config reads");
        let after_first = allowlist(&snapshot);
        assert_eq!(
            after_first
                .iter()
                .filter(|entry| *entry == "cargo test *")
                .count(),
            1,
            "{after_first:?}"
        );
        assert!(after_first.contains(&"git status".to_owned()));
        assert!(store.diagnostics().is_empty(), "{:?}", store.diagnostics());

        // A second approval extends the same list rather than replacing it, and
        // the merge stays a sorted union.
        store
            .authorize(
                "bash",
                json!({"command": "cargo build --release"}),
                vibe_core::policy::PermissionContext::asking(vec![
                    vibe_core::policy::PermissionRequirement::command("cargo build"),
                ]),
                &AlwaysPermanently,
            )
            .await
            .expect("the operator approves permanently again");
        let snapshot = service.config_document().expect("config reads");
        let after_second = allowlist(&snapshot);
        assert!(after_second.contains(&"cargo test *".to_owned()));
        assert!(after_second.contains(&"cargo build *".to_owned()));
        assert_eq!(
            after_second.len(),
            after_first.len() + 1,
            "{after_second:?}"
        );
        let mut sorted = after_second.clone();
        sorted.sort();
        assert_eq!(after_second, sorted, "the merge writes a sorted union");
    }

    struct AlwaysPermanently;

    impl vibe_core::policy::ApprovalAgent for AlwaysPermanently {
        fn request<'a>(
            &'a self,
            _request: vibe_core::policy::ApprovalRequest,
        ) -> vibe_core::policy::ApprovalFuture<'a> {
            Box::pin(async move { Ok(vibe_core::policy::ApprovalDecision::ApprovePermanently) })
        }
    }

    #[test]
    fn public_config_methods_preserve_unknown_values_and_redact_proxy_secrets() {
        let (_temporary, service) = service();
        let result = service
            .dispatch(
                "config/batchWrite",
                &BTreeMap::from([(
                    "writes".to_owned(),
                    json!([{
                        "target": "user",
                        "expectedFingerprint": null,
                        "mutations": [
                            {"path": ["future", "flag"], "value": true},
                            {"path": ["proxy"], "value": "https://proxy.example"}
                        ]
                    }]),
                )]),
            )
            .expect("write");
        assert_eq!(
            result.result["snapshot"]["config"]["future"]["flag"],
            json!(true)
        );
        assert_eq!(
            result.result["snapshot"]["config"]["proxy"],
            json!("[redacted]")
        );
        assert_eq!(
            service
                .dispatch("config/proxy/read", &BTreeMap::new())
                .expect("proxy read")
                .result["settings"]["values"]["HTTP_PROXY"],
            Value::Null
        );
    }

    #[test]
    fn proxy_environment_round_trips_all_supported_keys_and_preserves_other_values() {
        let (temporary, service) = service();
        fs::create_dir_all(temporary.path().join("home")).expect("proxy home");
        fs::write(
            temporary.path().join("home/.env"),
            "MISTRAL_API_KEY='preserved'\nHTTP_PROXY='old'\n",
        )
        .expect("dotenv fixture");
        service
            .dispatch(
                "config/proxy/write",
                &BTreeMap::from([(
                    "changes".to_owned(),
                    json!({
                        "HTTP_PROXY": "https://proxy.example",
                        "HTTPS_PROXY": "https://secure-proxy.example",
                        "ALL_PROXY": "socks5://proxy.example",
                        "NO_PROXY": "localhost,.internal",
                        "SSL_CERT_FILE": "/certs/ca.pem",
                        "SSL_CERT_DIR": "/certs",
                    }),
                )]),
            )
            .expect("proxy write");
        let dispatch = service
            .dispatch("config/proxy/read", &BTreeMap::new())
            .expect("proxy read");
        assert_eq!(
            dispatch.result["settings"]["values"]["NO_PROXY"],
            "localhost,.internal"
        );
        let persisted =
            fs::read_to_string(temporary.path().join("home/.env")).expect("dotenv persisted");
        assert!(persisted.contains("MISTRAL_API_KEY='preserved'"));
        assert!(persisted.contains("SSL_CERT_DIR='/certs'"));
    }

    #[test]
    fn proxy_environment_rejects_unknown_keys_without_mutation() {
        let (temporary, service) = service();
        fs::create_dir_all(temporary.path().join("home")).expect("proxy home");
        fs::write(temporary.path().join("home/.env"), "HTTP_PROXY='old'\n")
            .expect("dotenv fixture");
        let error = service
            .dispatch(
                "config/proxy/write",
                &BTreeMap::from([("changes".to_owned(), json!({"BAD_PROXY": "value"}))]),
            )
            .expect_err("unknown key rejected");
        assert!(matches!(error, Release3Error::InvalidParams(_)));
        assert_eq!(
            fs::read_to_string(temporary.path().join("home/.env")).expect("unchanged"),
            "HTTP_PROXY='old'\n"
        );
    }

    #[test]
    fn configured_default_agent_is_resolved_from_the_live_config_snapshot() {
        let (_temporary, service) = service();
        service
            .dispatch(
                "config/batchWrite",
                &BTreeMap::from([(
                    "writes".to_owned(),
                    json!([{
                        "target": "user",
                        "expectedFingerprint": null,
                        "mutations": [{"path": ["default_agent"], "value": "plan"}]
                    }]),
                )]),
            )
            .expect("default agent config");

        assert_eq!(
            service.default_agent_name().expect("configured agent"),
            "plan"
        );
    }

    #[test]
    fn discovered_user_agent_remains_selectable_after_service_restart() {
        let temporary = tempdir().expect("tempdir");
        let workspace = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        let agents = vibe_home.join("extensions/agents");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&agents).expect("agent directory");
        fs::write(
            agents.join("reviewer.toml"),
            "display_name = \"Reviewer\"\ndescription = \"Custom reviewer\"\nagent_type = \"agent\"\n",
        )
        .expect("custom agent");

        let service = Release3Service::new(
            Release3Paths {
                session_root: temporary.path().join("sessions"),
                vibe_home,
                working_directory: workspace,
            },
            true,
        )
        .expect("restarted service");

        assert_eq!(
            service
                .agent_profile("reviewer")
                .expect("discovered profile")
                .display_name,
            "Reviewer"
        );
        fs::write(
            agents.join("reviewer.toml"),
            "display_name = \"Reloaded Reviewer\"\ndescription = \"Custom reviewer\"\nagent_type = \"agent\"\n",
        )
        .expect("updated custom agent");
        assert_eq!(
            service
                .agent_profile("reviewer")
                .expect("reloaded profile")
                .display_name,
            "Reloaded Reviewer"
        );
        assert!(
            service
                .dispatch("agents/list", &BTreeMap::new())
                .expect("agents list")
                .result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().any(|agent| agent["name"] == "reviewer"))
        );
    }

    fn patch(ops: Value) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("ops".to_owned(), ops),
            ("reason".to_owned(), json!("config screen edit")),
            ("reloadRuntime".to_owned(), json!(false)),
        ])
    }

    fn digest(path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    /// Reference `_config_patch`: a client addresses a field by pointer, and the
    /// server decides which file backs it.
    #[test]
    fn config_patch_writes_by_pointer_and_reports_the_keys_it_moved() {
        let (temporary, service) = service();
        let user = temporary.path().join("home/config.toml");

        let written = service
            .dispatch(
                "config/patch",
                &patch(json!([
                    {"op": "set", "path": "/theme", "value": "nord"},
                    {"op": "set", "path": "/tools/bash/allowlist", "value": ["git status"]},
                ])),
            )
            .expect("patch applies");

        assert_eq!(written.result["rejected"], json!(false));
        assert_eq!(written.result["failures"], json!([]));
        // The answer names what failed, not what moved: `ConfigPatchResponse`
        // declares no room for the changed keys, so the effect is read from the
        // configuration the patch produced.
        assert!(!written.result.contains_key("changedKeys"));
        let document = service.config_document().expect("config read");
        assert_eq!(document["config"]["theme"], json!("nord"));
        assert_eq!(
            document["config"]["tools"]["bash"]["allowlist"],
            json!(["git status"]),
            "the intermediate tables the leaf needs were created"
        );
        assert!(
            fs::read_to_string(&user)
                .expect("the user file was written")
                .contains("nord")
        );

        let removed = service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "remove", "path": "/theme"}])),
            )
            .expect("removal applies");
        assert_eq!(removed.result["rejected"], json!(false));
        assert_eq!(
            service.config_document().expect("config read")["config"]["theme"],
            json!("auto"),
            "removing the override falls back to the shipped default"
        );

        // A table that already exists is diffed down to the leaf that moved.
        let deepened = service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/tools/bash/allowlist", "value": ["git status", "ls"]}])),
            )
            .expect("deep set applies");
        assert_eq!(deepened.result["failures"], json!([]));
        assert_eq!(
            service.config_document().expect("config read")["config"]["tools"]["bash"]["allowlist"],
            json!(["git status", "ls"])
        );

        // A patch that writes the value already in place is answered the same
        // way rather than being refused.
        let repeated = service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/tools/bash/allowlist", "value": ["git status", "ls"]}])),
            )
            .expect("repeat applies");
        assert_eq!(repeated.result["rejected"], json!(false));
        assert_eq!(repeated.result["failures"], json!([]));
    }

    /// The preflight runs against the merged configuration and refuses the whole
    /// request, so nothing reaches disk. Reference `ConfigPatchValidationError`.
    #[test]
    fn a_rejected_patch_leaves_every_configuration_file_byte_identical() {
        let (temporary, service) = service();
        let user = temporary.path().join("home/config.toml");
        service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/theme", "value": "nord"}])),
            )
            .expect("seed applies");
        let before = digest(&user).expect("the user file exists");

        for ops in [
            // Leaves no configured model behind.
            json!([{"op": "set", "path": "/models", "value": {}}]),
            // Traverses a scalar the merged document already carries.
            json!([{"op": "set", "path": "/theme/nested", "value": true}]),
            // Names a list position that does not exist.
            json!([{"op": "remove", "path": "/providers/9"}]),
        ] {
            let rejected = service
                .dispatch("config/patch", &patch(ops.clone()))
                .expect("the request is answered rather than raised");
            assert_eq!(rejected.result["rejected"], json!(true), "{ops}");
            assert_eq!(rejected.result["failures"], json!([]));
            assert_eq!(
                digest(&user),
                Some(before.clone()),
                "{ops} touched the file"
            );
        }
        assert_eq!(
            service.config_document().expect("config read")["config"]["theme"],
            json!("nord")
        );
    }

    /// The reference applies each layer on its own once the preflight passes, so
    /// one file failing is reported rather than undoing the file that worked.
    #[test]
    fn a_write_that_cannot_land_is_reported_per_target_beside_one_that_did() {
        let (temporary, service) = service();
        let project = temporary.path().join("workspace/.vibe");
        fs::create_dir_all(&project).expect("project directory");
        fs::write(project.join("config.toml"), "theme = \"nord\"\n").expect("project fixture");

        let outcome = service
            .dispatch(
                "config/patch",
                &patch(json!([
                    {"op": "set", "path": "/theme", "value": "dracula", "targetLayer": "project"},
                    // `displayed_workdir` only exists in the defaults layer, so
                    // the merged preflight resolves it and the user file cannot.
                    {"op": "remove", "path": "/displayed_workdir", "targetLayer": "user"},
                ])),
            )
            .expect("the patch is applied per target");

        assert_eq!(outcome.result["rejected"], json!(false));
        let failures = outcome.result["failures"]
            .as_array()
            .expect("failures are reported as a list");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0]
                .as_str()
                .is_some_and(|failure| failure.contains("/displayed_workdir")),
            "{failures:?}"
        );
        assert_eq!(
            service.config_document().expect("config read")["config"]["theme"],
            json!("dracula"),
            "the write that succeeded stands"
        );

        // An operation naming no target goes to the file the selection resolves
        // to, which is the trusted project file now that one exists.
        service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/default_agent", "value": "plan"}])),
            )
            .expect("the unrouted patch applies");
        assert!(
            fs::read_to_string(project.join("config.toml"))
                .expect("the project file survives")
                .contains("plan")
        );
        assert!(
            digest(&temporary.path().join("home/config.toml")).is_none(),
            "an unrouted operation reached the user file"
        );
    }

    #[test]
    fn a_patch_aimed_at_an_untrusted_project_changes_nothing() {
        let temporary = tempdir().expect("tempdir");
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(workspace.join(".vibe")).expect("project directory");
        let service = Release3Service::new(
            Release3Paths {
                vibe_home: temporary.path().join("home"),
                working_directory: workspace.clone(),
                session_root: temporary.path().join("sessions"),
            },
            false,
        )
        .expect("untrusted service");

        let error = service
            .dispatch(
                "config/patch",
                &patch(json!([
                    {"op": "set", "path": "/theme", "value": "nord", "targetLayer": "project"},
                    {"op": "set", "path": "/default_agent", "value": "plan"},
                ])),
            )
            .expect_err("an untrusted project is refused");

        assert!(
            matches!(&error, Release3Error::Config(message) if message.contains("trust")),
            "{error}"
        );
        assert!(
            digest(&workspace.join(".vibe/config.toml")).is_none(),
            "the project file was created despite the refusal"
        );
        assert!(
            digest(&temporary.path().join("home/config.toml")).is_none(),
            "the user half of the patch was written despite the refusal"
        );
    }

    #[test]
    fn config_patch_rejects_a_malformed_operation_before_it_reaches_the_store() {
        let (_temporary, service) = service();
        for ops in [
            json!([{"op": "toggle", "path": "/theme", "value": "nord"}]),
            json!([{"op": "set", "path": "theme", "value": "nord"}]),
            json!([{"op": "set", "value": "nord"}]),
            json!([{"op": "set", "path": "/theme", "value": "nord", "targetLayer": "global"}]),
        ] {
            let error = service
                .dispatch("config/patch", &patch(ops.clone()))
                .expect_err("the operation is refused");
            assert!(matches!(error, Release3Error::InvalidParams(_)), "{ops}");
        }
        assert!(
            service
                .dispatch("config/patch", &BTreeMap::new())
                .is_err_and(|error| matches!(error, Release3Error::InvalidParams(_)))
        );
    }

    /// Reference `_config_fields_read`: the settings screen renders from this
    /// answer alone.
    #[test]
    fn config_fields_read_describes_the_published_surface_and_its_targets() {
        let (_temporary, service) = service();
        service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/theme", "value": "nord"}])),
            )
            .expect("seed applies");

        let response = service
            .dispatch("config/fields/read", &BTreeMap::new())
            .expect("fields read");
        let fields = response.result["fields"]
            .as_array()
            .expect("the response carries a field list");
        assert_eq!(response.result["targets"], json!(["user", "project"]));
        assert!(
            fields.iter().all(|field| field["name"] != json!("tools")),
            "per-tool settings have no editor on either side"
        );
        assert_eq!(
            fields.len(),
            vibe_core::config::registry::FIELDS
                .iter()
                .filter(|spec| spec.published && spec.name != "tools")
                .count()
        );

        let theme = fields
            .iter()
            .find(|field| field["name"] == json!("theme"))
            .expect("theme is described");
        assert_eq!(theme["kind"], json!("enum"));
        assert_eq!(theme["path"], json!("/theme"));
        assert_eq!(theme["value"], json!("nord"));
        assert_eq!(theme["popular"], json!(true));
        assert!(
            theme["enumChoices"]
                .as_array()
                .is_some_and(|choices| choices.contains(&json!("nord")))
        );
        assert!(
            theme["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert_eq!(
            theme["layerValues"],
            json!([
                {"layer": "selected_toml", "value": "nord"},
                {"layer": "defaults", "value": "auto"},
            ]),
            "layer values run from the highest priority down to the defaults"
        );
        assert_eq!(
            fields
                .iter()
                .filter(|field| field["popular"] == json!(true))
                .count(),
            12,
            "the popular set is the reference one"
        );
    }

    /// Reference `ConfigSchemaReadResponse`: a version token beside the schema
    /// object, so a client can cache the surface it names.
    #[test]
    fn config_schema_publishes_every_declared_field_with_a_version_token() {
        let (_temporary, service) = service();
        let response = service
            .dispatch("config/schema", &BTreeMap::new())
            .expect("config schema");

        let version = response.result["configSchemaVersion"]
            .as_str()
            .expect("the response carries a version token");
        assert!(version.starts_with("sha256:"), "{version}");
        let properties = response.result["schema"]["properties"]
            .as_object()
            .expect("the schema declares properties");
        assert_eq!(properties.len(), vibe_core::config::registry::FIELDS.len());
        for field in vibe_core::config::registry::FIELDS {
            assert!(
                properties.contains_key(field.name),
                "`{}` is not published",
                field.name
            );
        }
        // A settings screen renders these directly, so their shape is asserted
        // rather than assumed.
        assert_eq!(
            properties["auto_compact_threshold"]["type"],
            json!("integer")
        );
        assert_eq!(
            properties["auto_compact_threshold"]["default"],
            json!(200_000)
        );
        assert_eq!(properties["api_timeout"]["type"], json!("number"));
        assert_eq!(
            properties["otel_redaction"]["enum"],
            json!(["default", "none", "strict"])
        );

        let again = service
            .dispatch("config/schema", &BTreeMap::new())
            .expect("config schema");
        assert_eq!(again.result, response.result, "the schema is not cacheable");
    }

    /// The Defaults layer is the shipped document at every construction site,
    /// so a session opened without a configuration file still reads the
    /// reference defaults.
    #[test]
    fn config_read_composes_the_shipped_defaults_without_a_configuration_file() {
        let (_temporary, service) = service();
        let snapshot = service.config_document().expect("config read");
        let config = &snapshot["config"];

        assert_eq!(config["active_model"], json!("mistral-medium-3.5"));
        assert_eq!(config["theme"], json!("auto"));
        assert_eq!(config["auto_compact_threshold"], json!(200_000));
        assert_eq!(
            config["models"]["local"]["provider"],
            json!("llamacpp"),
            "models are read back keyed by alias"
        );
        assert_eq!(
            snapshot["validationWarnings"],
            json!([]),
            "the shipped defaults need no repair"
        );
        let layers = snapshot["layerValues"]
            .as_array()
            .expect("the snapshot lists its layers");
        let defaults = layers
            .iter()
            .find(|layer| layer["layer"] == json!("defaults"))
            .expect("the defaults layer is composed");
        assert!(
            defaults["values"]
                .as_object()
                .is_some_and(|values| values.len() > 50),
            "the defaults layer is empty"
        );
    }

    #[test]
    fn builtin_lean_agent_installation_is_persisted_and_reflected_in_agent_listing() {
        let (_temporary, service) = service();
        let before = service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("agents list");
        assert!(
            before.result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().all(|agent| agent["name"] != "lean"))
        );

        service
            .dispatch(
                "agents/install",
                &BTreeMap::from([("agentName".to_owned(), json!("lean"))]),
            )
            .expect("lean install");
        let installed = service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("installed agents list");
        assert!(
            installed.result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().any(|agent| agent["name"] == "lean"))
        );
        let config = service.config_document().expect("config after install");
        assert_eq!(config["config"]["installed_agents"], json!(["lean"]));

        service
            .dispatch(
                "agents/uninstall",
                &BTreeMap::from([("agentName".to_owned(), json!("lean"))]),
            )
            .expect("lean uninstall");
        let after = service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("agents after uninstall");
        assert!(
            after.result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().all(|agent| agent["name"] != "lean"))
        );
    }

    #[test]
    fn switching_away_from_auto_approve_removes_its_permission_override() {
        let (_temporary, service) = service();
        let session_id = "agent-switch";
        let working_directory = service
            .paths
            .working_directory
            .to_string_lossy()
            .into_owned();
        service
            .create_runtime_session(session_id, &working_directory, 1)
            .expect("session");
        let mut metadata = service.store.load(session_id).expect("metadata").metadata;
        metadata
            .config
            .insert("active_model".to_owned(), json!("base-model"));
        service
            .store
            .update_metadata(&metadata)
            .expect("base session config");

        for name in ["auto-approve", "default"] {
            let update = service
                .dispatch(
                    "session/agent/update",
                    &BTreeMap::from([
                        ("sessionId".to_owned(), json!(session_id)),
                        ("name".to_owned(), json!(name)),
                    ]),
                )
                .expect("agent update");
            assert_eq!(
                update
                    .attachment
                    .as_ref()
                    .and_then(|attachment| attachment.agent.as_deref()),
                Some(name)
            );
        }

        let metadata = service.store.load(session_id).expect("metadata").metadata;
        assert_eq!(
            metadata
                .agent_profile
                .as_ref()
                .and_then(|profile| profile.get("name")),
            Some(&json!("default"))
        );
        assert!(
            !metadata.config.contains_key("bypass_tool_permissions"),
            "the prior auto-approve override must not survive"
        );
        assert_eq!(
            metadata.config.get("active_model"),
            Some(&json!("base-model")),
            "agent overlays must not destroy the underlying session config"
        );
    }

    #[test]
    fn rewind_resolves_an_entry_identity_and_forks_before_the_selected_message() {
        let (_temporary, service) = service();
        let session_id = "rewind-source";
        let working_directory = service
            .paths
            .working_directory
            .to_string_lossy()
            .into_owned();
        let mut hydrated = service
            .create_runtime_session(session_id, &working_directory, 1)
            .expect("session");
        for (offset, message) in [
            ModelMessage::user("first question".to_owned()),
            ModelMessage::Assistant {
                content: "first answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
            ModelMessage::user("edit this question".to_owned()),
            ModelMessage::Assistant {
                content: "second answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
        ]
        .iter()
        .enumerate()
        {
            service
                .store
                .append_message(
                    &mut hydrated.metadata,
                    message,
                    u64::try_from(offset).unwrap_or_default().saturating_add(2),
                )
                .expect("append message");
        }

        // A rewindable point is addressed by the identity a stored user
        // message carries, and this service answers the two fields only the
        // transcript decides; the paths come from the session's engine.
        let preview = service
            .dispatch(
                "session/rewind/read",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!(session_id)),
                    ("entryId".to_owned(), json!("history:2:user")),
                ]),
            )
            .expect("rewind preview");
        assert_eq!(preview.result["hasFileChanges"], json!(false));
        assert_eq!(preview.result["paths"], json!([]));
        assert!(
            matches!(
                service.dispatch(
                    "session/rewind/read",
                    &BTreeMap::from([
                        ("sessionId".to_owned(), json!(session_id)),
                        ("entryId".to_owned(), json!("history:1:user")),
                    ]),
                ),
                Err(Release3Error::NotFound(_))
            ),
            "an assistant message is not a rewindable entry"
        );

        let rewind = service
            .dispatch(
                "session/rewind",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!(session_id)),
                    ("entryId".to_owned(), json!("history:2:user")),
                    ("restoreFiles".to_owned(), json!(false)),
                    ("statistics".to_owned(), json!({"tokens": 17})),
                ]),
            )
            .expect("rewind");
        let child = rewind.attachment.expect("child attachment");
        assert_eq!(child.parent_session_id.as_deref(), Some(session_id));
        assert_eq!(rewind.result["message"], json!("edit this question"));
        assert_eq!(child.hydrated.messages.len(), 2);
        assert_eq!(child.hydrated.metadata.statistics["tokens"], 17);
        assert_eq!(
            service
                .store
                .load(session_id)
                .expect("source remains")
                .messages
                .len(),
            4
        );
    }

    #[test]
    fn caller_paths_cannot_expand_server_authorized_roots() {
        let (temporary, service) = service();
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory");
        std::fs::write(
            outside.join("agent.toml"),
            "display_name = \"Outside\"\nagent_type = \"agent\"\n",
        )
        .expect("outside agent");
        let install = service.dispatch(
            "agents/install",
            &BTreeMap::from([("path".to_owned(), json!(outside.join("agent.toml")))]),
        );
        assert!(matches!(install, Err(Release3Error::InvalidParams(_))));
        let prompt = service.dispatch(
            "workspace/prompt/prepare",
            &BTreeMap::from([
                ("base".to_owned(), json!("base")),
                ("addDirectories".to_owned(), json!([outside])),
            ]),
        );
        assert!(matches!(prompt, Err(Release3Error::InvalidParams(_))));
    }

    #[test]
    fn project_mcp_stdio_config_activates_only_after_workspace_trust() {
        let (temporary, service) = service();
        fs::create_dir_all(temporary.path().join("workspace/.vibe")).expect("project config root");
        fs::write(
            temporary.path().join("workspace/.vibe/config.toml"),
            r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/usr/bin/fixture"
args = ["--stdio"]
env = { MODE = "test" }
cwd = "."
startup_timeout_sec = 1
tool_timeout_sec = 2
"#,
        )
        .expect("project MCP config");

        assert!(
            service
                .mcp_servers_for_session(&temporary.path().join("workspace"), false, &[])
                .expect("untrusted user fallback")
                .is_empty()
        );
        let trusted = service
            .mcp_servers_for_session(&temporary.path().join("workspace"), true, &[])
            .expect("trusted project MCP config");
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].alias, "fixture");
        assert_eq!(trusted[0].startup_timeout_ms, 1_000);
        assert_eq!(trusted[0].tool_timeout_ms, 2_000);

        let runtime = json!({
            "name": "runtime",
            "transport": "stdio",
            "command": "/must-not-run"
        });
        assert!(matches!(
            service.mcp_servers_for_session(
                &temporary.path().join("workspace"),
                false,
                &[runtime]
            ),
            Err(Release3Error::InvalidParams(message))
                if message.contains("trusted workspace")
        ));
    }

    #[test]
    fn saved_session_methods_create_independent_runtime_attachments() {
        let (_temporary, service) = service();
        let mut metadata = service
            .store
            .create("parent", "/workspace", None, 1)
            .expect("create");
        service
            .store
            .append_message(&mut metadata, &ModelMessage::user("hello".to_owned()), 2)
            .expect("append");
        let fork = service
            .dispatch(
                "session/fork",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!("parent")),
                    ("newSessionId".to_owned(), json!("child")),
                    ("systemPrompt".to_owned(), json!("fresh")),
                    ("config".to_owned(), json!({"model": "child"})),
                ]),
            )
            .expect("fork");
        assert_eq!(
            fork.attachment.expect("attachment").parent_session_id,
            Some("parent".to_owned())
        );
        assert_eq!(fork.result["currentConfig"]["model"], "child");
        assert_eq!(
            service
                .dispatch(
                    "session/list",
                    &BTreeMap::from([("limit".to_owned(), json!(50))]),
                )
                .expect("list")
                .result["sessions"]
                .as_array()
                .expect("array")
                .len(),
            2
        );
    }

    /// US-072: the global dotenv file stands in for a `VIBE_*` variable the
    /// process does not export, all the way through to the composed
    /// configuration.
    #[test]
    fn the_global_dotenv_file_feeds_the_environment_layer() {
        let temporary = tempdir().expect("tempdir");
        let vibe_home = temporary.path().join("home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&vibe_home).expect("vibe home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(
            vibe_home.join(".env"),
            "VIBE_THEME=nord\nMISTRAL_API_KEY=secret\n",
        )
        .expect("dotenv fixture");

        let service = Release3Service::new(
            Release3Paths {
                vibe_home: vibe_home.clone(),
                working_directory: workspace,
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("service");
        let snapshot = service.config_document().expect("configuration reads");

        assert_eq!(snapshot["config"]["theme"], json!("nord"));
        // The credential in the same file is not a configuration field and
        // never reaches the published document.
        assert!(
            !snapshot.to_string().contains("secret"),
            "no dotenv secret reaches the configuration surface"
        );
    }

    /// US-073: the startup step brings an older file forward; constructing the
    /// service does not.
    #[test]
    fn the_startup_migration_rewrites_the_user_file() {
        let temporary = tempdir().expect("tempdir");
        let vibe_home = temporary.path().join("home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&vibe_home).expect("vibe home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let user_path = vibe_home.join("config.toml");
        std::fs::write(&user_path, "disabled_tools = [\"search_replace\"]\n")
            .expect("user fixture");

        let service = Release3Service::new(
            Release3Paths {
                vibe_home,
                working_directory: workspace,
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("service");
        assert_eq!(
            std::fs::read_to_string(&user_path).expect("user file"),
            "disabled_tools = [\"search_replace\"]\n",
            "building the service leaves the file alone"
        );

        assert!(
            service
                .migrate_configuration()
                .expect("migrations run")
                .is_empty()
        );

        assert!(
            std::fs::read_to_string(&user_path)
                .expect("user file")
                .contains("edit")
        );
        let snapshot = service.config_document().expect("configuration reads");
        assert_eq!(snapshot["config"]["disabled_tools"], json!(["edit"]));
    }

    /// US-071: an added directory is a project root of its own, so its
    /// `.vibe/hooks.toml` and the rest of its extensions are read, ahead of the
    /// user-level file.
    #[test]
    fn an_added_directory_contributes_its_own_extension_root() {
        let temporary = tempdir().expect("tempdir");
        let workspace = temporary.path().join("workspace");
        let added = temporary.path().join("library");
        let vibe_home = temporary.path().join("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(added.join(".vibe/commands")).expect("added extensions");
        std::fs::create_dir_all(vibe_home.join("extensions")).expect("user extensions");
        std::fs::write(
            added.join(".vibe/commands/release.md"),
            "Cut a release build.\n",
        )
        .expect("command fixture");
        let hook = "[[hooks]]\nname = \"%NAME%\"\ntype = \"pre_tool\"\nprogram = \"echo\"\n";
        std::fs::write(
            added.join(".vibe/hooks.toml"),
            hook.replace("%NAME%", "project-hook"),
        )
        .expect("project hook fixture");
        std::fs::write(
            vibe_home.join("extensions/hooks.toml"),
            hook.replace("%NAME%", "user-hook"),
        )
        .expect("user hook fixture");

        let service = Release3Service::new(
            Release3Paths {
                vibe_home,
                working_directory: workspace.clone(),
                session_root: temporary.path().join("sessions"),
            },
            true,
        )
        .expect("service")
        .with_allowed_roots(vec![added.clone(), added.clone(), workspace]);

        assert_eq!(
            service.discovery_roots.project,
            vec![
                service.paths.working_directory.join(".vibe"),
                added.join(".vibe")
            ],
            "the working directory leads, the added directory follows once"
        );
        let catalog = service.catalog();
        assert!(
            catalog.commands.contains_key("release"),
            "the added directory's commands are discovered"
        );
        assert_eq!(
            catalog
                .hooks
                .iter()
                .map(|hook| hook.name.as_str())
                .collect::<Vec<_>>(),
            vec!["project-hook", "user-hook"],
            "each open root's hook file is read, then the user-level one"
        );
    }

    #[test]
    fn app_server_advertises_and_dispatches_release3_resources() {
        let (_temporary, service) = service();
        service
            .store
            .create("saved", "/workspace", None, 1)
            .expect("saved session");
        let server = AppServer::with_release3_service(service);
        let mut connection = server.connect(TransportKind::InProcess);
        let initialized = connection.dispatch(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1"},"capabilities":{"callbackKinds":[]}}}"#,
        );
        let response = decode_frame(&initialized.outbound[0]).expect("initialize response");
        assert!(matches!(response, Envelope::Success(_)));
        let Envelope::Success(response) = response else {
            return;
        };
        assert!(
            response.result["capabilities"]["methods"]
                .as_array()
                .expect("methods")
                .contains(&json!("config/schema"))
        );
        connection.dispatch(br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        let resume = connection.dispatch(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"saved","systemPrompt":"fresh","config":{}}}"#,
        );
        assert!(matches!(
            decode_frame(&resume.outbound[0]).expect("resume response"),
            Envelope::Success(_)
        ));
        assert_eq!(
            server.session("saved").expect("attached runtime").id,
            "saved"
        );
    }
}
